//! TLS сервера (SRS 6.2): конфигурация rustls из PEM-файлов (цепочка и ключ) с перечитыванием без перезапуска —
//! по событию ОС в каталогах файлов (inotify, [`crate::fswatch`]; замена Secret через символическую ссылку), по
//! SIGHUP, по новой версии секции и по запросу (например, после выпуска ACME). Опрос mtime — только если событий
//! ОС нет (SRS 2.8).
//!
//! Закрытый ключ (PLT-24, решение 37) — из файла `key_file` или из хранилища секретов OpenBao/Vault по ссылке
//! `key: { secret: "<путь>#<поле>" }` (PEM в поле секрета; на диск не пишется). Новая версия секрета подхватывается
//! без перезапуска: секрет перечитывается с периодом `key_poll_interval`, по SIGHUP, по событию файла цепочки и по
//! запросу; ключ, не подходящий к цепочке, отклоняется — действует прежняя пара, пока цепочка и ключ не совпадут.
//!
//! Клиентский доступ — TLS 1.3; TLS 1.2 включается настройкой `min_version: "1.2"` для устаревших клиентов.
//! Ошибка перечитывания оставляет в силе прежние сертификаты.
//!
//! Сертификат клиента (mTLS, PLT-22) — по настройке слушателя ([`crate::EndpointConfig::client_ca_file`]):
//! необязательный, проверяется по CA слушателя (`ClientAuth`); проверенная цепочка доступна протоколу через
//! `peer_certificates()` соединения TLS. Сертификат от чужого CA — отказ рукопожатия, без сертификата —
//! соединение принимается. Без настройки сертификат клиента не запрашивается.
//!
//! Списки отзыва (CRL, PLT-28) — `client_crl_files` слушателя (PEM или DER): отозванный сертификат клиента не
//! признаётся — отказ рукопожатия, как сертификату чужого CA. Сертификат CA, для которого списка нет, принимается.
//! Файлы CA и CRL перечитываются без перезапуска вместе с сертификатами сервера: по событию ОС в их каталогах,
//! по SIGHUP, опросом (без событий ОС) и по запросу; ошибка перечитывания оставляет в силе прежние CA и списки.
//!
//! Текущие цепочка и ключ для подписи (профиль `.mobileconfig`, PLT-23) — [`TlsCerts::certified_key`]: то же, что
//! у слушателей, включая перечитанное; ключ — `Arc<dyn SigningKey>` rustls, закрытый ключ наружу не выдаётся.

use std::fmt;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::time::{Duration, SystemTime};

use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, CertificateRevocationListDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::ClientCertVerifier;
use rustls::sign::CertifiedKey;
use rustls::{RootCertStore, ServerConfig};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tp_contracts::Error;
use tp_contracts::config::duration;
use tp_contracts::role::CancellationToken;
use tp_contracts::secrets::{Secret, SecretRef, SecretStore};

use crate::fswatch::{self, DirWatch, FsEvents};
use crate::reload::Hup;

/// Секция `tls`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    /// PEM-файл с цепочкой сертификатов (сертификат узла первым).
    pub cert_file: PathBuf,
    /// PEM-файл с закрытым ключом (PKCS#8, PKCS#1 или SEC1).
    pub key_file: PathBuf,
    /// Закрытый ключ из хранилища секретов вместо `key_file`: `{ secret: "<путь>#<поле>" }`, в поле — PEM.
    pub key: Option<SecretRef>,
    /// Минимальная версия TLS для клиентского доступа.
    pub min_version: TlsVersion,
    /// Период запасного опроса файлов (`1m`), только если события ОС (inotify) недоступны; `0s` — без опроса,
    /// только по SIGHUP, новой версии секции и запросу.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub reload_interval: Duration,
    /// Период проверки новой версии секрета `key` (`1m`); `0s` — без опроса, только по SIGHUP, событию файла
    /// цепочки, новой версии секции и запросу.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub key_poll_interval: Duration,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            cert_file: PathBuf::from("/etc/thepost/tls/fullchain.pem"),
            key_file: PathBuf::from("/etc/thepost/tls/privkey.pem"),
            key: None,
            min_version: TlsVersion::Tls13,
            reload_interval: Duration::from_secs(60),
            key_poll_interval: Duration::from_secs(60),
        }
    }
}

/// Версия TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
pub enum TlsVersion {
    /// TLS 1.2 (устаревшие клиенты).
    #[serde(rename = "1.2")]
    Tls12,
    /// TLS 1.3.
    #[default]
    #[serde(rename = "1.3")]
    Tls13,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TlsError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}: no certificates found")]
    NoCert(PathBuf),
    #[error("{0}: no private key found")]
    NoKey(PathBuf),
    #[error("tls.key: secret {0}: no private key in PEM found")]
    NoKeyInSecret(SecretRef),
    #[error("tls.key: secret {0}: the secret store is not connected")]
    NoStore(SecretRef),
    #[error("{path}: client CA: {reason}")]
    ClientCa { path: PathBuf, reason: String },
    #[error("{path}: client CRL: {reason}")]
    ClientCrl { path: PathBuf, reason: String },
    #[error("tls: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("tls: loader task failed")]
    Join,
}

impl From<TlsError> for Error {
    fn from(e: TlsError) -> Self {
        match e {
            TlsError::Join => Error::Internal(e.to_string()),
            _ => Error::Invalid(e.to_string()),
        }
    }
}

/// Отметка версии файла: mtime и размер.
type FileStamp = (Option<SystemTime>, u64);

/// Отметка версии файлов: цепочка и ключ.
type Stamp = [FileStamp; 2];

/// Действующие сертификаты сервера. Разделяется слушателями через `Arc`; новые соединения
/// получают текущую конфигурацию, установленные соединения не затрагиваются.
pub struct TlsCerts {
    current: RwLock<Arc<Loaded>>,
    files: Mutex<Source>,
    store: Option<Arc<dyn SecretStore>>,
    /// Проверки сертификата клиента слушателей: их файлы CA и CRL перечитываются вместе с сертификатами.
    clients: Mutex<Vec<Weak<ClientFiles>>>,
}

/// Источник действующей пары: секция, отметка файлов и значение секрета ключа (сравнение версий).
#[derive(Clone)]
struct Source {
    cfg: TlsConfig,
    stamp: Stamp,
    key: Option<Secret>,
}

/// Конфигурации с проверкой сертификата клиента: слушатель, его действующий верификатор и конфигурация.
type ClientVariants = Vec<(
    Weak<ClientFiles>,
    Arc<dyn ClientCertVerifier>,
    Arc<ServerConfig>,
)>;

/// Загруженные цепочка и ключ: конфигурация без сертификата клиента и варианты с ним для слушателей с
/// [`ClientAuth`] (строятся при первом рукопожатии и живут до перечитывания файлов сервера либо CA и CRL клиента).
struct Loaded {
    plain: Arc<ServerConfig>,
    /// Цепочка и ключ подписи (ключ загружен провайдером криптографии слушателей).
    certified: Arc<CertifiedKey>,
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    min_version: TlsVersion,
    with_client: Mutex<ClientVariants>,
}

impl Loaded {
    fn new(
        chain: Vec<CertificateDer<'static>>,
        key: PrivateKeyDer<'static>,
        min_version: TlsVersion,
    ) -> Result<Self, TlsError> {
        let plain = Arc::new(build(chain.clone(), key.clone_key(), min_version, None)?);
        let certified = Arc::new(CertifiedKey::from_der(
            chain.clone(),
            key.clone_key(),
            &provider(),
        )?);
        Ok(Self {
            plain,
            certified,
            chain,
            key,
            min_version,
            with_client: Mutex::new(Vec::new()),
        })
    }

    /// Конфигурация с проверкой сертификата клиента `auth`.
    fn for_client(&self, auth: &ClientAuth) -> Result<Arc<ServerConfig>, TlsError> {
        let mut variants = self
            .with_client
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let verifier = auth.files.state().verifier;
        let owner = Arc::downgrade(&auth.files);
        if let Some((_, _, cfg)) = variants
            .iter()
            .find(|(o, v, _)| o.ptr_eq(&owner) && Arc::ptr_eq(v, &verifier))
        {
            return Ok(cfg.clone());
        }
        let cfg = Arc::new(build(
            self.chain.clone(),
            self.key.clone_key(),
            self.min_version,
            Some(verifier.clone()),
        )?);
        // Варианты закрытых слушателей и прежний верификатор этого слушателя (до перечитывания CA и CRL) не нужны.
        variants.retain(|(o, _, _)| o.strong_count() > 0 && !o.ptr_eq(&owner));
        variants.push((owner, verifier, cfg.clone()));
        Ok(cfg)
    }
}

/// Проверка необязательного сертификата клиента слушателя по доверенным CA (mTLS, PLT-22) и спискам отзыва
/// (PLT-28): сертификат от этих CA принимается, от чужого или отозванный — отказ рукопожатия, без сертификата —
/// соединение принимается. Файлы перечитываются [`TlsCerts`], в котором проверка учтена ([`TlsCerts::watch_client`]).
#[derive(Clone)]
pub(crate) struct ClientAuth {
    files: Arc<ClientFiles>,
}

/// Файлы проверки сертификата клиента и построенный по ним верификатор.
struct ClientFiles {
    ca_file: PathBuf,
    crl_files: Vec<PathBuf>,
    state: RwLock<ClientState>,
}

#[derive(Clone)]
struct ClientState {
    verifier: Arc<dyn ClientCertVerifier>,
    /// Отметки файла CA и файлов CRL.
    stamp: Vec<FileStamp>,
}

impl fmt::Debug for ClientAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClientAuth")
            .field("ca_file", &self.files.ca_file)
            .field("crl_files", &self.files.crl_files)
            .finish_non_exhaustive()
    }
}

impl ClientAuth {
    /// Доверенные CA из PEM-файла `ca_file` (один или несколько сертификатов) и списки отзыва `crl_files` (PEM
    /// или DER). Блокирующий I/O: при открытии слушателя (запуск), как и привязка сокета.
    pub(crate) fn load(ca_file: &Path, crl_files: &[PathBuf]) -> Result<Self, TlsError> {
        let state = read_client(ca_file, crl_files)?;
        Ok(Self {
            files: Arc::new(ClientFiles {
                ca_file: ca_file.to_owned(),
                crl_files: crl_files.to_vec(),
                state: RwLock::new(state),
            }),
        })
    }
}

impl ClientFiles {
    fn state(&self) -> ClientState {
        self.state
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn paths(&self) -> impl Iterator<Item = &PathBuf> {
        std::iter::once(&self.ca_file).chain(&self.crl_files)
    }

    /// Перечитывает CA и CRL: безусловно (`force`) или при изменившихся отметках файлов. `true` — верификатор
    /// заменён; ошибка оставляет прежний. Блокирующий I/O.
    fn reload(&self, force: bool) -> Result<bool, TlsError> {
        if !force && client_stamp(&self.ca_file, &self.crl_files) == self.state().stamp {
            return Ok(false);
        }
        let state = read_client(&self.ca_file, &self.crl_files)?;
        *self.state.write().unwrap_or_else(PoisonError::into_inner) = state;
        Ok(true)
    }
}

fn client_stamp(ca_file: &Path, crl_files: &[PathBuf]) -> Vec<FileStamp> {
    std::iter::once(ca_file)
        .chain(crl_files.iter().map(PathBuf::as_path))
        .map(file_stamp)
        .collect()
}

/// Верификатор сертификата клиента по файлам CA и CRL. Блокирующий I/O.
fn read_client(ca_file: &Path, crl_files: &[PathBuf]) -> Result<ClientState, TlsError> {
    let bad = |reason: String| TlsError::ClientCa {
        path: ca_file.to_owned(),
        reason,
    };
    // Отметка снимается до чтения: изменение во время чтения будет замечено следующим опросом.
    let stamp = client_stamp(ca_file, crl_files);
    let mut roots = RootCertStore::empty();
    for ca in read_chain(ca_file)? {
        roots.add(ca).map_err(|e| bad(e.to_string()))?;
    }
    let mut crls = Vec::new();
    for path in crl_files {
        crls.extend(read_crls(path)?);
    }
    let mut builder = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider())
        .allow_unauthenticated();
    if !crls.is_empty() {
        // Список есть не у каждого CA файла: сертификат CA без списка принимается, отозванный по списку — нет.
        builder = builder.with_crls(crls).allow_unknown_revocation_status();
    }
    let verifier = builder.build().map_err(|e| bad(e.to_string()))?;
    Ok(ClientState { verifier, stamp })
}

/// Списки отзыва из файла: PEM (один или несколько) либо один список в DER. Блокирующий I/O.
fn read_crls(path: &Path) -> Result<Vec<CertificateRevocationListDer<'static>>, TlsError> {
    let bytes = std::fs::read(path).map_err(|source| TlsError::Io {
        path: path.to_owned(),
        source,
    })?;
    let bad = |reason: String| TlsError::ClientCrl {
        path: path.to_owned(),
        reason,
    };
    let pem = rustls_pemfile::crls(&mut bytes.as_slice())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| bad(e.to_string()))?;
    if !pem.is_empty() {
        return Ok(pem);
    }
    // DER — SEQUENCE: первый байт 0x30. Содержимое разбирает верификатор.
    if bytes.first() != Some(&0x30) {
        return Err(bad("no CRL found".to_owned()));
    }
    Ok(vec![CertificateRevocationListDer::from(bytes)])
}

impl fmt::Debug for TlsCerts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
        f.debug_struct("TlsCerts")
            .field("cert_file", &files.cfg.cert_file)
            .field("min_version", &files.cfg.min_version)
            .finish_non_exhaustive()
    }
}

impl TlsCerts {
    /// Загружает сертификаты по `cfg` (чтение файлов — в `spawn_blocking`). Ключ по ссылке на секрет (`key`)
    /// требует хранилища — [`TlsCerts::load_with_secrets`], здесь — `Invalid`.
    pub async fn load(cfg: &TlsConfig) -> tp_contracts::Result<Self> {
        Self::load_from(cfg, None).await
    }

    /// Как [`TlsCerts::load`], с хранилищем секретов `store` для ключа по ссылке `key`; оно же используется при
    /// перечитывании. Секция с ключом в файле хранилище не читает.
    pub async fn load_with_secrets(
        cfg: &TlsConfig,
        store: Arc<dyn SecretStore>,
    ) -> tp_contracts::Result<Self> {
        Self::load_from(cfg, Some(store)).await
    }

    async fn load_from(
        cfg: &TlsConfig,
        store: Option<Arc<dyn SecretStore>>,
    ) -> tp_contracts::Result<Self> {
        let key = fetch_key(cfg, store.as_deref()).await?;
        let (loaded, source) = read_source(cfg.clone(), key).await?;
        Ok(Self {
            current: RwLock::new(Arc::new(loaded)),
            files: Mutex::new(source),
            store,
            clients: Mutex::new(Vec::new()),
        })
    }

    /// Текущая конфигурация rustls (без сертификата клиента).
    pub fn server_config(&self) -> Arc<ServerConfig> {
        self.loaded().plain.clone()
    }

    /// Текущие цепочка (сертификат узла первым) и ключ сервера — те же, что у слушателей, включая перечитанные
    /// после ACME, события ОС и SIGHUP. Ключ — [`rustls::sign::SigningKey`]: подписывает, но закрытый ключ не
    /// выдаёт. Вызывать на каждую подпись: после перечитывания отдаётся новая пара.
    pub fn certified_key(&self) -> Arc<CertifiedKey> {
        self.loaded().certified.clone()
    }

    fn loaded(&self) -> Arc<Loaded> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Акцептор на текущей конфигурации.
    pub fn acceptor(&self) -> TlsAcceptor {
        TlsAcceptor::from(self.server_config())
    }

    /// TLS-рукопожатие поверх принятого соединения (неявный TLS или STARTTLS).
    pub async fn accept(&self, tcp: TcpStream) -> std::io::Result<TlsStream<TcpStream>> {
        self.acceptor().accept(tcp).await
    }

    /// TLS-рукопожатие с запросом необязательного сертификата клиента по `auth` (без `auth` — как
    /// [`TlsCerts::accept`]).
    pub(crate) async fn accept_with(
        &self,
        tcp: TcpStream,
        auth: Option<&ClientAuth>,
    ) -> std::io::Result<TlsStream<TcpStream>> {
        let cfg = match auth {
            Some(auth) => self
                .loaded()
                .for_client(auth)
                .map_err(std::io::Error::other)?,
            None => self.server_config(),
        };
        TlsAcceptor::from(cfg).accept(tcp).await
    }

    /// Берёт файлы CA и CRL проверки `auth` на перечитывание вместе с сертификатами (до закрытия слушателя).
    pub(crate) fn watch_client(&self, auth: &ClientAuth) {
        let mut clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        clients.retain(|c| c.strong_count() > 0);
        clients.push(Arc::downgrade(&auth.files));
    }

    /// Проверки сертификата клиента открытых слушателей.
    fn clients(&self) -> Vec<Arc<ClientFiles>> {
        let clients = self.clients.lock().unwrap_or_else(PoisonError::into_inner);
        clients.iter().filter_map(Weak::upgrade).collect()
    }

    /// Перечитывает файлы CA и CRL слушателей: безусловно (`force`) или изменившиеся. `true` — что-то заменено.
    /// Ошибка одного слушателя не мешает остальным; возвращается первая.
    async fn reload_clients(&self, force: bool) -> tp_contracts::Result<bool> {
        let clients = self.clients();
        if clients.is_empty() {
            return Ok(false);
        }
        let read = tokio::task::spawn_blocking(move || {
            let mut changed = false;
            let mut failed = None;
            for client in clients {
                match client.reload(force) {
                    Ok(done) => changed |= done,
                    Err(e) => failed = failed.or(Some(e)),
                }
            }
            failed.map_or(Ok(changed), Err)
        });
        Ok(read.await.map_err(|_| TlsError::Join)??)
    }

    /// Перечитывает файлы текущей секции и файлы CA и CRL слушателей безусловно (например, после выпуска
    /// сертификата ACME).
    pub async fn reload(&self) -> tp_contracts::Result<()> {
        let cfg = self.config();
        let pair = self.reload_with(cfg).await;
        let clients = self.reload_clients(true).await;
        pair.and(clients.map(|_| ()))
    }

    /// Применяет новую версию секции: пути и версия TLS. Ошибка — прежние сертификаты остаются.
    pub async fn apply(&self, cfg: &TlsConfig) -> tp_contracts::Result<()> {
        if *cfg == self.config() {
            return Ok(());
        }
        self.reload_with(cfg.clone()).await
    }

    /// Перечитывает пару, если изменились mtime или размер файлов либо значение секрета ключа (новая версия), и
    /// изменившиеся файлы CA и CRL слушателей. `true` — сертификаты, CA или списки отзыва заменены.
    pub async fn reload_if_changed(&self) -> tp_contracts::Result<bool> {
        let pair = self.reload_pair_if_changed().await;
        let clients = self.reload_clients(false).await;
        Ok(pair? | clients?)
    }

    async fn reload_pair_if_changed(&self) -> tp_contracts::Result<bool> {
        let old = self
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let probe = old.cfg.clone();
        let stamp = tokio::task::spawn_blocking(move || stamp(&probe))
            .await
            .map_err(|_| TlsError::Join)?;
        let key = fetch_key(&old.cfg, self.store.as_deref()).await?;
        let same_key = match (&key, &old.key) {
            (Some(new), Some(old)) => new.expose() == old.expose(),
            (None, None) => true,
            _ => false,
        };
        if stamp == old.stamp && same_key {
            return Ok(false);
        }
        self.replace(old.cfg, key).await?;
        Ok(true)
    }

    /// Перечитывает файлы до сигнала остановки по событиям ОС ([`fswatch::os_events`]) и SIGHUP; без событий ОС —
    /// опрос с периодом `reload_interval`. Секрет ключа (`key`) проверяется с периодом `key_poll_interval`.
    /// Ошибки пишет в лог.
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        self.run_with(fswatch::os_events(), shutdown).await;
    }

    /// Как [`TlsCerts::run`] с источником событий `events`.
    pub async fn run_with(self: Arc<Self>, events: Arc<dyn FsEvents>, shutdown: CancellationToken) {
        let mut hup = Hup::install();
        let mut fs = DirWatch::new(events);
        loop {
            let cfg = self.config();
            let mut files = vec![cfg.cert_file.clone()];
            if cfg.key.is_none() {
                files.push(cfg.key_file.clone());
            }
            for client in self.clients() {
                files.extend(client.paths().cloned());
            }
            let dirs = tokio::task::spawn_blocking(move || {
                fswatch::dirs_of(&files.iter().map(PathBuf::as_path).collect::<Vec<_>>())
            })
            .await
            .unwrap_or_default();
            fs.ensure(dirs, "tls").await;
            // Файлы опрашиваются только без событий ОС, секрет ключа — всегда (событий о версии секрета нет).
            let nonzero = |d: Duration| (!d.is_zero()).then_some(d);
            let file_poll = nonzero(cfg.reload_interval).filter(|_| !fs.active());
            let key_poll = cfg.key.as_ref().and(nonzero(cfg.key_poll_interval));
            // Без событий ОС и с выключенным опросом — ждать SIGHUP, запроса или новой секции (опрос могут включить).
            let poll = match [file_poll, key_poll].into_iter().flatten().min() {
                Some(wait) => Some((wait, true)),
                None if fs.active() => None,
                None => Some((Duration::from_secs(60), false)),
            };
            let forced = tokio::select! {
                () = shutdown.cancelled() => return,
                () = hup.recv() => true,
                () = fs.changed() => false,
                () = sleep_or_never(poll.map(|(wait, _)| wait)) => {
                    if poll.is_some_and(|(_, check)| !check) {
                        continue;
                    }
                    false
                }
            };
            let result = if forced {
                self.reload().await.map(|()| true)
            } else {
                self.reload_if_changed().await
            };
            match result {
                Ok(true) => tracing::info!("tls certificates reloaded"),
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "tls reload failed, keeping previous certificates")
                }
            }
        }
    }

    fn config(&self) -> TlsConfig {
        self.files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cfg
            .clone()
    }

    async fn reload_with(&self, cfg: TlsConfig) -> tp_contracts::Result<()> {
        let key = fetch_key(&cfg, self.store.as_deref()).await?;
        self.replace(cfg, key).await
    }

    /// Читает пару по `cfg` с ключом `key` (значение секрета; `None` — ключ в файле) и делает её действующей.
    async fn replace(&self, cfg: TlsConfig, key: Option<Secret>) -> tp_contracts::Result<()> {
        let (loaded, source) = read_source(cfg, key).await?;
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(loaded);
        *self.files.lock().unwrap_or_else(PoisonError::into_inner) = source;
        Ok(())
    }
}

/// Значение секрета ключа секции; `None` — ключ в файле. Обращение к хранилищу — до чтения файлов, вне
/// `spawn_blocking`.
async fn fetch_key(
    cfg: &TlsConfig,
    store: Option<&dyn SecretStore>,
) -> tp_contracts::Result<Option<Secret>> {
    let Some(secret) = &cfg.key else {
        return Ok(None);
    };
    let store = store.ok_or_else(|| TlsError::NoStore(secret.clone()))?;
    store.get(secret).await.map(Some)
}

/// Читает и проверяет пару секции в `spawn_blocking`.
async fn read_source(
    cfg: TlsConfig,
    key: Option<Secret>,
) -> tp_contracts::Result<(Loaded, Source)> {
    let read = tokio::task::spawn_blocking(move || {
        read_config(&cfg, key.as_ref()).map(|(loaded, stamp)| (loaded, Source { cfg, stamp, key }))
    });
    Ok(read.await.map_err(|_| TlsError::Join)??)
}

/// Конфигурация rustls сервера из цепочки и ключа. Провайдер криптографии — установленный в процессе,
/// иначе aws-lc-rs.
pub fn server_config(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    min_version: TlsVersion,
) -> tp_contracts::Result<ServerConfig> {
    Ok(build(chain, key, min_version, None)?)
}

/// Ставит процессный [`CryptoProvider`] rustls (aws-lc-rs), если он ещё не задан. В сборке включены оба
/// провайдера (aws-lc-rs и ring), и без процессного клиенты, берущие его неявно (instant-acme, reqwest),
/// паникуют. Повторный вызов и гонка с другим потоком — не ошибка: используется уже установленный.
pub fn install_crypto_provider() {
    if CryptoProvider::get_default().is_none() {
        // Ошибка — провайдер успел поставить другой поток; используется он.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

/// Процессный провайдер криптографии, иначе aws-lc-rs.
fn provider() -> Arc<CryptoProvider> {
    CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()))
}

/// Конфигурация сервера; `client` — проверка сертификата клиента, `None` — сертификат не запрашивается.
fn build(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    min_version: TlsVersion,
    client: Option<Arc<dyn ClientCertVerifier>>,
) -> Result<ServerConfig, TlsError> {
    let versions: &[&rustls::SupportedProtocolVersion] = match min_version {
        TlsVersion::Tls13 => &[&rustls::version::TLS13],
        TlsVersion::Tls12 => &[&rustls::version::TLS13, &rustls::version::TLS12],
    };
    let builder =
        ServerConfig::builder_with_provider(provider()).with_protocol_versions(versions)?;
    let builder = match client {
        Some(verifier) => builder.with_client_cert_verifier(verifier),
        None => builder.with_no_client_auth(),
    };
    Ok(builder.with_single_cert(chain, key)?)
}

/// Читает и проверяет файлы секции; `secret` — значение секрета ключа (`key`). Блокирующий I/O.
fn read_config(cfg: &TlsConfig, secret: Option<&Secret>) -> Result<(Loaded, Stamp), TlsError> {
    // Отметка снимается до чтения: изменение во время чтения будет замечено следующим опросом.
    let stamp = stamp(cfg);
    let chain = read_chain(&cfg.cert_file)?;
    let key = match (&cfg.key, secret) {
        (Some(r), Some(pem)) => rustls_pemfile::private_key(&mut pem.expose().as_bytes())
            .ok()
            .flatten()
            .ok_or_else(|| TlsError::NoKeyInSecret(r.clone()))?,
        (Some(r), None) => return Err(TlsError::NoStore(r.clone())),
        (None, _) => read_key(&cfg.key_file)?,
    };
    Ok((Loaded::new(chain, key, cfg.min_version)?, stamp))
}

/// Цепочка сертификатов из PEM-файла. Блокирующий I/O.
pub(crate) fn read_chain(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let io = |source| TlsError::Io {
        path: path.to_owned(),
        source,
    };
    let mut reader = BufReader::new(File::open(path).map_err(io)?);
    let chain = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(io)?;
    if chain.is_empty() {
        return Err(TlsError::NoCert(path.to_owned()));
    }
    Ok(chain)
}

pub(crate) fn read_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let io = |source| TlsError::Io {
        path: path.to_owned(),
        source,
    };
    let mut reader = BufReader::new(File::open(path).map_err(io)?);
    rustls_pemfile::private_key(&mut reader)
        .map_err(io)?
        .ok_or_else(|| TlsError::NoKey(path.to_owned()))
}

async fn sleep_or_never(wait: Option<Duration>) {
    match wait {
        Some(wait) => tokio::time::sleep(wait).await,
        None => std::future::pending().await,
    }
}

fn file_stamp(path: &Path) -> FileStamp {
    match std::fs::metadata(path) {
        Ok(m) => (m.modified().ok(), m.len()),
        Err(_) => (None, 0),
    }
}

fn stamp(cfg: &TlsConfig) -> Stamp {
    let one = file_stamp;
    // Ключ из секрета: файл `key_file` не читается и на отметку не влияет.
    let key = match cfg.key {
        Some(_) => (None, 0),
        None => one(&cfg.key_file),
    };
    [one(&cfg.cert_file), key]
}

#[cfg(test)]
pub(crate) mod tests {
    use rustls::pki_types::ServerName;
    use rustls::{ClientConfig, RootCertStore};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsConnector;

    use super::*;
    use crate::fswatch::NoFsEvents;
    use crate::fswatch::tests::MemFsEvents;

    pub(crate) const CERT_A: &str = include_str!("testdata/cert-a.pem");
    pub(crate) const KEY_A: &str = include_str!("testdata/key-a.pem");
    pub(crate) const CERT_B: &str = include_str!("testdata/cert-b.pem");
    pub(crate) const KEY_B: &str = include_str!("testdata/key-b.pem");
    /// CA сертификатов клиента и выписанный им сертификат шлюза `gateway.example.test` (mTLS).
    pub(crate) const CA_CLIENT: &str = include_str!("testdata/ca-client.pem");
    pub(crate) const CERT_GW: &str = include_str!("testdata/cert-gw.pem");
    pub(crate) const KEY_GW: &str = include_str!("testdata/key-gw.pem");
    /// CA со списками отзыва (PLT-28): сертификаты клиентов `ok.example.test` и `revoked.example.test`, список
    /// без отозванных (`CRL_EMPTY`) и с отозванным `revoked.example.test` (`CRL_REVOKED`, он же в DER).
    pub(crate) const CA_CRL: &str = include_str!("testdata/ca-crl.pem");
    pub(crate) const CERT_CRL_OK: &str = include_str!("testdata/cert-crl-ok.pem");
    pub(crate) const KEY_CRL_OK: &str = include_str!("testdata/key-crl-ok.pem");
    pub(crate) const CERT_CRL_REVOKED: &str = include_str!("testdata/cert-crl-revoked.pem");
    pub(crate) const KEY_CRL_REVOKED: &str = include_str!("testdata/key-crl-revoked.pem");
    pub(crate) const CRL_EMPTY: &str = include_str!("testdata/crl-empty.pem");
    pub(crate) const CRL_REVOKED: &str = include_str!("testdata/crl-revoked.pem");
    pub(crate) const CRL_REVOKED_DER: &[u8] = include_bytes!("testdata/crl-revoked.der");

    /// Временный каталог теста; удаляется при drop.
    pub(crate) struct TempDir(pub(crate) PathBuf);

    impl TempDir {
        pub(crate) fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "tp-platform-{tag}-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("temp dir");
            Self(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    pub(crate) fn der(pem: &str) -> CertificateDer<'static> {
        rustls_pemfile::certs(&mut pem.as_bytes())
            .next()
            .expect("cert")
            .expect("pem")
    }

    /// Клиент, доверяющий самоподписанным `certs`, с версиями `versions`.
    pub(crate) fn connector(
        certs: &[&str],
        versions: &[&'static rustls::SupportedProtocolVersion],
    ) -> TlsConnector {
        let mut roots = RootCertStore::empty();
        for pem in certs {
            roots.add(der(pem)).expect("root");
        }
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let cfg = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .expect("versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        TlsConnector::from(Arc::new(cfg))
    }

    pub(crate) fn write_pair(dir: &Path, cert: &str, key: &str) -> TlsConfig {
        let cfg = TlsConfig {
            cert_file: dir.join("cert.pem"),
            key_file: dir.join("key.pem"),
            ..TlsConfig::default()
        };
        std::fs::write(&cfg.cert_file, cert).expect("write cert");
        std::fs::write(&cfg.key_file, key).expect("write key");
        cfg
    }

    /// Рукопожатие с сервером на `certs`: версия протокола и сертификат сервера.
    async fn handshake(
        certs: &TlsCerts,
        client: TlsConnector,
    ) -> std::io::Result<(rustls::ProtocolVersion, CertificateDer<'static>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let acceptor = certs.acceptor();
        let server = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.expect("accept");
            let mut tls = acceptor.accept(tcp).await?;
            let mut buf = [0u8; 4];
            tls.read_exact(&mut buf).await?;
            tls.write_all(b"pong").await?;
            tls.shutdown().await
        });
        let tcp = TcpStream::connect(addr).await.expect("connect");
        let name = ServerName::try_from("localhost").expect("name");
        let mut tls = client.connect(name, tcp).await?;
        tls.write_all(b"ping").await?;
        let mut buf = [0u8; 4];
        tls.read_exact(&mut buf).await?;
        assert_eq!(&buf, b"pong");
        let (_, conn) = tls.get_ref();
        let version = conn.protocol_version().expect("version");
        let cert = conn.peer_certificates().expect("peer certs")[0].clone();
        let _ = server.await;
        Ok((version, cert))
    }

    #[tokio::test]
    async fn tls13_by_default_tls12_as_option() {
        let dir = TempDir::new("tls-versions");
        let cfg = write_pair(&dir.0, CERT_A, KEY_A);
        let certs = TlsCerts::load(&cfg).await.expect("load");
        let any = &[&rustls::version::TLS13, &rustls::version::TLS12];
        let (version, cert) = handshake(&certs, connector(&[CERT_A], any))
            .await
            .expect("tls13");
        assert_eq!(version, rustls::ProtocolVersion::TLSv1_3);
        assert_eq!(cert, der(CERT_A));
        let tls12_only = || connector(&[CERT_A], &[&rustls::version::TLS12]);
        assert!(handshake(&certs, tls12_only()).await.is_err());

        certs
            .apply(&TlsConfig {
                min_version: TlsVersion::Tls12,
                ..cfg
            })
            .await
            .expect("apply");
        let (version, _) = handshake(&certs, tls12_only()).await.expect("tls12");
        assert_eq!(version, rustls::ProtocolVersion::TLSv1_2);
    }

    #[tokio::test]
    async fn files_are_reread_without_restart() {
        let dir = TempDir::new("tls-reload");
        let cfg = write_pair(&dir.0, CERT_A, KEY_A);
        let certs = TlsCerts::load(&cfg).await.expect("load");
        assert!(!certs.reload_if_changed().await.expect("same"));

        // Повреждённый ключ: перечитывание отклонено, действуют прежние сертификаты.
        std::fs::write(&cfg.key_file, "garbage").expect("write");
        assert!(matches!(
            certs.reload_if_changed().await,
            Err(Error::Invalid(_))
        ));
        let both = || connector(&[CERT_A, CERT_B], &[&rustls::version::TLS13]);
        assert_eq!(handshake(&certs, both()).await.expect("old").1, der(CERT_A));

        write_pair(&dir.0, CERT_B, KEY_B);
        assert!(certs.reload_if_changed().await.expect("changed"));
        assert_eq!(handshake(&certs, both()).await.expect("new").1, der(CERT_B));
    }

    /// Подпись `msg` ключом `ck` проверяется открытым ключом его сертификата узла (ECDSA P-256 тестовых пар).
    fn signed_by_chain(ck: &CertifiedKey, msg: &[u8]) -> bool {
        use rustls::SignatureScheme;
        let scheme = SignatureScheme::ECDSA_NISTP256_SHA256;
        let Some(signer) = ck.key.choose_scheme(&[scheme]) else {
            return false;
        };
        let sig = signer.sign(msg).expect("sign");
        // Открытый ключ P-256 — последние 65 байт SubjectPublicKeyInfo сертификата (несжатая точка).
        let cert = rustls::server::ParsedCertificate::try_from(&ck.cert[0]).expect("parse cert");
        let spki = cert.subject_public_key_info();
        let point = &spki.as_ref()[spki.as_ref().len() - 65..];
        let algs = rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms;
        let (_, verifiers) = algs
            .mapping
            .iter()
            .find(|(s, _)| *s == scheme)
            .expect("p-256 verifier");
        verifiers[0].verify_signature(point, msg, &sig).is_ok()
    }

    #[tokio::test]
    async fn certified_key_follows_reload() {
        let dir = TempDir::new("tls-certified");
        let cfg = write_pair(&dir.0, CERT_A, KEY_A);
        let certs = TlsCerts::load(&cfg).await.expect("load");
        let a = certs.certified_key();
        assert_eq!(a.cert, vec![der(CERT_A)]);
        a.keys_match().expect("key matches cert a");
        assert!(signed_by_chain(&a, b"profile"));

        // Ошибка перечитывания — прежняя пара.
        std::fs::write(&cfg.key_file, "garbage").expect("write");
        assert!(certs.reload_if_changed().await.is_err());
        assert_eq!(certs.certified_key().cert, vec![der(CERT_A)]);

        write_pair(&dir.0, CERT_B, KEY_B);
        certs.reload().await.expect("reload");
        let b = certs.certified_key();
        assert_eq!(b.cert, vec![der(CERT_B)]);
        b.keys_match().expect("key matches cert b");
        assert!(signed_by_chain(&b, b"profile"));
        // Старая пара не подходит к новой цепочке.
        let mixed = CertifiedKey::new(b.cert.clone(), a.key.clone());
        assert!(mixed.keys_match().is_err());
        assert!(!signed_by_chain(&mixed, b"profile"));
    }

    /// Ждёт замены конфигурации rustls после `before`.
    async fn replaced(certs: &TlsCerts, before: &Arc<ServerConfig>) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while Arc::ptr_eq(&certs.server_config(), before) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("certificates replaced in time");
    }

    #[tokio::test]
    async fn file_event_reloads_without_waiting_for_poll() {
        let dir = TempDir::new("tls-event");
        let mut cfg = write_pair(&dir.0, CERT_A, KEY_A);
        cfg.reload_interval = Duration::from_secs(3600);
        let certs = Arc::new(TlsCerts::load(&cfg).await.expect("load"));
        let events = Arc::new(MemFsEvents::default());
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(certs.clone().run_with(events.clone(), shutdown.clone()));
        tokio::time::timeout(Duration::from_secs(5), async {
            while events.watches() == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("watch installed");

        let before = certs.server_config();
        write_pair(&dir.0, CERT_B, KEY_B);
        assert_eq!(events.fire(&dir.0), 1);
        replaced(&certs, &before).await;
        let both = connector(&[CERT_A, CERT_B], &[&rustls::version::TLS13]);
        assert_eq!(handshake(&certs, both).await.expect("new").1, der(CERT_B));

        shutdown.cancel();
        task.await.expect("join");
    }

    #[tokio::test]
    async fn poll_fallback_without_fs_events() {
        let dir = TempDir::new("tls-poll");
        let mut cfg = write_pair(&dir.0, CERT_A, KEY_A);
        cfg.reload_interval = Duration::from_millis(20);
        let certs = Arc::new(TlsCerts::load(&cfg).await.expect("load"));
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(
            certs
                .clone()
                .run_with(Arc::new(NoFsEvents), shutdown.clone()),
        );

        let before = certs.server_config();
        write_pair(&dir.0, CERT_B, KEY_B);
        replaced(&certs, &before).await;

        shutdown.cancel();
        task.await.expect("join");
    }

    fn key_ref() -> SecretRef {
        "thepost/tls#key".parse().expect("ref")
    }

    /// Секция с цепочкой в файле и ключом по ссылке на секрет; файла ключа нет.
    fn secret_key_cfg(dir: &Path, cert: &str) -> TlsConfig {
        let cfg = TlsConfig {
            cert_file: dir.join("cert.pem"),
            key_file: dir.join("absent-key.pem"),
            key: Some(key_ref()),
            ..TlsConfig::default()
        };
        std::fs::write(&cfg.cert_file, cert).expect("write cert");
        cfg
    }

    /// Сертификат сервера, который слушатель с неявным TLS на `certs` отдаёт клиенту.
    async fn listener_cert(certs: &Arc<TlsCerts>) -> CertificateDer<'static> {
        use crate::listen::{EndpointConfig, Listener};
        let mut endpoint = EndpointConfig::new("127.0.0.1:0".parse().expect("addr"));
        endpoint.implicit_tls = true;
        let tcp = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let at = tcp.local_addr().expect("addr");
        let l = Listener::from_tcp(tcp, &endpoint, Some(certs.clone())).expect("listener");
        let server = tokio::spawn(async move {
            let conn = l.accept().await.expect("accept").establish().await?;
            assert!(conn.info.tls);
            let mut stream = conn.stream;
            stream.write_all(b"pong").await.expect("pong");
            stream.shutdown().await.expect("shutdown");
            tp_contracts::Result::Ok(())
        });
        let tcp = TcpStream::connect(at).await.expect("connect");
        let name = ServerName::try_from("localhost").expect("name");
        let mut tls = connector(&[CERT_A, CERT_B], &[&rustls::version::TLS13])
            .connect(name, tcp)
            .await
            .expect("handshake");
        let mut buf = [0u8; 4];
        tls.read_exact(&mut buf).await.expect("pong");
        let cert = tls.get_ref().1.peer_certificates().expect("peer certs")[0].clone();
        server.await.expect("join").expect("establish");
        cert
    }

    #[tokio::test]
    async fn listener_starts_with_key_from_secret_store_and_follows_new_version() {
        let dir = TempDir::new("tls-secret-key");
        let cfg = secret_key_cfg(&dir.0, CERT_A);
        let store = Arc::new(tp_testkit::MemSecretStore::default());
        store.set(&key_ref(), KEY_A);
        // Без хранилища ключ по ссылке не загрузить; файл ключа не читается.
        let err = TlsCerts::load(&cfg).await.expect_err("no store");
        assert!(
            matches!(&err, Error::Invalid(m) if m.contains("thepost/tls#key")),
            "{err}"
        );
        let certs = Arc::new(
            TlsCerts::load_with_secrets(&cfg, store.clone())
                .await
                .expect("load"),
        );
        assert_eq!(listener_cert(&certs).await, der(CERT_A));
        assert!(!certs.reload_if_changed().await.expect("same version"));

        // Новая версия секрета раньше новой цепочки: ключ не подходит — действует прежняя пара.
        store.set(&key_ref(), KEY_B);
        assert!(matches!(
            certs.reload_if_changed().await,
            Err(Error::Invalid(_))
        ));
        assert_eq!(listener_cert(&certs).await, der(CERT_A));
        std::fs::write(&cfg.cert_file, CERT_B).expect("write cert");
        assert!(certs.reload_if_changed().await.expect("new version"));
        assert_eq!(listener_cert(&certs).await, der(CERT_B));
        certs.certified_key().keys_match().expect("pair b");

        // Не PEM в секрете и недоступное хранилище — прежняя пара остаётся.
        store.set(&key_ref(), "garbage");
        let err = certs.reload_if_changed().await.expect_err("garbage");
        assert!(
            matches!(&err, Error::Invalid(m) if !m.contains("garbage")),
            "{err}"
        );
        store.set(&key_ref(), KEY_B);
        store.seal();
        assert!(certs.reload().await.expect_err("sealed").is_retryable());
        store.unseal();
        assert_eq!(listener_cert(&certs).await, der(CERT_B));
    }

    #[tokio::test]
    async fn secret_version_is_polled_even_with_fs_events() {
        let dir = TempDir::new("tls-secret-poll");
        let mut cfg = secret_key_cfg(&dir.0, CERT_A);
        cfg.reload_interval = Duration::from_secs(3600);
        cfg.key_poll_interval = Duration::from_millis(20);
        let store = Arc::new(tp_testkit::MemSecretStore::default());
        store.set(&key_ref(), KEY_A);
        let certs = Arc::new(
            TlsCerts::load_with_secrets(&cfg, store.clone())
                .await
                .expect("load"),
        );
        let events = Arc::new(MemFsEvents::default());
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(certs.clone().run_with(events.clone(), shutdown.clone()));

        // Событий ОС о файлах нет: новую пару замечает только опрос секрета.
        let before = certs.server_config();
        std::fs::write(&cfg.cert_file, CERT_B).expect("write cert");
        store.set(&key_ref(), KEY_B);
        replaced(&certs, &before).await;
        assert_eq!(certs.certified_key().cert, vec![der(CERT_B)]);

        shutdown.cancel();
        task.await.expect("join");
    }

    #[tokio::test]
    async fn missing_or_mismatched_files_are_invalid() {
        let dir = TempDir::new("tls-invalid");
        let mut cfg = write_pair(&dir.0, CERT_A, KEY_B);
        assert!(matches!(TlsCerts::load(&cfg).await, Err(Error::Invalid(_))));
        cfg.cert_file = dir.0.join("absent.pem");
        let err = TlsCerts::load(&cfg).await.expect_err("absent");
        assert!(err.to_string().contains("absent.pem"), "{err}");
        let empty = dir.0.join("empty.pem");
        std::fs::write(&empty, "").expect("write");
        cfg.cert_file = empty;
        assert!(matches!(TlsCerts::load(&cfg).await, Err(Error::Invalid(_))));
    }

    #[test]
    fn config_from_yaml() {
        let cfg: TlsConfig =
            serde_yaml_ng::from_str("min_version: \"1.2\"\ncert_file: /x/c.pem\n").expect("parse");
        assert_eq!(cfg.min_version, TlsVersion::Tls12);
        assert_eq!(cfg.cert_file, PathBuf::from("/x/c.pem"));
        assert_eq!(cfg.key_file, TlsConfig::default().key_file);
        assert!(serde_yaml_ng::from_str::<TlsConfig>("min_version: \"1.1\"").is_err());
        assert_eq!(cfg.reload_interval, Duration::from_secs(60));
        let off: TlsConfig = serde_yaml_ng::from_str("reload_interval: 0s").expect("0s");
        assert!(off.reload_interval.is_zero());
        assert!(serde_yaml_ng::from_str::<TlsConfig>("reload_interval: 60000").is_err());
        assert!(serde_yaml_ng::from_str::<TlsConfig>("reload_interval_ms: 60000").is_err());
        assert_eq!(cfg.key, None);
        assert_eq!(cfg.key_poll_interval, Duration::from_secs(60));
        let secret: TlsConfig = serde_yaml_ng::from_str(
            "key: { secret: \"thepost/tls#key\" }\nkey_poll_interval: 5m\n",
        )
        .expect("secret key");
        assert_eq!(secret.key, Some(key_ref()));
        assert_eq!(secret.key_poll_interval, Duration::from_secs(300));
        // Ключ открытым текстом в конфигурации — ошибка, значение в неё не попадает.
        let err = serde_yaml_ng::from_str::<TlsConfig>("key: PRIVATE-KEY-TEXT").expect_err("plain");
        assert!(!err.to_string().contains("PRIVATE-KEY-TEXT"), "{err}");
    }
}
