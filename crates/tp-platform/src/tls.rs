//! TLS сервера (SRS 6.2): конфигурация rustls из PEM-файлов (цепочка и ключ) с перечитыванием без перезапуска —
//! по событию ОС в каталогах файлов (inotify, [`crate::fswatch`]; замена Secret через символическую ссылку), по
//! SIGHUP, по новой версии секции и по запросу (например, после выпуска ACME). Опрос mtime — только если событий
//! ОС нет (SRS 2.8).
//!
//! Закрытый ключ (PLT-24, PLT-31; решения 37, 100) — только из хранилища секретов OpenBao/Vault по ссылке
//! `key: { secret: "<путь>#<поле>" }` (PEM в поле секрета; на диск не пишется; по умолчанию `thepost/tls#key`).
//! Файла ключа нет: `key_file` в секции — ошибка разбора. Новая версия секрета подхватывается без перезапуска:
//! секрет перечитывается с периодом `key_poll_interval`, по SIGHUP, по событию файла цепочки и по запросу; ключ, не
//! подходящий к цепочке, отклоняется — действует прежняя пара, пока цепочка и ключ не совпадут.
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
    /// Закрытый ключ из хранилища секретов: `{ secret: "<путь>#<поле>" }`, в поле — PEM (PKCS#8, PKCS#1 или
    /// SEC1); по умолчанию `thepost/tls#key`. Встроенный ACME пишет выпущенный ключ сюда же.
    pub key: SecretRef,
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
            key: default_key(),
            min_version: TlsVersion::Tls13,
            reload_interval: Duration::from_secs(60),
            key_poll_interval: Duration::from_secs(60),
        }
    }
}

/// Ссылка на ключ TLS по умолчанию: `thepost/tls#key`.
fn default_key() -> SecretRef {
    SecretRef::new("thepost/tls", "key").expect("valid default secret reference")
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
    #[error("no private key in PEM found")]
    NoKeyInPem,
    #[error("tls.key: secret {0} not found in the secret store")]
    NoSecret(SecretRef),
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

/// Действующие сертификаты сервера. Разделяется слушателями через `Arc`; новые соединения
/// получают текущую конфигурацию, установленные соединения не затрагиваются.
pub struct TlsCerts {
    current: RwLock<Arc<Loaded>>,
    files: Mutex<Source>,
    store: Arc<dyn SecretStore>,
    /// Проверки сертификата клиента слушателей: их файлы CA и CRL перечитываются вместе с сертификатами.
    clients: Mutex<Vec<Weak<ClientFiles>>>,
}

/// Источник действующей пары: секция, отметка файла цепочки и значение секрета ключа (сравнение версий).
#[derive(Clone)]
struct Source {
    cfg: TlsConfig,
    stamp: FileStamp,
    key: Secret,
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
    /// Загружает цепочку из файла `cert_file` (чтение — в `spawn_blocking`) и ключ по ссылке `key` из хранилища
    /// секретов `store`; оно же читается при перечитывании.
    pub async fn load(cfg: &TlsConfig, store: Arc<dyn SecretStore>) -> tp_contracts::Result<Self> {
        let key = fetch_key(cfg, store.as_ref()).await?;
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

    /// Перечитывает пару, если изменились mtime или размер файла цепочки либо значение секрета ключа (новая
    /// версия), и изменившиеся файлы CA и CRL слушателей. `true` — сертификаты, CA или списки отзыва заменены.
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
        let key = fetch_key(&old.cfg, self.store.as_ref()).await?;
        if stamp == old.stamp && key.expose() == old.key.expose() {
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
            let key_poll = nonzero(cfg.key_poll_interval);
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
        let key = fetch_key(&cfg, self.store.as_ref()).await?;
        self.replace(cfg, key).await
    }

    /// Читает пару по `cfg` с ключом `key` (значение секрета) и делает её действующей.
    async fn replace(&self, cfg: TlsConfig, key: Secret) -> tp_contracts::Result<()> {
        let (loaded, source) = read_source(cfg, key).await?;
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(loaded);
        *self.files.lock().unwrap_or_else(PoisonError::into_inner) = source;
        Ok(())
    }
}

/// Значение секрета ключа секции; секрета нет — `Invalid` со ссылкой (ссылка по умолчанию в config.yaml не видна).
/// Обращение к хранилищу — до чтения файлов, вне `spawn_blocking`.
async fn fetch_key(cfg: &TlsConfig, store: &dyn SecretStore) -> tp_contracts::Result<Secret> {
    match store.get(&cfg.key).await {
        Err(Error::NotFound) => Err(TlsError::NoSecret(cfg.key.clone()).into()),
        other => other,
    }
}

/// Читает и проверяет пару секции в `spawn_blocking`.
async fn read_source(cfg: TlsConfig, key: Secret) -> tp_contracts::Result<(Loaded, Source)> {
    let read = tokio::task::spawn_blocking(move || {
        read_config(&cfg, &key).map(|(loaded, stamp)| (loaded, Source { cfg, stamp, key }))
    });
    Ok(read.await.map_err(|_| TlsError::Join)??)
}

/// Подходит ли закрытый ключ `key` (PEM) к цепочке из файла `cert_file` — та же проверка, что при загрузке пары
/// слушателей; ошибка — `Invalid` без значения ключа. Для проверок `thepost doctor` ключей других каналов.
pub async fn check_pair(cert_file: &Path, key: &Secret) -> tp_contracts::Result<()> {
    let (cert_file, key) = (cert_file.to_owned(), key.clone());
    let check = tokio::task::spawn_blocking(move || {
        let chain = read_chain(&cert_file)?;
        let key = pem_key(&key).ok_or(TlsError::NoKeyInPem)?;
        build(chain, key, TlsVersion::Tls13, None).map(drop)
    });
    Ok(check.await.map_err(|_| TlsError::Join)??)
}

/// Закрытый ключ из PEM секрета; `None` — ключа в PEM нет.
fn pem_key(pem: &Secret) -> Option<PrivateKeyDer<'static>> {
    rustls_pemfile::private_key(&mut pem.expose().as_bytes())
        .ok()
        .flatten()
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

/// Читает цепочку секции и проверяет её с ключом `secret` (значение секрета `key`). Блокирующий I/O.
fn read_config(cfg: &TlsConfig, secret: &Secret) -> Result<(Loaded, FileStamp), TlsError> {
    // Отметка снимается до чтения: изменение во время чтения будет замечено следующим опросом.
    let stamp = stamp(cfg);
    let chain = read_chain(&cfg.cert_file)?;
    let key = pem_key(secret).ok_or_else(|| TlsError::NoKeyInSecret(cfg.key.clone()))?;
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

/// Отметка файла цепочки; версию ключа отличает значение секрета.
fn stamp(cfg: &TlsConfig) -> FileStamp {
    file_stamp(&cfg.cert_file)
}

#[cfg(test)]
pub(crate) mod tests;
