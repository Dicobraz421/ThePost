//! Транспорт запросов узел → узел: HTTPS с mTLS внутреннего канала (CLU-12; SRS 2.8, CONTRACTS.md#Rpc).
//!
//! [`NodeRpc`] — реализация [`Rpc`] узла:
//! - клиент (reqwest) идёт по маршруту [`route`]: недоступный узел (не установлено соединение, не прошла проверка
//!   TLS, сертификат назвал другой узел) — следующий; срок вызова — [`RpcError::Timeout`]; узел маршрута — этот
//!   же узел — вызов функции без сети;
//! - сервер (axum, [`NodeRpc::listen`]) принимает только соединения с клиентским сертификатом внутреннего УЦ;
//!   идентичность вызывающего узла — по SAN сертификата ([`node_identity`], [`NodeIdentity`]), его роли — из
//!   состава кластера; дальше матрица разрешений ([`Permissions`]), обработчик службы ([`Rpc::serve`]), предел
//!   параллельности ([`Admission`]) и остаток срока вызова.
//!
//! Идентичность узла по сертификату (CLU-14): URI `thepost:node:<id>` в SAN; без него — имена DNS и адреса IP
//! из SAN, узел — тот, чей хост в `NodeInfo::addr` среди них (без масок `*.`); CN — только у сертификата без
//! таких SAN (выпуск до CLU-14). Сертификаты перечитываются без перезапуска ([`NodeRpc::reload_tls`],
//! [`NodeRpc::follow_tls`]; ключ из хранилища секретов — [`NodeTls::load_with_secrets`],
//! [`NodeRpc::follow_tls_with_secrets`], CLU-16): новые соединения — с новыми, установленные дорабатывают на прежних.
//!
//! Протокол: `POST /rpc/<роль>/<служба>`, тело — длина ключа (u32, big-endian), ключ, тело запроса; остаток срока —
//! заголовок `x-thepost-rpc-deadline-ms`. Ответ — заголовок `x-thepost-rpc` (`ok` или вид ошибки), тело — ответ
//! обработчика или текст ошибки (`invalid`, `internal`).
//!
//! ALPN — только `http/1.1`: у axum и reqwest в workspace не включена фича `http2`; после её включения в ALPN
//! добавляется `h2` ([`ALPN`]). Объявить `h2` раньше нельзя: сервер без фичи не поймёт клиента, выбравшего `h2`.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::serve::{IncomingStream, Listener};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use futures::{Stream, StreamExt};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tp_contracts::membership::{Membership, NodeId, NodeInfo};
use tp_contracts::role::Role;
use tp_contracts::rpc::{Rpc, RpcError, RpcHandler, RpcRequest, RpcService};
use tp_contracts::secrets::SecretStore;
use tp_contracts::tls::{InternalTls, KeySource};
use tp_contracts::{Error, Result};
use tracing::{debug, info, warn};
use x509_parser::extensions::GeneralName;

use crate::rpc::{Admission, Permissions, RpcConfig, route};

/// Протоколы ALPN внутреннего канала (сервер и клиент).
pub const ALPN: &[&[u8]] = &[b"http/1.1"];

/// Заголовок ответа: `ok` или вид ошибки.
const H_STATUS: &str = "x-thepost-rpc";
/// Заголовок запроса: остаток срока вызова, мс.
const H_DEADLINE: &str = "x-thepost-rpc-deadline-ms";
/// Заголовок ответа `unavailable`: рекомендуемая пауза перед повтором, мс.
const H_RETRY_AFTER: &str = "x-thepost-rpc-retry-after-ms";

type StdResult<T, E> = std::result::Result<T, E>;

/// Префикс URI узла в SAN сертификата: `thepost:node:<id>`.
pub const NODE_URI_PREFIX: &str = "thepost:node:";

/// Кого называет сертификат узла ([`node_identity`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeIdentity {
    /// Узел по URI `thepost:node:<id>` в SAN или, у сертификата без SAN узла, по CN.
    Node(NodeId),
    /// Имена DNS (нижний регистр, без завершающей точки) и адреса IP из SAN: узел — тот, чей хост в
    /// `NodeInfo::addr` среди них.
    Hosts(Vec<String>),
}

impl NodeIdentity {
    /// Называет ли сертификат узел `node`: по идентификатору или по хосту его адреса.
    pub fn names(&self, node: &NodeInfo) -> bool {
        match self {
            Self::Node(id) => *id == node.id,
            Self::Hosts(hosts) => addr_host(&node.addr).is_some_and(|h| hosts.contains(&h)),
        }
    }

    /// Узел, которого называет сертификат, в составе `live`: по идентификатору — он сам; по хостам — единственный
    /// узел состава с таким хостом (ни одного или несколько — `None`).
    pub fn resolve(&self, live: &[NodeInfo]) -> Option<NodeId> {
        match self {
            Self::Node(id) => Some(id.clone()),
            Self::Hosts(_) => {
                let mut found = live.iter().filter(|n| self.names(n));
                match (found.next(), found.next()) {
                    (Some(n), None) => Some(n.id.clone()),
                    _ => None,
                }
            }
        }
    }
}

/// Хост адреса узла (`https://<хост>:<порт>`) в записи [`NodeIdentity::Hosts`].
fn addr_host(addr: &str) -> Option<String> {
    let url = reqwest::Url::parse(addr).ok()?;
    let host = url.host_str()?;
    let bare = host.trim_start_matches('[').trim_end_matches(']');
    Some(match bare.parse::<IpAddr>() {
        Ok(ip) => ip.to_string(),
        Err(_) => dns_name(host),
    })
}

fn dns_name(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// Идентичность узла по его сертификату: единственный URI `thepost:node:<id>` в SAN; без него — имена DNS и
/// адреса IP из SAN; без них — единственный CN субъекта. Иначе — `Invalid`.
pub fn node_identity(cert: &CertificateDer<'_>) -> Result<NodeIdentity> {
    let (_, x509) = x509_parser::parse_x509_certificate(cert.as_ref())
        .map_err(|e| Error::Invalid(format!("node certificate: {e}")))?;
    let san = x509
        .subject_alternative_name()
        .map_err(|e| Error::Invalid(format!("node certificate SAN: {e}")))?;
    let (mut ids, mut hosts) = (Vec::new(), Vec::new());
    for name in san.iter().flat_map(|s| s.value.general_names.iter()) {
        match name {
            GeneralName::URI(uri) => {
                let id = uri
                    .get(..NODE_URI_PREFIX.len())
                    .filter(|p| p.eq_ignore_ascii_case(NODE_URI_PREFIX))
                    .map(|_| &uri[NODE_URI_PREFIX.len()..]);
                if let Some(id) = id
                    && !ids.contains(&id)
                {
                    ids.push(id);
                }
            }
            GeneralName::DNSName(dns) => hosts.push(dns_name(dns)),
            GeneralName::IPAddress(ip) => {
                if let Ok(v4) = <[u8; 4]>::try_from(*ip) {
                    hosts.push(IpAddr::from(v4).to_string());
                } else if let Ok(v6) = <[u8; 16]>::try_from(*ip) {
                    hosts.push(IpAddr::from(v6).to_string());
                }
            }
            _ => {}
        }
    }
    match ids.as_slice() {
        [id] => return NodeId::new(*id).map(NodeIdentity::Node),
        [] => {}
        _ => {
            return Err(Error::invalid(
                "node certificate: SAN names several nodes (thepost:node:<id>)",
            ));
        }
    }
    if !hosts.is_empty() {
        hosts.sort();
        hosts.dedup();
        return Ok(NodeIdentity::Hosts(hosts));
    }
    let mut names = x509.subject().iter_common_name();
    let (Some(cn), None) = (names.next(), names.next()) else {
        return Err(Error::invalid(
            "node certificate: no thepost:node URI, DNS or IP in SAN and no single CN (node id)",
        ));
    };
    let cn = cn
        .as_str()
        .map_err(|e| Error::Invalid(format!("node certificate CN: {e}")))?;
    NodeId::new(cn).map(NodeIdentity::Node)
}

/// Сертификат `tls` годится узлу `id`: по идентификатору — только `id`; по хостам — проверяется по адресу в
/// составе кластера при каждом соединении.
fn check_identity(id: &NodeId, tls: &NodeIdentity) -> Result<()> {
    match tls {
        NodeIdentity::Node(cert) if cert != id => Err(Error::Invalid(format!(
            "rpc.tls.cert_file names node {:?}, this node is {:?}",
            cert.as_str(),
            id.as_str()
        ))),
        _ => Ok(()),
    }
}

/// Клиент reqwest с TLS `tls`.
fn build_client(config: &RpcConfig, tls: ClientConfig) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .tls_backend_preconfigured(tls)
        .tls_info(true)
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(config.connect_timeout)
        .build()
        .map_err(|e| Error::Internal(format!("rpc client: {e}")))
}

/// TLS внутреннего канала узла: сервер требует клиентский сертификат внутреннего УЦ, клиент предъявляет свой
/// и проверяет сервер по тому же УЦ и имени хоста из адреса узла.
#[derive(Clone)]
pub struct NodeTls {
    identity: NodeIdentity,
    server: Arc<ServerConfig>,
    client: ClientConfig,
}

impl std::fmt::Debug for NodeTls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeTls")
            .field("identity", &self.identity)
            .finish_non_exhaustive()
    }
}

impl NodeTls {
    /// Читает PEM-файлы секции `rpc.tls`: `ca_file`, `cert_file` и `key_file` обязательны. Ключ по ссылке на
    /// секрет (`key`) требует хранилища — [`NodeTls::load_with_secrets`], здесь — `Invalid`.
    pub async fn load(tls: &InternalTls) -> Result<Self> {
        Self::load_from(tls, None).await
    }

    /// Как [`NodeTls::load`], с хранилищем секретов `store` для ключа по ссылке `key`: значение секрета — PEM,
    /// на диск не пишется. Обязательны `ca_file`, `cert_file` и `key_file` либо `key` (оба сразу — `Invalid`).
    /// Секция с ключом в файле хранилище не читает. Хранилище недоступно — его ошибка (повторяемая).
    pub async fn load_with_secrets(tls: &InternalTls, store: &dyn SecretStore) -> Result<Self> {
        Self::load_from(tls, Some(store)).await
    }

    async fn load_from(tls: &InternalTls, store: Option<&dyn SecretStore>) -> Result<Self> {
        let need = |path: &Option<PathBuf>, field: &str| {
            path.clone().ok_or_else(|| {
                Error::Invalid(format!("rpc.tls.{field} is required (node-to-node mTLS)"))
            })
        };
        let source = tls.key_source().map_err(|e| match e {
            Error::Invalid(m) => Error::Invalid(format!("rpc.{m}")),
            other => other,
        })?;
        let ca = read_pem(need(&tls.ca_file, "ca_file")?).await?;
        let cert = read_pem(need(&tls.cert_file, "cert_file")?).await?;
        match source {
            Some(KeySource::File(path)) => {
                let key = read_pem(path.to_path_buf()).await?;
                Self::from_pem(&ca, &cert, &key)
            }
            Some(KeySource::Secret(secret)) => {
                let store = store.ok_or_else(|| {
                    Error::invalid("rpc.tls.key requires a secret store (secrets section)")
                })?;
                // Значение секрета в текст ошибки не попадает: только ссылка и вид отказа.
                let key = store.get(secret).await.map_err(|e| {
                    if e.is_retryable() {
                        e
                    } else {
                        Error::Invalid(format!("rpc.tls.key: secret {secret}: {e}"))
                    }
                })?;
                Self::from_pem(&ca, &cert, key.expose().as_bytes())
            }
            None => Err(Error::invalid(
                "rpc.tls.key_file or rpc.tls.key is required (node-to-node mTLS)",
            )),
        }
    }

    /// Из PEM: корневые сертификаты внутреннего УЦ, цепочка сертификата узла (первый — узла) и его ключ.
    pub fn from_pem(ca: &[u8], cert: &[u8], key: &[u8]) -> Result<Self> {
        let invalid =
            |what: &str, e: &dyn std::fmt::Display| Error::Invalid(format!("rpc.tls: {what}: {e}"));
        let mut roots = RootCertStore::empty();
        for c in CertificateDer::pem_slice_iter(ca) {
            let c = c.map_err(|e| invalid("ca", &e))?;
            roots.add(c).map_err(|e| invalid("ca", &e))?;
        }
        if roots.is_empty() {
            return Err(Error::invalid("rpc.tls: ca: no certificates"));
        }
        let chain = CertificateDer::pem_slice_iter(cert)
            .collect::<StdResult<Vec<_>, _>>()
            .map_err(|e| invalid("cert", &e))?;
        let leaf = chain
            .first()
            .ok_or_else(|| Error::invalid("rpc.tls: cert: no certificates"))?;
        let identity = node_identity(leaf)?;
        let key = PrivateKeyDer::from_pem_slice(key).map_err(|e| invalid("key", &e))?;

        let provider = CryptoProvider::get_default()
            .cloned()
            .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
        let roots = Arc::new(roots);
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::clone(&roots), Arc::clone(&provider))
                .build()
                .map_err(|e| invalid("client verifier", &e))?;
        let mut server = ServerConfig::builder_with_provider(Arc::clone(&provider))
            .with_safe_default_protocol_versions()
            .map_err(|e| invalid("server", &e))?
            .with_client_cert_verifier(verifier)
            .with_single_cert(chain.clone(), key.clone_key())
            .map_err(|e| invalid("server", &e))?;
        server.alpn_protocols = ALPN.iter().map(|p| p.to_vec()).collect();
        let mut client = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| invalid("client", &e))?
            .with_root_certificates(roots)
            .with_client_auth_cert(chain, key)
            .map_err(|e| invalid("client", &e))?;
        client.alpn_protocols = ALPN.iter().map(|p| p.to_vec()).collect();
        Ok(Self {
            identity,
            server: Arc::new(server),
            client,
        })
    }

    /// Кого называет сертификат.
    pub fn identity(&self) -> &NodeIdentity {
        &self.identity
    }
}

async fn read_pem(path: PathBuf) -> Result<Vec<u8>> {
    tokio::fs::read(&path)
        .await
        .map_err(|e| Error::Invalid(format!("rpc.tls: cannot read {}: {e}", path.display())))
}

/// Состав кластера с кэшем на `members_ttl`.
struct Members {
    source: Arc<dyn Membership>,
    ttl: Duration,
    cached: Mutex<Option<(Instant, Arc<Vec<NodeInfo>>)>>,
}

impl Members {
    async fn live(&self) -> Result<Arc<Vec<NodeInfo>>> {
        if let Some((at, list)) = &*self.cached.lock().unwrap_or_else(PoisonError::into_inner)
            && at.elapsed() < self.ttl
        {
            return Ok(Arc::clone(list));
        }
        let list = Arc::new(self.source.live_info().await?);
        *self.cached.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((Instant::now(), Arc::clone(&list)));
        Ok(list)
    }
}

struct Inner {
    id: NodeId,
    /// TLS сервера для новых соединений ([`NodeRpc::reload_tls`]).
    tls: RwLock<Arc<ServerConfig>>,
    config: RpcConfig,
    permissions: Permissions,
    admission: Admission,
    members: Members,
    handlers: RwLock<HashMap<RpcService, Arc<dyn RpcHandler>>>,
    /// Клиент с текущим TLS; при замене сертификатов — новый (новые соединения).
    client: RwLock<reqwest::Client>,
}

/// Запросы узел → узел поверх HTTPS с mTLS; клоны разделяют состояние.
#[derive(Clone)]
pub struct NodeRpc {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for NodeRpc {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeRpc")
            .field("id", &self.inner.id)
            .finish_non_exhaustive()
    }
}

/// Итог попытки на одном узле маршрута.
enum Attempt {
    /// Ответ узла (или окончательная ошибка).
    Done(StdResult<Bytes, RpcError>),
    /// Узел недоступен — следующий.
    NextNode,
}

impl NodeRpc {
    /// Узел `id` с секцией `config`, TLS `tls` и составом кластера `members`. Сертификат с URI узла (или CN)
    /// должен называть `id`; сертификат с именами DNS/IP — хост адреса узла в составе (проверяют другие узлы).
    pub fn new(
        id: NodeId,
        config: &RpcConfig,
        tls: NodeTls,
        members: Arc<dyn Membership>,
    ) -> Result<Self> {
        config.check()?;
        check_identity(&id, &tls.identity)?;
        let client = build_client(config, tls.client)?;
        Ok(Self {
            inner: Arc::new(Inner {
                id,
                tls: RwLock::new(tls.server),
                permissions: Permissions::from_config(config)?,
                admission: Admission::new(config.max_in_flight),
                members: Members {
                    source: members,
                    ttl: config.members_ttl,
                    cached: Mutex::new(None),
                },
                handlers: RwLock::new(HashMap::new()),
                client: RwLock::new(client),
                config: config.clone(),
            }),
        })
    }

    /// Идентификатор узла.
    pub fn id(&self) -> &NodeId {
        &self.inner.id
    }

    /// Принимает запросы других узлов на `listener` до `shutdown`. Адрес слушателя узел объявляет в составе
    /// кластера (`NodeInfo::addr`, `https://<хост из сертификата>:<порт>`).
    pub async fn listen(
        &self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> Result<()> {
        let (tx, rx) = mpsc::channel(self.inner.config.max_in_flight);
        let accept = tokio::spawn(accept_loop(listener, Arc::clone(&self.inner), tx));
        let listener = TlsListener {
            rx,
            _accept: AbortOnDrop(accept),
        };
        let app = Router::new()
            .route("/rpc/{role}/{name}", post(http_handle))
            .layer(DefaultBodyLimit::max(self.inner.config.max_body_bytes))
            .with_state(Arc::clone(&self.inner));
        axum::serve(listener, app.into_make_service_with_connect_info::<Peer>())
            .with_graceful_shutdown(shutdown)
            .await
            .map_err(Error::from)
    }

    /// Заменяет сертификаты без перезапуска: новые соединения (сервера и клиента) — с `tls`, установленные
    /// дорабатывают на прежних. Сертификат проверяется, как в [`NodeRpc::new`]; ошибка — прежние остаются.
    pub fn reload_tls(&self, tls: NodeTls) -> Result<()> {
        check_identity(&self.inner.id, &tls.identity)?;
        let client = build_client(&self.inner.config, tls.client)?;
        *self
            .inner
            .tls
            .write()
            .unwrap_or_else(PoisonError::into_inner) = tls.server;
        *self
            .inner
            .client
            .write()
            .unwrap_or_else(PoisonError::into_inner) = client;
        Ok(())
    }

    /// Перечитывает файлы секции `tls` ([`NodeTls::load`]) по каждому событию `events` до конца потока.
    /// События — изменение каталога сертификатов, SIGHUP, запрос (источник — приложение, как у PLT-17/PLT-19).
    /// Ошибка — `warn` в лог, прежние сертификаты остаются. Ключ по ссылке `key` —
    /// [`NodeRpc::follow_tls_with_secrets`].
    pub async fn follow_tls<S>(&self, tls: &InternalTls, events: S)
    where
        S: Stream<Item = ()>,
    {
        self.follow(tls, None, events).await;
    }

    /// Как [`NodeRpc::follow_tls`], с хранилищем секретов `store`: по тем же событиям заново читаются файлы и
    /// ключ по ссылке `key` ([`NodeTls::load_with_secrets`]). Ключ, не подходящий к сертификату, недоступное
    /// хранилище, нет секрета — `warn` в лог, действует прежняя пара.
    pub async fn follow_tls_with_secrets<S>(
        &self,
        tls: &InternalTls,
        store: Arc<dyn SecretStore>,
        events: S,
    ) where
        S: Stream<Item = ()>,
    {
        self.follow(tls, Some(store.as_ref()), events).await;
    }

    async fn follow<S>(&self, tls: &InternalTls, store: Option<&dyn SecretStore>, events: S)
    where
        S: Stream<Item = ()>,
    {
        let mut events = std::pin::pin!(events);
        while events.next().await.is_some() {
            match NodeTls::load_from(tls, store)
                .await
                .and_then(|t| self.reload_tls(t))
            {
                Ok(()) => info!(node = %self.inner.id, "rpc: tls certificates reloaded"),
                Err(e) => warn!(
                    node = %self.inner.id,
                    error = %e,
                    "rpc: tls reload failed, keeping previous certificates"
                ),
            }
        }
    }
}

impl Inner {
    /// Вызывающий узел по сертификату соединения; по хостам — по составу кластера (не найден или неоднозначен —
    /// `Forbidden`).
    async fn caller(&self, identity: &NodeIdentity) -> StdResult<NodeId, RpcError> {
        if let NodeIdentity::Node(id) = identity {
            return Ok(id.clone());
        }
        let live = self.members.live().await.map_err(RpcError::Failed)?;
        identity.resolve(&live).ok_or_else(|| {
            debug!(peer = ?identity, "rpc: caller certificate names no single cluster node");
            RpcError::Failed(Error::Forbidden)
        })
    }

    /// Обработка запроса на этом узле: роли вызывающего по составу кластера, матрица, обработчик, предел
    /// параллельности, срок.
    async fn dispatch(
        &self,
        caller: &NodeId,
        service: &RpcService,
        key: String,
        body: Bytes,
        deadline: Duration,
    ) -> StdResult<Bytes, RpcError> {
        let live = self.members.live().await.map_err(RpcError::Failed)?;
        let roles = live
            .iter()
            .find(|n| n.id == *caller)
            .map(|n| n.roles.clone())
            .unwrap_or_default();
        if let Err(e) = self.permissions.check(&roles, service) {
            debug!(caller = %caller, service = %service, "rpc: forbidden by permissions matrix");
            return Err(e);
        }
        let handler = self
            .handlers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(service)
            .cloned()
            .ok_or(RpcError::Failed(Error::NotFound))?;
        let _slot = self.admission.try_admit()?;
        let request = RpcRequest {
            caller: caller.clone(),
            key,
            body,
            deadline,
        };
        match tokio::time::timeout(deadline, handler.handle(request)).await {
            Ok(Ok(out)) => Ok(out),
            Ok(Err(e)) => Err(RpcError::Failed(e)),
            Err(_) => Err(RpcError::Timeout),
        }
    }

    /// Вызов по маршруту до первого доступного узла.
    async fn call_routed(
        &self,
        service: &RpcService,
        key: &str,
        body: Bytes,
        started: Instant,
        deadline: Duration,
    ) -> StdResult<Bytes, RpcError> {
        let live = self.members.live().await.map_err(|e| {
            if e.is_retryable() {
                RpcError::Unavailable
            } else {
                RpcError::Failed(e)
            }
        })?;
        let nodes = route(&live, service, key)?;
        let mut framed = None;
        for node in nodes {
            let remaining = deadline.saturating_sub(started.elapsed());
            if node.id == self.id {
                return self
                    .dispatch(&self.id, service, key.to_owned(), body, remaining)
                    .await;
            }
            let framed = match &framed {
                Some(f) => Bytes::clone(f),
                None => {
                    let f = frame(key, &body)?;
                    if f.len() > self.config.max_body_bytes {
                        return Err(RpcError::Failed(Error::invalid(
                            "rpc: request exceeds rpc.max_body_bytes",
                        )));
                    }
                    framed.insert(f).clone()
                }
            };
            match self.remote(node, service, framed, remaining).await {
                Attempt::Done(out) => return out,
                Attempt::NextNode => {}
            }
        }
        Err(RpcError::Unavailable)
    }

    /// Попытка на узле `node`.
    async fn remote(
        &self,
        node: &NodeInfo,
        service: &RpcService,
        framed: Bytes,
        remaining: Duration,
    ) -> Attempt {
        let addr = node.addr.trim_end_matches('/');
        if !addr
            .get(..8)
            .is_some_and(|s| s.eq_ignore_ascii_case("https://"))
        {
            warn!(node = %node.id, "rpc: node address is not https://, skipped");
            return Attempt::NextNode;
        }
        let url = format!("{addr}/rpc/{}/{}", service.role(), service.name());
        let deadline_ms = remaining.as_millis().max(1).to_string();
        let client = self
            .client
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let sent = client
            .post(url)
            .header(H_DEADLINE, deadline_ms)
            .body(framed)
            .send()
            .await;
        let resp = match sent {
            Ok(r) => r,
            Err(e) if e.is_connect() => {
                debug!(node = %node.id, error = %e, "rpc: node unreachable, trying next");
                return Attempt::NextNode;
            }
            Err(e) => {
                warn!(node = %node.id, error = %e, "rpc: request failed");
                return Attempt::Done(Err(RpcError::Unavailable));
            }
        };
        let peer = resp
            .extensions()
            .get::<reqwest::tls::TlsInfo>()
            .and_then(|i| i.peer_certificate())
            .map(|der| node_identity(&CertificateDer::from(der)));
        match peer {
            Some(Ok(id)) if id.names(node) => {}
            other => {
                warn!(
                    node = %node.id,
                    peer = ?other.and_then(|r| r.ok()),
                    "rpc: server certificate names another node, skipped"
                );
                return Attempt::NextNode;
            }
        }
        let header = |name: &str| {
            resp.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        let (kind, retry_after, status) = (header(H_STATUS), header(H_RETRY_AFTER), resp.status());
        match resp.bytes().await {
            Ok(body) => Attempt::Done(decode(
                status,
                kind.as_deref(),
                retry_after.as_deref(),
                body,
            )),
            Err(e) => {
                warn!(node = %node.id, error = %e, "rpc: response failed");
                Attempt::Done(Err(RpcError::Unavailable))
            }
        }
    }
}

#[async_trait]
impl Rpc for NodeRpc {
    async fn call(
        &self,
        service: &RpcService,
        key: &str,
        body: Bytes,
        deadline: Duration,
    ) -> StdResult<Bytes, RpcError> {
        let started = Instant::now();
        tokio::time::timeout(
            deadline,
            self.inner
                .call_routed(service, key, body, started, deadline),
        )
        .await
        .unwrap_or(Err(RpcError::Timeout))
    }

    fn serve(&self, service: RpcService, handler: Arc<dyn RpcHandler>) -> Result<()> {
        let mut handlers = self
            .inner
            .handlers
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if handlers.contains_key(&service) {
            return Err(Error::Conflict);
        }
        handlers.insert(service, handler);
        Ok(())
    }
}

// ---------- Сервер ----------

/// Вызывающий узел соединения (по клиентскому сертификату).
#[derive(Debug, Clone)]
struct Peer {
    identity: NodeIdentity,
}

impl Connected<IncomingStream<'_, TlsListener>> for Peer {
    fn connect_info(stream: IncomingStream<'_, TlsListener>) -> Self {
        stream.remote_addr().clone()
    }
}

/// Снимает задачу при удалении.
struct AbortOnDrop(JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Соединения после рукопожатия mTLS (рукопожатия — в отдельных задачах, не задерживают приём).
struct TlsListener {
    rx: mpsc::Receiver<(TlsStream<TcpStream>, Peer)>,
    _accept: AbortOnDrop,
}

impl Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = Peer;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.rx.recv().await {
            Some(conn) => conn,
            // Приём остановлен: новых соединений не будет.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Err(io::Error::other("rpc listener has no local peer"))
    }
}

async fn accept_loop(
    listener: TcpListener,
    inner: Arc<Inner>,
    tx: mpsc::Sender<(TlsStream<TcpStream>, Peer)>,
) {
    let handshake_timeout = inner.config.handshake_timeout;
    loop {
        let (tcp, remote) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                warn!(error = %e, "rpc: accept failed");
                // Пауза при нехватке дескрипторов и т. п., чтобы не крутить цикл.
                tokio::time::sleep(inner.config.accept_retry_delay).await;
                continue;
            }
        };
        // Текущие сертификаты: замена ([`NodeRpc::reload_tls`]) действует на следующее соединение.
        let acceptor = TlsAcceptor::from(Arc::clone(
            &inner.tls.read().unwrap_or_else(PoisonError::into_inner),
        ));
        let tx = tx.clone();
        tokio::spawn(async move {
            match handshake(acceptor, tcp, handshake_timeout).await {
                Ok(conn) => {
                    let _ = tx.send(conn).await;
                }
                Err(e) => debug!(remote = %remote, error = %e, "rpc: handshake rejected"),
            }
        });
    }
}

async fn handshake(
    acceptor: TlsAcceptor,
    tcp: TcpStream,
    timeout: Duration,
) -> Result<(TlsStream<TcpStream>, Peer)> {
    let stream = tokio::time::timeout(timeout, acceptor.accept(tcp))
        .await
        .map_err(|_| Error::UNAVAILABLE)??;
    let leaf = stream
        .get_ref()
        .1
        .peer_certificates()
        .and_then(|c| c.first())
        .ok_or(Error::Forbidden)?;
    let identity = node_identity(leaf)?;
    Ok((stream, Peer { identity }))
}

async fn http_handle(
    State(rpc): State<Arc<Inner>>,
    ConnectInfo(peer): ConnectInfo<Peer>,
    Path((role, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let out = async {
        let service = role
            .parse::<Role>()
            .ok()
            .and_then(|r| RpcService::new(r, name).ok())
            .ok_or(RpcError::Failed(Error::NotFound))?;
        let deadline = headers
            .get(H_DEADLINE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_millis)
            .ok_or_else(|| RpcError::Failed(Error::invalid("rpc: missing deadline")))?;
        let (key, body) = unframe(body)
            .ok_or_else(|| RpcError::Failed(Error::invalid("rpc: malformed request")))?;
        let caller = rpc.caller(&peer.identity).await?;
        rpc.dispatch(&caller, &service, key, body, deadline).await
    }
    .await;
    encode(out)
}

// ---------- Формат ----------

fn frame(key: &str, body: &Bytes) -> StdResult<Bytes, RpcError> {
    let len = u32::try_from(key.len())
        .map_err(|_| RpcError::Failed(Error::invalid("rpc: key too long")))?;
    let mut buf = BytesMut::with_capacity(4 + key.len() + body.len());
    buf.put_u32(len);
    buf.put_slice(key.as_bytes());
    buf.put_slice(body);
    Ok(buf.freeze())
}

fn unframe(mut buf: Bytes) -> Option<(String, Bytes)> {
    if buf.len() < 4 {
        return None;
    }
    let len = usize::try_from(buf.get_u32()).ok()?;
    if buf.len() < len {
        return None;
    }
    let key = buf.split_to(len);
    let key = String::from_utf8(key.to_vec()).ok()?;
    Some((key, buf))
}

fn encode(out: StdResult<Bytes, RpcError>) -> Response {
    let (status, kind, body, retry_after) = match out {
        Ok(body) => (StatusCode::OK, "ok", body, None),
        Err(RpcError::Busy) => (StatusCode::TOO_MANY_REQUESTS, "busy", Bytes::new(), None),
        Err(RpcError::Timeout) => (StatusCode::GATEWAY_TIMEOUT, "timeout", Bytes::new(), None),
        Err(RpcError::Unavailable) => (
            StatusCode::SERVICE_UNAVAILABLE,
            "unavailable",
            Bytes::new(),
            None,
        ),
        Err(RpcError::Failed(e)) => match e {
            Error::NotFound => (StatusCode::NOT_FOUND, "not-found", Bytes::new(), None),
            Error::Conflict => (StatusCode::CONFLICT, "conflict", Bytes::new(), None),
            Error::Invalid(m) => (StatusCode::BAD_REQUEST, "invalid", Bytes::from(m), None),
            Error::Forbidden => (StatusCode::FORBIDDEN, "forbidden", Bytes::new(), None),
            Error::QuotaExceeded => (
                StatusCode::INSUFFICIENT_STORAGE,
                "quota",
                Bytes::new(),
                None,
            ),
            Error::Unavailable { retry_after } => (
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                Bytes::new(),
                retry_after,
            ),
            Error::Internal(m) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal",
                Bytes::from(m),
                None,
            ),
        },
    };
    let mut resp = (status, body).into_response();
    let headers = resp.headers_mut();
    headers.insert(H_STATUS, HeaderValue::from_static(kind));
    if let Some(d) = retry_after {
        headers.insert(
            H_RETRY_AFTER,
            HeaderValue::from(u64::try_from(d.as_millis()).unwrap_or(u64::MAX)),
        );
    }
    resp
}

fn decode(
    status: StatusCode,
    kind: Option<&str>,
    retry_after: Option<&str>,
    body: Bytes,
) -> StdResult<Bytes, RpcError> {
    let text = || String::from_utf8_lossy(&body).into_owned();
    let failed = |e| Err(RpcError::Failed(e));
    match kind {
        Some("ok") => Ok(body),
        Some("busy") => Err(RpcError::Busy),
        Some("timeout") => Err(RpcError::Timeout),
        Some("not-found") => failed(Error::NotFound),
        Some("conflict") => failed(Error::Conflict),
        Some("invalid") => failed(Error::Invalid(text())),
        Some("forbidden") => failed(Error::Forbidden),
        Some("quota") => failed(Error::QuotaExceeded),
        Some("unavailable") => failed(Error::Unavailable {
            retry_after: retry_after
                .and_then(|v| v.parse().ok())
                .map(Duration::from_millis),
        }),
        Some("internal") => failed(Error::Internal(text())),
        _ => failed(Error::Internal(format!(
            "rpc: unexpected response {status}"
        ))),
    }
}

#[cfg(test)]
mod tests;
