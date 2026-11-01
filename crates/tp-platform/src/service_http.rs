//! Служебный HTTP узла (SRS 6.1, 6.4): `/metrics`, `/healthz/live`, `/healthz/ready`.
//!
//! Готовность узла — все зарегистрированные роли готовы ([`ReadyHandle`]) и узел не выводится из пула (drain);
//! по `/healthz/ready` балансировщик включает и исключает узел.
//!
//! Служебный HTTP — только по TLS (решение «Шифрование любого обмена», PLT-11): сертификаты узла [`TlsCerts`]
//! (файлы, секреты Kubernetes, ACME — с перечитыванием без перезапуска, как у слушателей протоколов).
//! Открытый HTTP — только `profile: dev` с `service_http.allow_plaintext: true` и предупреждением в лог при
//! каждом запуске; вне dev — ошибка конфигурации.
//!
//! Адрес — только внутренний (SRS 6.8, PLT-18): по умолчанию `127.0.0.1:8081`; loopback, частные и локальные
//! диапазоны разрешены всегда, остальные адреса (в том числе `0.0.0.0` / `[::]` — все интерфейсы) — только из
//! явного списка `service_http.internal_networks`, иначе ошибка проверки конфигурации.

use std::collections::BTreeMap;
use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinSet;
use tokio_rustls::server::TlsStream;
use tp_contracts::Error;
use tp_contracts::config::duration;
use tp_contracts::role::{CancellationToken, ReadyHandle};
use tp_contracts::tls::Profile;

use crate::listen::Cidr;
use crate::telemetry::Metrics;
use crate::tls::TlsCerts;

/// Пауза после ошибки приёма соединения (нехватка дескрипторов и т. п.).
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// Секция `service_http`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceHttpConfig {
    /// Адрес служебного HTTP (метрики и проверки доступности), только внутренний (`127.0.0.1:8081`).
    /// Требует перезапуска.
    pub listen: SocketAddr,
    /// Сети, которые считаются внутренними сверх loopback и частных диапазонов (`10.0.0.0/8`, `172.16.0.0/12`,
    /// `192.168.0.0/16`, `169.254.0.0/16`, `fc00::/7`, `fe80::/10`). `listen` вне них — ошибка проверки.
    /// Все интерфейсы (`0.0.0.0`, `[::]`) — только если адрес явно входит в список (`0.0.0.0/32`), например за
    /// межсетевым экраном или в поде Kubernetes. Требует перезапуска.
    #[schemars(with = "Vec<String>")]
    pub internal_networks: Vec<Cidr>,
    /// Открытый HTTP без TLS. Только `profile: dev`, с предупреждением в лог; иначе — ошибка конфигурации.
    /// Требует перезапуска.
    pub allow_plaintext: bool,
    /// Время на TLS-рукопожатие (`10s`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub handshake_timeout: Duration,
    /// Одновременных незавершённых TLS-рукопожатий; сверх — новые соединения ждут в очереди сокета.
    pub max_handshakes: usize,
}

impl Default for ServiceHttpConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 8081)),
            internal_networks: Vec::new(),
            allow_plaintext: false,
            handshake_timeout: Duration::from_secs(10),
            max_handshakes: 128,
        }
    }
}

impl ServiceHttpConfig {
    /// Проверка секции (`thepost config check` и запуск): адрес — только внутренний, `allow_plaintext` — только
    /// в профиле dev.
    pub fn check(&self, profile: Profile) -> tp_contracts::Result<()> {
        let ip = self.listen.ip();
        if !is_internal(ip) && !self.internal_networks.iter().any(|n| n.contains(ip)) {
            return Err(Error::Invalid(format!(
                "listen {}: not an internal address; /metrics and /healthz listen only on internal addresses (loopback, private ranges or service_http.internal_networks)",
                self.listen
            )));
        }
        if self.allow_plaintext && profile != Profile::Dev {
            return Err(Error::Invalid(
                "allow_plaintext is allowed only with profile: dev; /metrics and /healthz are served over TLS"
                    .to_owned(),
            ));
        }
        if self.handshake_timeout.is_zero() {
            return Err(Error::Invalid(
                "handshake_timeout must be positive".to_owned(),
            ));
        }
        if self.max_handshakes == 0 {
            return Err(Error::Invalid("max_handshakes must be positive".to_owned()));
        }
        Ok(())
    }

    /// Предупреждение для лога при открытом служебном HTTP.
    pub fn plaintext_warning() -> &'static str {
        "service http (/metrics, /healthz) is NOT encrypted (service_http.allow_plaintext, profile dev); never use in production"
    }
}

/// Адрес внутренний без настройки: loopback, частные и локальные диапазоны IPv4 и IPv6.
/// Неуказанный адрес (`0.0.0.0`, `::`) — все интерфейсы, в том числе публичные, — не внутренний.
fn is_internal(ip: IpAddr) -> bool {
    match ip.to_canonical() {
        IpAddr::V4(v4) => v4.is_loopback() || v4.is_private() || v4.is_link_local(),
        IpAddr::V6(v6) => v6.is_loopback() || v6.is_unique_local() || v6.is_unicast_link_local(),
    }
}

/// Состояние готовности узла. Клонируется; роли получают [`ReadyHandle`] через [`Health::register`].
#[derive(Debug, Clone, Default)]
pub struct Health(Arc<HealthInner>);

#[derive(Debug, Default)]
struct HealthInner {
    roles: Mutex<BTreeMap<String, ReadyHandle>>,
    draining: AtomicBool,
}

impl Health {
    /// Регистрирует роль; признак её готовности изначально `false`. Повторная регистрация возвращает тот же признак.
    pub fn register(&self, role: &str) -> ReadyHandle {
        self.roles().entry(role.to_owned()).or_default().clone()
    }

    /// Выводит узел из пула: `/healthz/ready` отвечает 503 до конца работы процесса.
    pub fn drain(&self) {
        self.0.draining.store(true, Ordering::Release);
    }

    /// Узел выводится из пула.
    pub fn is_draining(&self) -> bool {
        self.0.draining.load(Ordering::Acquire)
    }

    /// Узел готов: не drain, есть роли, все роли готовы.
    pub fn is_ready(&self) -> bool {
        let roles = self.roles();
        !self.is_draining() && !roles.is_empty() && roles.values().all(ReadyHandle::is_ready)
    }

    /// Готовность по ролям.
    pub fn roles_status(&self) -> Vec<(String, bool)> {
        self.roles()
            .iter()
            .map(|(name, h)| (name.clone(), h.is_ready()))
            .collect()
    }

    fn roles(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ReadyHandle>> {
        self.0.roles.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug, Clone)]
struct AppState {
    health: Health,
    metrics: Metrics,
}

/// Маршруты служебного HTTP.
pub fn router(health: Health, metrics: Metrics) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/healthz/live", get(|| async { "live\n" }))
        .route("/healthz/ready", get(ready_handler))
        .with_state(AppState { health, metrics })
}

/// Слушающий сокет для `cfg.listen`. Требует контекста tokio-рантайма.
pub fn bind(cfg: &ServiceHttpConfig) -> tp_contracts::Result<TcpListener> {
    tp_sys::bind_tcp(cfg.listen, &tp_sys::ListenerConfig::default())
        .map_err(|e| Error::Invalid(format!("service_http.listen {}: {e}", cfg.listen)))
}

/// Обслуживает служебный HTTP до сигнала остановки: по TLS на сертификатах `certs` или, только в профиле dev
/// с `allow_plaintext`, открытым HTTP с предупреждением в лог. Нарушение — `Error::Invalid` до приёма соединений.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    cfg: &ServiceHttpConfig,
    profile: Profile,
    certs: Option<Arc<TlsCerts>>,
    shutdown: CancellationToken,
) -> tp_contracts::Result<()> {
    cfg.check(profile).map_err(|e| match e {
        Error::Invalid(m) => Error::Invalid(format!("service_http: {m}")),
        other => other,
    })?;
    if cfg.allow_plaintext {
        tracing::warn!("{}", ServiceHttpConfig::plaintext_warning());
        return serve_on(listener, router, shutdown).await;
    }
    let certs = certs.ok_or_else(|| {
        Error::Invalid(
            "service_http: TLS certificates are required (section tls or acme); plaintext only with profile: dev and service_http.allow_plaintext"
                .to_owned(),
        )
    })?;
    let tls = TlsListener {
        tcp: listener,
        certs,
        timeout: cfg.handshake_timeout,
        max_pending: cfg.max_handshakes,
        pending: JoinSet::new(),
    };
    serve_on(tls, router, shutdown).await
}

async fn serve_on<L>(
    listener: L,
    router: Router,
    shutdown: CancellationToken,
) -> tp_contracts::Result<()>
where
    L: axum::serve::Listener,
    L::Addr: fmt::Debug,
{
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { shutdown.cancelled().await })
        .await
        .map_err(|e| Error::Internal(format!("service http: {e}")))
}

/// Результат рукопожатия: поток TLS и адрес клиента; `None` — рукопожатие не удалось.
type Handshake = Option<(TlsStream<TcpStream>, SocketAddr)>;

/// Слушатель TLS для axum: рукопожатия идут параллельно в задачах с таймаутом, медленный клиент не задерживает
/// приём остальных. Сертификаты берутся из [`TlsCerts`] на каждое соединение — перечитывание действует сразу.
struct TlsListener {
    tcp: TcpListener,
    certs: Arc<TlsCerts>,
    timeout: Duration,
    max_pending: usize,
    pending: JoinSet<Handshake>,
}

impl TlsListener {
    fn start_handshake(&mut self, tcp: TcpStream, peer: SocketAddr) {
        let acceptor = self.certs.acceptor();
        let timeout = self.timeout;
        self.pending.spawn(async move {
            match tokio::time::timeout(timeout, acceptor.accept(tcp)).await {
                Ok(Ok(tls)) => Some((tls, peer)),
                Ok(Err(e)) => {
                    tracing::debug!(%peer, error = %e, "service http: tls handshake failed");
                    None
                }
                Err(_) => {
                    tracing::debug!(%peer, "service http: tls handshake timed out");
                    None
                }
            }
        });
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let room = self.pending.len() < self.max_pending;
            let busy = !self.pending.is_empty();
            tokio::select! {
                accepted = self.tcp.accept(), if room => match accepted {
                    Ok((tcp, peer)) => self.start_handshake(tcp, peer),
                    Err(e) => {
                        tracing::warn!(error = %e, "service http: accept failed");
                        tokio::time::sleep(ACCEPT_BACKOFF).await;
                    }
                },
                Some(done) = self.pending.join_next(), if busy => {
                    if let Ok(Some(conn)) = done {
                        return conn;
                    }
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.tcp.local_addr()
    }
}

async fn metrics_handler(State(s): State<AppState>) -> Response {
    match s.metrics.encode() {
        Ok(text) => (
            [(
                header::CONTENT_TYPE,
                "application/openmetrics-text; version=1.0.0; charset=utf-8",
            )],
            text,
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "metrics encoding failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn ready_handler(State(s): State<AppState>) -> Response {
    let mut body = String::new();
    if s.health.is_draining() {
        body.push_str("draining\n");
    }
    for (role, ready) in s.health.roles_status() {
        body.push_str(&role);
        body.push_str(if ready { " ready\n" } else { " not-ready\n" });
    }
    let status = if s.health.is_ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, body).into_response()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use prometheus_client::metrics::counter::Counter;
    use rustls::pki_types::ServerName;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    use super::*;
    use crate::telemetry::BusSample;
    use crate::tls::tests::{CERT_A, KEY_A, TempDir, connector, write_pair};

    fn local() -> ServiceHttpConfig {
        ServiceHttpConfig {
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            ..ServiceHttpConfig::default()
        }
    }

    async fn request<S: AsyncRead + AsyncWrite + Unpin>(mut s: S, path: &str) -> (u16, String) {
        let req = format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
        s.write_all(req.as_bytes()).await.expect("write");
        let mut raw = Vec::new();
        // Сервер может закрыть TLS без close_notify: берём то, что успели прочитать.
        let _ = s.read_to_end(&mut raw).await;
        let resp = String::from_utf8_lossy(&raw).into_owned();
        let code = resp
            .split(' ')
            .nth(1)
            .and_then(|c| c.parse().ok())
            .expect("status");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
        (code, body)
    }

    async fn get_plain(addr: SocketAddr, path: &str) -> (u16, String) {
        let s = TcpStream::connect(addr).await.expect("connect");
        request(s, path).await
    }

    async fn get_tls(addr: SocketAddr, path: &str) -> (u16, String) {
        let tcp = TcpStream::connect(addr).await.expect("connect");
        let name = ServerName::try_from("localhost").expect("name");
        let tls = connector(&[CERT_A], &[&rustls::version::TLS13])
            .connect(name, tcp)
            .await
            .expect("tls handshake");
        request(tls, path).await
    }

    #[test]
    fn ready_follows_roles_and_drain() {
        let health = Health::default();
        assert!(!health.is_ready(), "no roles yet");
        let imap = health.register("imap");
        let smtp = health.register("smtp-in");
        assert!(!health.is_ready());
        imap.set(true);
        assert!(!health.is_ready());
        smtp.set(true);
        assert!(health.is_ready());
        health.register("imap").set(false);
        assert!(!health.is_ready(), "same handle on re-register");
        imap.set(true);
        assert!(health.is_ready());
        health.drain();
        assert!(!health.is_ready());
        assert_eq!(
            health.roles_status(),
            [("imap".to_owned(), true), ("smtp-in".to_owned(), true)]
        );
    }

    /// Счётчики шины в процессе (как `tp_cluster::BusCounters`): платформа читает снимок на каждый `/metrics`.
    #[derive(Default)]
    struct Bus {
        published: AtomicU64,
        received: AtomicU64,
    }

    impl Bus {
        fn publish(&self) {
            self.published.fetch_add(1, Ordering::Relaxed);
            self.received.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[tokio::test]
    async fn https_endpoints_with_bus_metrics() {
        let dir = TempDir::new("service-https");
        let certs = Arc::new(
            TlsCerts::load(&write_pair(&dir.0, CERT_A, KEY_A))
                .await
                .expect("certs"),
        );
        let health = Health::default();
        let metrics = Metrics::default();
        let hits = Counter::<u64>::default();
        metrics.register("tp_hits", "Hits", hits.clone());
        hits.inc();
        let bus = Arc::new(Bus::default());
        let source = bus.clone();
        metrics.register_bus(move || BusSample {
            connected: true,
            published: source.published.load(Ordering::Relaxed),
            received: source.received.load(Ordering::Relaxed),
            ..BusSample::default()
        });

        let cfg = local();
        let listener = bind(&cfg).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let shutdown = CancellationToken::new();
        let trigger = shutdown.clone();
        let router = router(health.clone(), metrics);
        let server = tokio::spawn(async move {
            serve(listener, router, &cfg, Profile::Prod, Some(certs), shutdown).await
        });

        assert_eq!(
            get_tls(addr, "/healthz/live").await,
            (200, "live\n".to_owned())
        );
        let role = health.register("jmap");
        assert_eq!(
            get_tls(addr, "/healthz/ready").await,
            (503, "jmap not-ready\n".to_owned())
        );
        role.set(true);
        assert_eq!(
            get_tls(addr, "/healthz/ready").await,
            (200, "jmap ready\n".to_owned())
        );
        health.drain();
        assert_eq!(
            get_tls(addr, "/healthz/ready").await,
            (503, "draining\njmap ready\n".to_owned())
        );

        bus.publish();
        bus.publish();
        let (code, body) = get_tls(addr, "/metrics").await;
        assert_eq!(code, 200);
        assert!(body.contains("tp_hits_total 1"), "{body}");
        assert!(body.contains("tp_bus_published_total 2"), "{body}");
        assert!(body.contains("tp_bus_received_total 2"), "{body}");
        assert!(body.contains("tp_bus_connected 1"), "{body}");
        bus.publish();
        let (_, body) = get_tls(addr, "/metrics").await;
        assert!(body.contains("tp_bus_published_total 3"), "{body}");

        // Открытый HTTP на порту TLS не обслуживается.
        let mut plain = TcpStream::connect(addr).await.expect("connect");
        plain
            .write_all(b"GET /healthz/live HTTP/1.1\r\nHost: t\r\n\r\n")
            .await
            .expect("write");
        let mut buf = Vec::new();
        let _ = plain.read_to_end(&mut buf).await;
        assert!(!String::from_utf8_lossy(&buf).contains("live"), "{buf:?}");
        // После неудачного рукопожатия сервер продолжает обслуживать TLS.
        assert_eq!(get_tls(addr, "/healthz/live").await.0, 200);

        trigger.cancel();
        server.await.expect("join").expect("serve");
    }

    #[tokio::test]
    async fn plaintext_only_in_dev() {
        let open = ServiceHttpConfig {
            allow_plaintext: true,
            ..local()
        };
        assert!(open.check(Profile::Dev).is_ok());
        let e = open.check(Profile::Prod).expect_err("prod");
        assert!(
            matches!(&e, Error::Invalid(m) if m.contains("profile: dev")),
            "{e:?}"
        );

        let app = || router(Health::default(), Metrics::default());
        let listener = bind(&open).expect("bind");
        let e = serve(
            listener,
            app(),
            &open,
            Profile::Prod,
            None,
            CancellationToken::new(),
        )
        .await
        .expect_err("plaintext outside dev");
        assert!(
            matches!(&e, Error::Invalid(m) if m.starts_with("service_http: allow_plaintext")),
            "{e:?}"
        );

        // Без сертификатов и без allow_plaintext — ошибка в любом профиле.
        let listener = bind(&local()).expect("bind");
        let e = serve(
            listener,
            app(),
            &local(),
            Profile::Dev,
            None,
            CancellationToken::new(),
        )
        .await
        .expect_err("no certs");
        assert!(
            matches!(&e, Error::Invalid(m) if m.contains("TLS certificates are required")),
            "{e:?}"
        );

        let listener = bind(&open).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let shutdown = CancellationToken::new();
        let trigger = shutdown.clone();
        let server = tokio::spawn(async move {
            serve(listener, app(), &open, Profile::Dev, None, shutdown).await
        });
        assert_eq!(
            get_plain(addr, "/healthz/live").await,
            (200, "live\n".to_owned())
        );
        trigger.cancel();
        server.await.expect("join").expect("serve");
    }

    #[test]
    fn listen_internal_only() {
        assert_eq!(
            ServiceHttpConfig::default().listen,
            SocketAddr::from(([127, 0, 0, 1], 8081))
        );
        assert!(ServiceHttpConfig::default().check(Profile::Prod).is_ok());
        let at = |listen: &str, nets: &[&str]| ServiceHttpConfig {
            listen: listen.parse().expect("addr"),
            internal_networks: nets.iter().map(|n| n.parse().expect("cidr")).collect(),
            ..ServiceHttpConfig::default()
        };
        for ok in [
            "127.0.0.1:1",
            "10.0.0.1:1",
            "172.31.0.1:1",
            "192.168.1.1:1",
            "169.254.0.1:1",
            "[::1]:1",
            "[fd12::1]:1",
            "[fe80::1]:1",
            "[::ffff:10.0.0.1]:1",
        ] {
            assert!(at(ok, &[]).check(Profile::Prod).is_ok(), "{ok}");
        }
        for bad in [
            "0.0.0.0:8081",
            "[::]:8081",
            "8.8.8.8:1",
            "[2001:db8::1]:1",
            "172.32.0.1:1",
        ] {
            let e = at(bad, &[]).check(Profile::Dev).expect_err(bad);
            assert!(
                matches!(&e, Error::Invalid(m) if m.contains("not an internal address")),
                "{bad}: {e:?}"
            );
        }
        assert!(
            at("0.0.0.0:8081", &["0.0.0.0/32"])
                .check(Profile::Prod)
                .is_ok()
        );
        assert!(at("[::]:8081", &["::/128"]).check(Profile::Prod).is_ok());
        assert!(
            at("8.8.8.8:1", &["8.8.8.0/24"])
                .check(Profile::Prod)
                .is_ok()
        );
        assert!(
            at("8.8.8.8:1", &["8.8.4.0/24"])
                .check(Profile::Prod)
                .is_err()
        );

        let cfg: ServiceHttpConfig = serde_yaml_ng::from_str(
            "listen: 0.0.0.0:8081\ninternal_networks: [0.0.0.0/32, 10.0.0.0/8]\n",
        )
        .expect("yaml");
        assert_eq!(cfg.internal_networks.len(), 2);
        assert!(cfg.check(Profile::Prod).is_ok());
    }

    #[test]
    fn config_from_yaml() {
        let cfg: ServiceHttpConfig = serde_yaml_ng::from_str(
            "listen: 127.0.0.1:9090\nhandshake_timeout: 2s\nallow_plaintext: true\n",
        )
        .expect("parse");
        assert_eq!(cfg.handshake_timeout, Duration::from_secs(2));
        assert!(cfg.allow_plaintext);
        assert_eq!(
            cfg.max_handshakes,
            ServiceHttpConfig::default().max_handshakes
        );
        assert!(serde_yaml_ng::from_str::<ServiceHttpConfig>("handshake_timeout: 2000").is_err());
        let zero = ServiceHttpConfig {
            handshake_timeout: Duration::ZERO,
            ..ServiceHttpConfig::default()
        };
        assert!(zero.check(Profile::Dev).is_err());
    }
}
