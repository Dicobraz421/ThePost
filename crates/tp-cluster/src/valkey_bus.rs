//! Шина уведомлений через Valkey pub/sub: Enterprise, несколько узлов (CONTRACTS.md#EventBus).
//!
//! Каналы: `<prefix>:acct:<tenant>:<account>` — уведомления учётной записи, `<prefix>:sys` — системные.
//! На узел одно pub/sub-соединение: фоновая задача подписывается на каналы, которые нужны
//! подписчикам этого узла (счётчик ссылок), и раздаёт пришедшее через [`LocalBus`] — поведение
//! подписок (буфер, `Lagged`, освобождение каналов) то же, что у шины в процессе.
//!
//! При потере связи задача завершает потоки всех подписчиков (они переподписываются и сверяются
//! по `ModSeq`) и переподключается с экспоненциальной задержкой. Пока связи нет, `subscribe`
//! и `publish` отвечают `Unavailable`. Публикация — отдельным мультиплексным соединением.
//!
//! Связь — только `rediss://` с проверкой сертификата сервера по CA из секции `tls` (CONTRACTS.md#Internal
//! TLS); файлы сертификатов перечитываются при каждом подключении. `redis://` — только профиль dev с
//! `tls.allow_plaintext`, с предупреждением в лог при запуске. Счётчики — [`BusCounters`].

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use redis::aio::{MultiplexedConnection, PubSubSink, PubSubStream};
use redis::{ClientTlsConfig, ConnectionAddr, ConnectionInfo, IntoConnectionInfo, TlsCertificates};
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tp_contracts::bus::{Event, EventBus, EventKind, EventStream};
use tp_contracts::config::duration;
use tp_contracts::meta::AccountScope;
use tp_contracts::secrets::{Secret, SecretRef, SecretStore};
use tp_contracts::tls::{InternalChannel, InternalTls, Profile, Transport};
use tp_contracts::{AccountId, Error, ModSeq, Result, TenantId};

use crate::local_bus::{LocalBus, LocalBusConfig};
use crate::metrics::{BusCounters, BusMetrics, inc};

/// Секция шины Valkey в config.yaml (`bus.valkey`). Незаданные поля — по умолчанию, неизвестные — ошибка
/// разбора. Запуск — [`ValkeyBus::connect`].
///
/// ```yaml
/// url: rediss://valkey.internal:6379
/// password: { secret: "thepost/valkey#password" }
/// tls: { ca_file: /etc/thepost/tls/ca.pem }
/// connect_timeout: 5s
/// ```
#[derive(Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ValkeyConfig {
    /// Адрес Valkey без пароля: `rediss://[пользователь@]узел:порт[/база]` (`redis://` — только профиль dev с
    /// `tls.allow_plaintext`; пароль в адресе — тоже только dev); в логи не попадает.
    pub url: String,
    /// Пароль — только ссылка на секрет в хранилище (`{ secret: "<путь>#<поле>" }`); значение читает
    /// [`ValkeyBus::connect`] при запуске.
    pub password: Option<SecretRef>,
    /// TLS канала: CA сервера, клиентский сертификат для mTLS, разрешение открытого канала в dev.
    pub tls: InternalTls,
    /// Префикс имён каналов: разные кластеры на одном Valkey не слышат друг друга.
    pub channel_prefix: String,
    /// Буфер уведомлений подписчика на узле (как `capacity` шины в процессе).
    pub buffer: usize,
    /// Таймаут установки соединения (`5s`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub connect_timeout: Duration,
    /// Таймаут команды (PUBLISH, SUBSCRIBE, PING).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub command_timeout: Duration,
    /// Начальная задержка переподключения (ноль считается за 1 мс); удваивается до `reconnect_max`.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub reconnect_min: Duration,
    /// Предельная задержка переподключения (не меньше `reconnect_min`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub reconnect_max: Duration,
    /// Период проверки pub/sub-соединения командой PING (ноль считается за 1 мс).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub ping_interval: Duration,
}

impl Default for ValkeyConfig {
    fn default() -> Self {
        Self {
            url: "rediss://127.0.0.1:6379".to_owned(),
            password: None,
            tls: InternalTls::default(),
            channel_prefix: "thepost".to_owned(),
            buffer: LocalBusConfig::default().capacity,
            connect_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_secs(2),
            reconnect_min: Duration::from_millis(100),
            reconnect_max: Duration::from_secs(10),
            ping_interval: Duration::from_secs(15),
        }
    }
}

impl fmt::Debug for ValkeyConfig {
    // Адрес в dev может содержать пароль — в журнал не попадает.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValkeyConfig")
            .field("url", &"<скрыт>")
            .field("password", &self.password)
            .field("tls", &self.tls)
            .field("channel_prefix", &self.channel_prefix)
            .field("buffer", &self.buffer)
            .field("connect_timeout", &self.connect_timeout)
            .field("command_timeout", &self.command_timeout)
            .field("reconnect_min", &self.reconnect_min)
            .field("reconnect_max", &self.reconnect_max)
            .field("ping_interval", &self.ping_interval)
            .finish()
    }
}

impl ValkeyConfig {
    /// Проверяет адрес, пароль и секцию TLS (`thepost config check` и запуск): `rediss://` с проверкой
    /// сертификата; `redis://` — только профиль dev с `tls.allow_plaintext`; пароль — ссылкой `password`
    /// (в адресе — только профиль dev и не вместе со ссылкой).
    pub fn check(&self, profile: Profile) -> Result<Transport> {
        let (info, transport) = self.parse(profile)?;
        if info.redis_settings().password().is_some() {
            if self.password.is_some() {
                return Err(Error::Invalid(
                    "шина Valkey: пароль задан и в адресе url, и ссылкой password".to_owned(),
                ));
            }
            if profile != Profile::Dev {
                return Err(Error::Invalid(
                    "шина Valkey: пароль в адресе url допустим только в профиле dev; \
                     задайте password: { secret: \"<путь>#<поле>\" }"
                        .to_owned(),
                ));
            }
        }
        Ok(transport)
    }

    /// Пароль из хранилища секретов по ссылке `password`; `None` — пароль не задан. Отказ хранилища —
    /// `Unavailable` (ссылка и причина — в журнал, значение — нет).
    pub async fn password(&self, secrets: &dyn SecretStore) -> Result<Option<Secret>> {
        let Some(secret) = &self.password else {
            return Ok(None);
        };
        secrets.get(secret).await.map(Some).map_err(|e| {
            tracing::warn!(%secret, error = %e, "шина Valkey: секрет password недоступен");
            match e {
                Error::Unavailable { retry_after } => Error::Unavailable { retry_after },
                _ => Error::UNAVAILABLE,
            }
        })
    }

    /// Адрес и TLS без правила о пароле (его проверяет [`Self::check`]).
    fn parse(&self, profile: Profile) -> Result<(ConnectionInfo, Transport)> {
        let transport = self
            .tls
            .check_url(InternalChannel::Valkey, &self.url, profile)?;
        let info = self
            .url
            .as_str()
            .into_connection_info()
            .map_err(|_| Error::Invalid("шина Valkey: неверный адрес url".to_owned()))?;
        if let ConnectionAddr::TcpTls { host, insecure, .. } = info.addr() {
            if *insecure {
                return Err(Error::Invalid(
                    "шина Valkey: #insecure в адресе отключает проверку сертификата".to_owned(),
                ));
            }
            // Клиент Valkey проверяет сертификат по узлу из адреса; другое имя не подставить.
            if self
                .tls
                .server_name
                .as_ref()
                .is_some_and(|name| name != host)
            {
                return Err(Error::Invalid(
                    "шина Valkey: tls.server_name должен совпадать с узлом в адресе url".to_owned(),
                ));
            }
        }
        Ok((info, transport))
    }

    fn reconnect_min(&self) -> Duration {
        self.reconnect_min.max(Duration::from_millis(1))
    }

    fn reconnect_max(&self) -> Duration {
        self.reconnect_max.max(self.reconnect_min())
    }

    fn ping_interval(&self) -> Duration {
        self.ping_interval.max(Duration::from_millis(1))
    }

    fn account_channel(&self, tenant: TenantId, account: AccountId) -> String {
        format!("{}:acct:{tenant}:{account}", self.channel_prefix)
    }

    fn system_channel(&self) -> String {
        format!("{}:sys", self.channel_prefix)
    }
}

/// Конфигурация шины Valkey прежнего вида: длительности в миллисекундах, пароль — в адресе.
///
/// Устарела: только для приложения до перехода на [`ValkeyConfig`] и [`ValkeyBus::connect`] (APP-07), затем
/// удаляется. Правило «пароль в адресе — только dev» здесь не действует: его проверяет секция приложения.
#[derive(Clone, PartialEq, Eq, JsonSchema)]
pub struct ValkeyBusConfig {
    /// Адрес Valkey (`rediss://[:пароль@]узел:порт[/база]`); в логи не попадает.
    pub url: String,
    /// TLS канала.
    pub tls: InternalTls,
    /// Префикс имён каналов.
    pub channel_prefix: String,
    /// Буфер уведомлений подписчика на узле.
    pub buffer: usize,
    /// Таймаут установки соединения, мс.
    pub connect_timeout_ms: u64,
    /// Таймаут команды, мс.
    pub command_timeout_ms: u64,
    /// Начальная задержка переподключения, мс.
    pub reconnect_min_ms: u64,
    /// Предельная задержка переподключения, мс.
    pub reconnect_max_ms: u64,
    /// Период проверки соединения PING, мс.
    pub ping_interval_ms: u64,
}

impl Default for ValkeyBusConfig {
    fn default() -> Self {
        let d = ValkeyConfig::default();
        let ms = |d: Duration| u64::try_from(d.as_millis()).unwrap_or(u64::MAX);
        Self {
            url: d.url,
            tls: d.tls,
            channel_prefix: d.channel_prefix,
            buffer: d.buffer,
            connect_timeout_ms: ms(d.connect_timeout),
            command_timeout_ms: ms(d.command_timeout),
            reconnect_min_ms: ms(d.reconnect_min),
            reconnect_max_ms: ms(d.reconnect_max),
            ping_interval_ms: ms(d.ping_interval),
        }
    }
}

impl fmt::Debug for ValkeyBusConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&ValkeyConfig::from(self.clone()), f)
    }
}

impl From<ValkeyBusConfig> for ValkeyConfig {
    fn from(c: ValkeyBusConfig) -> Self {
        Self {
            url: c.url,
            password: None,
            tls: c.tls,
            channel_prefix: c.channel_prefix,
            buffer: c.buffer,
            connect_timeout: Duration::from_millis(c.connect_timeout_ms),
            command_timeout: Duration::from_millis(c.command_timeout_ms),
            reconnect_min: Duration::from_millis(c.reconnect_min_ms),
            reconnect_max: Duration::from_millis(c.reconnect_max_ms),
            ping_interval: Duration::from_millis(c.ping_interval_ms),
        }
    }
}

impl ValkeyBusConfig {
    /// Проверяет адрес и секцию TLS: `rediss://` с проверкой сертификата; `redis://` — только профиль dev
    /// с `tls.allow_plaintext`.
    pub fn check(&self, profile: Profile) -> Result<Transport> {
        ValkeyConfig::from(self.clone())
            .parse(profile)
            .map(|(_, transport)| transport)
    }
}

/// Ошибка связи с Valkey.
#[derive(Debug, thiserror::Error)]
enum LinkError {
    #[error("таймаут")]
    Timeout,
    #[error("TLS: {0}")]
    Tls(String),
    #[error(transparent)]
    Redis(#[from] redis::RedisError),
}

async fn within<T>(
    limit: Duration,
    fut: impl Future<Output = redis::RedisResult<T>>,
) -> std::result::Result<T, LinkError> {
    Ok(tokio::time::timeout(limit, fut)
        .await
        .map_err(|_| LinkError::Timeout)??)
}

fn unavailable(retry_after: Option<Duration>) -> Error {
    Error::Unavailable { retry_after }
}

enum Cmd {
    /// Нужен канал; ответ — когда подписка в Valkey действует.
    Sub {
        channel: String,
        ack: oneshot::Sender<Result<()>>,
    },
    /// Подписчик канала ушёл.
    Unsub { channel: String },
}

/// Файлы TLS секции канала: CA сервера и клиентский сертификат. Блокирующий I/O.
fn read_certs(tls: &InternalTls) -> std::result::Result<TlsCertificates, String> {
    let read = |path: &Path, field: &str| {
        std::fs::read(path).map_err(|e| format!("tls.{field} {}: {e}", path.display()))
    };
    let root_cert = match tls.ca_file.as_deref() {
        Some(path) => {
            let pem = read(path, "ca_file")?;
            // Без сертификатов в файле проверка сервера не пройдёт ни разу — это ошибка настройки.
            if CertificateDer::pem_slice_iter(&pem).next().is_none() {
                return Err(format!(
                    "tls.ca_file {}: нет сертификатов PEM",
                    path.display()
                ));
            }
            Some(pem)
        }
        None => None,
    };
    let client_tls = match (tls.cert_file.as_deref(), tls.key_file.as_deref()) {
        (Some(cert), Some(key)) => Some(ClientTlsConfig {
            client_cert: read(cert, "cert_file")?,
            client_key: read(key, "key_file")?,
        }),
        _ => None,
    };
    Ok(TlsCertificates {
        client_tls,
        root_cert,
    })
}

/// Клиент Valkey; для TLS — с сертификатами из `certs`.
fn build_client(
    info: &ConnectionInfo,
    certs: Option<TlsCertificates>,
) -> redis::RedisResult<redis::Client> {
    match certs {
        Some(certs) => redis::Client::build_with_tls(info.clone(), certs),
        None => redis::Client::open(info.clone()),
    }
}

/// В сборке включены оба провайдера rustls (ring и aws-lc-rs) — без процессного провайдера
/// клиент TLS внутри redis не создаётся. Ставит тот же, что берёт платформа по умолчанию.
fn install_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // Ошибка — провайдер успел поставить другой поток; используется он.
        let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    }
}

struct Shared {
    config: ValkeyConfig,
    info: ConnectionInfo,
    transport: Transport,
    local: LocalBus,
    publisher: tokio::sync::Mutex<Option<MultiplexedConnection>>,
    counters: Arc<BusCounters>,
}

impl Shared {
    /// Клиент для нового подключения: файлы TLS перечитываются (ротация сертификатов).
    async fn client(&self) -> std::result::Result<redis::Client, LinkError> {
        let certs = match self.transport {
            Transport::Plaintext => None,
            Transport::Tls => {
                let tls = self.config.tls.clone();
                let certs = tokio::task::spawn_blocking(move || read_certs(&tls))
                    .await
                    .map_err(|_| LinkError::Tls("чтение файлов прервано".to_owned()))?
                    .map_err(LinkError::Tls)?;
                Some(certs)
            }
        };
        Ok(build_client(&self.info, certs)?)
    }

    async fn open_publisher(&self) -> std::result::Result<MultiplexedConnection, LinkError> {
        let client = self.client().await?;
        within(
            self.config.connect_timeout,
            client.get_multiplexed_async_connection(),
        )
        .await
    }

    async fn publisher(&self) -> Result<MultiplexedConnection> {
        let mut slot = self.publisher.lock().await;
        if let Some(conn) = slot.as_ref() {
            return Ok(conn.clone());
        }
        match self.open_publisher().await {
            Ok(conn) => {
                *slot = Some(conn.clone());
                Ok(conn)
            }
            Err(e) => {
                inc(&self.counters.publish_errors);
                tracing::warn!(error = %e, "шина Valkey: нет соединения для публикации");
                Err(unavailable(None))
            }
        }
    }

    async fn connect(
        &self,
        channels: &HashMap<String, usize>,
    ) -> std::result::Result<(PubSubSink, PubSubStream), LinkError> {
        let client = self.client().await?;
        let pubsub = within(self.config.connect_timeout, client.get_async_pubsub()).await?;
        let (mut sink, stream) = pubsub.split();
        for channel in channels.keys() {
            within(self.config.command_timeout, sink.subscribe(channel)).await?;
        }
        Ok((sink, stream))
    }

    fn on_message(&self, payload: &[u8]) {
        match decode(payload) {
            Some(event) => {
                inc(&self.counters.received);
                self.local.deliver(event);
            }
            None => {
                inc(&self.counters.decode_errors);
                tracing::debug!("шина Valkey: нераспознанное сообщение");
            }
        }
    }
}

/// Шина уведомлений через Valkey pub/sub.
pub struct ValkeyBus {
    shared: Arc<Shared>,
    cmds: mpsc::UnboundedSender<Cmd>,
    task: JoinHandle<()>,
}

impl fmt::Debug for ValkeyBus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValkeyBus")
            .field("config", &self.shared.config)
            .field("metrics", &self.metrics())
            .finish()
    }
}

impl Drop for ValkeyBus {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ValkeyBus {
    /// Запускает шину с паролем из хранилища секретов (ссылка `password`; секреты читаются при запуске).
    /// Ошибка конфигурации — `Invalid`, хранилище недоступно или нет секрета — `Unavailable`.
    /// `profile` — профиль узла (`profile` config.yaml). Вызывать внутри runtime tokio.
    pub async fn connect(
        config: ValkeyConfig,
        profile: Profile,
        secrets: &dyn SecretStore,
    ) -> Result<Self> {
        config.check(profile)?;
        let password = config.password(secrets).await?;
        Self::launch(config, profile, password.as_ref())
    }

    /// Запускает шину по конфигурации прежнего вида (пароль — в адресе). Устарело: до перехода
    /// приложения на [`Self::connect`] (APP-07). Вызывать внутри runtime tokio.
    pub fn start(config: ValkeyBusConfig, profile: Profile) -> Result<Self> {
        Self::launch(config.into(), profile, None)
    }

    /// Фоновая задача подключается к Valkey и держит подписки. `password` — значение по ссылке
    /// `password`: есть ссылка — должно быть и значение, и наоборот.
    fn launch(config: ValkeyConfig, profile: Profile, password: Option<&Secret>) -> Result<Self> {
        let (info, transport) = config.parse(profile)?;
        let info = match (&config.password, password) {
            (None, None) => info,
            (Some(_), Some(p)) => {
                let redis = info.redis_settings().clone().set_password(p.expose());
                info.set_redis_settings(redis)
            }
            (Some(_), None) => {
                return Err(Error::Invalid(
                    "шина Valkey: пароль по ссылке password не прочитан из хранилища".to_owned(),
                ));
            }
            (None, Some(_)) => {
                return Err(Error::Invalid(
                    "шина Valkey: пароль передан без ссылки password в конфигурации".to_owned(),
                ));
            }
        };
        match transport {
            Transport::Plaintext => {
                tracing::warn!("{}", InternalChannel::Valkey.plaintext_warning());
            }
            Transport::Tls => {
                install_crypto_provider();
                // Ошибка в файлах — ошибка конфигурации при запуске, а не бесконечные переподключения.
                let certs = read_certs(&config.tls)
                    .map_err(|e| Error::Invalid(format!("шина Valkey: {e}")))?;
                build_client(&info, Some(certs))
                    .map_err(|e| Error::Invalid(format!("шина Valkey: {e}")))?;
            }
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| Error::Internal("шина Valkey: запуск вне runtime tokio".to_owned()))?;
        let counters = Arc::new(BusCounters::default());
        let shared = Arc::new(Shared {
            local: LocalBus::with_counters(
                &LocalBusConfig {
                    capacity: config.buffer,
                },
                Arc::clone(&counters),
            ),
            config,
            info,
            transport,
            publisher: tokio::sync::Mutex::new(None),
            counters,
        });
        let (cmds, rx) = mpsc::unbounded_channel();
        let task = runtime.spawn(run(Arc::clone(&shared), rx));
        Ok(Self { shared, cmds, task })
    }

    /// Счётчики шины для метрик платформы.
    pub fn counters(&self) -> Arc<BusCounters> {
        Arc::clone(&self.shared.counters)
    }

    /// Снимок счётчиков.
    pub fn metrics(&self) -> BusMetrics {
        self.shared.counters.snapshot()
    }

    /// Подписывает канал в Valkey; поток `local` живёт, пока нужен канал.
    async fn attach(&self, channel: String, local: EventStream) -> Result<EventStream> {
        let (ack, acked) = oneshot::channel();
        self.cmds
            .send(Cmd::Sub {
                channel: channel.clone(),
                ack,
            })
            .map_err(|_| unavailable(None))?;
        // Задача может быть занята подключением — ждём его и саму подписку.
        let wait = self.shared.config.connect_timeout + self.shared.config.command_timeout;
        match tokio::time::timeout(wait, acked).await {
            Ok(Ok(Ok(()))) => Ok(Guarded {
                inner: local,
                _release: Release {
                    cmds: self.cmds.clone(),
                    channel,
                },
            }
            .boxed()),
            Ok(Ok(Err(e))) => Err(e),
            // Задача остановлена или не успела: если подписка всё же пройдёт, ответ не дойдёт
            // и задача сама снимет её.
            Ok(Err(_)) | Err(_) => Err(unavailable(None)),
        }
    }
}

#[async_trait]
impl EventBus for ValkeyBus {
    async fn publish(&self, event: Event) -> Result<()> {
        let config = &self.shared.config;
        let channel = match event.account {
            Some(account) => config.account_channel(event.tenant, account),
            None => config.system_channel(),
        };
        let payload = encode(&event);
        let mut conn = self.shared.publisher().await?;
        let sent = within(
            config.command_timeout,
            redis::cmd("PUBLISH")
                .arg(&channel)
                .arg(&payload)
                .query_async::<i64>(&mut conn),
        )
        .await;
        match sent {
            Ok(_) => {
                inc(&self.shared.counters.published);
                Ok(())
            }
            Err(e) => {
                *self.shared.publisher.lock().await = None;
                inc(&self.shared.counters.publish_errors);
                tracing::warn!(error = %e, "шина Valkey: публикация не прошла");
                Err(unavailable(None))
            }
        }
    }

    async fn subscribe(&self, scope: AccountScope) -> Result<EventStream> {
        // Сначала локальная подписка: уведомление, пришедшее сразу после SUBSCRIBE, не потеряется.
        let local = self.shared.local.subscribe_account(scope);
        let channel = self
            .shared
            .config
            .account_channel(scope.tenant, scope.account);
        self.attach(channel, local).await
    }

    async fn subscribe_system(&self) -> Result<EventStream> {
        let local = self.shared.local.subscribe_system_now();
        self.attach(self.shared.config.system_channel(), local)
            .await
    }
}

/// Поток подписчика; при удалении сообщает задаче, что канал ему больше не нужен.
struct Guarded {
    inner: EventStream,
    _release: Release,
}

impl Stream for Guarded {
    type Item = Event;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Event>> {
        self.inner.poll_next_unpin(cx)
    }
}

struct Release {
    cmds: mpsc::UnboundedSender<Cmd>,
    channel: String,
}

impl Drop for Release {
    fn drop(&mut self) {
        let channel = std::mem::take(&mut self.channel);
        // Ошибка — задача остановлена вместе с шиной; снимать нечего.
        let _ = self.cmds.send(Cmd::Unsub { channel });
    }
}

/// Уменьшает счётчик канала; `true`, если канал больше никому не нужен.
fn release(channels: &mut HashMap<String, usize>, channel: &str) -> bool {
    match channels.get_mut(channel) {
        Some(n) if *n > 1 => {
            *n -= 1;
            false
        }
        Some(_) => {
            channels.remove(channel);
            true
        }
        None => false,
    }
}

/// Фоновая задача: соединение, подписки, переподключение.
async fn run(shared: Arc<Shared>, mut cmds: mpsc::UnboundedReceiver<Cmd>) {
    let mut channels: HashMap<String, usize> = HashMap::new();
    let mut backoff = shared.config.reconnect_min();
    loop {
        match shared.connect(&channels).await {
            Ok((sink, stream)) => {
                shared.counters.set_connected(true);
                tracing::info!("шина Valkey: соединение установлено");
                backoff = shared.config.reconnect_min();
                let stop = serve(&shared, &mut cmds, &mut channels, sink, stream).await;
                shared.counters.set_connected(false);
                // Уведомления за время разрыва потеряны — подписчики сверятся по ModSeq.
                shared.local.close_all();
                if stop {
                    return;
                }
                inc(&shared.counters.reconnects);
            }
            Err(e) => tracing::warn!(error = %e, "шина Valkey: нет связи"),
        }

        let deadline = Instant::now() + backoff;
        loop {
            tokio::select! {
                () = tokio::time::sleep_until(deadline) => break,
                cmd = cmds.recv() => match cmd {
                    None => return,
                    Some(Cmd::Sub { ack, .. }) => {
                        let _ = ack.send(Err(unavailable(Some(backoff))));
                    }
                    Some(Cmd::Unsub { channel }) => {
                        release(&mut channels, &channel);
                    }
                },
            }
        }
        backoff = (backoff * 2).min(shared.config.reconnect_max());
    }
}

/// Обслуживает соединение; `true` — шина удалена и задача завершается, `false` — связь потеряна.
async fn serve(
    shared: &Shared,
    cmds: &mut mpsc::UnboundedReceiver<Cmd>,
    channels: &mut HashMap<String, usize>,
    mut sink: PubSubSink,
    mut stream: PubSubStream,
) -> bool {
    let timeout = shared.config.command_timeout;
    let mut ping = tokio::time::interval_at(
        Instant::now() + shared.config.ping_interval(),
        shared.config.ping_interval(),
    );
    ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(msg) => shared.on_message(msg.get_payload_bytes()),
                None => {
                    tracing::warn!("шина Valkey: соединение закрыто");
                    return false;
                }
            },
            cmd = cmds.recv() => match cmd {
                None => return true,
                Some(Cmd::Sub { channel, ack }) => {
                    if let Some(n) = channels.get_mut(&channel) {
                        *n += 1;
                    } else if let Err(e) = within(timeout, sink.subscribe(&channel)).await {
                        tracing::warn!(error = %e, "шина Valkey: SUBSCRIBE не прошёл");
                        let _ = ack.send(Err(unavailable(None)));
                        return false;
                    } else {
                        channels.insert(channel.clone(), 1);
                    }
                    // Подписчик не дождался ответа — канал ему не нужен.
                    if ack.send(Ok(())).is_err()
                        && release(channels, &channel)
                        && let Err(e) = within(timeout, sink.unsubscribe(&channel)).await
                    {
                        tracing::warn!(error = %e, "шина Valkey: UNSUBSCRIBE не прошёл");
                        return false;
                    }
                }
                Some(Cmd::Unsub { channel }) => {
                    if release(channels, &channel)
                        && let Err(e) = within(timeout, sink.unsubscribe(&channel)).await
                    {
                        tracing::warn!(error = %e, "шина Valkey: UNSUBSCRIBE не прошёл");
                        return false;
                    }
                }
            },
            _ = ping.tick() => {
                if let Err(e) = within(timeout, sink.ping::<redis::Value>()).await {
                    tracing::warn!(error = %e, "шина Valkey: PING не прошёл");
                    return false;
                }
            }
        }
    }
}

/// Версия формата сообщения в канале.
const WIRE_VERSION: &str = "1";

/// Сообщение: `1|<tenant>|<account или ->|<вид>`, вид — `mc:<mailbox>:<modseq>`, `ml`, `qr`, `cr`.
fn encode(event: &Event) -> String {
    let account = event
        .account
        .map_or_else(|| "-".to_owned(), |a| a.to_string());
    let kind = match &event.kind {
        EventKind::MailboxChanged { mailbox, modseq } => format!("mc:{mailbox}:{}", modseq.0),
        EventKind::MailboxListChanged => "ml".to_owned(),
        EventKind::QueueReady => "qr".to_owned(),
        EventKind::ConfigReloaded => "cr".to_owned(),
    };
    format!("{WIRE_VERSION}|{}|{account}|{kind}", event.tenant)
}

fn decode(payload: &[u8]) -> Option<Event> {
    let text = std::str::from_utf8(payload).ok()?;
    let mut parts = text.split('|');
    if parts.next()? != WIRE_VERSION {
        return None;
    }
    let tenant = parts.next()?.parse().ok()?;
    let account = match parts.next()? {
        "-" => None,
        a => Some(a.parse().ok()?),
    };
    let kind = match parts.next()?.split(':').collect::<Vec<_>>().as_slice() {
        ["mc", mailbox, modseq] => EventKind::MailboxChanged {
            mailbox: mailbox.parse().ok()?,
            modseq: ModSeq(modseq.parse().ok()?),
        },
        ["ml"] => EventKind::MailboxListChanged,
        ["qr"] => EventKind::QueueReady,
        ["cr"] => EventKind::ConfigReloaded,
        _ => return None,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(Event {
        tenant,
        account,
        kind,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance;
    use std::sync::Mutex;
    use tokio::net::{TcpListener, TcpStream};

    fn dev_plaintext() -> InternalTls {
        InternalTls {
            allow_plaintext: true,
            ..InternalTls::default()
        }
    }

    fn valkey_url() -> Option<String> {
        std::env::var("TEST_VALKEY_URL").ok()
    }

    /// Свой префикс на тест: тесты идут параллельно на одном Valkey. Valkey в CI — без TLS (профиль dev).
    fn config(url: String) -> ValkeyConfig {
        ValkeyConfig {
            url,
            tls: dev_plaintext(),
            channel_prefix: format!("tp-test-{}", AccountId::new()),
            connect_timeout: Duration::from_secs(1),
            command_timeout: Duration::from_secs(1),
            reconnect_min: Duration::from_millis(20),
            reconnect_max: Duration::from_millis(200),
            ..ValkeyConfig::default()
        }
    }

    #[test]
    fn wire_roundtrip() {
        let scope = conformance::scope();
        let events = [
            conformance::mailbox_changed(scope, u64::MAX),
            Event {
                kind: EventKind::MailboxListChanged,
                ..conformance::mailbox_changed(scope, 0)
            },
            conformance::system(EventKind::QueueReady),
            conformance::system(EventKind::ConfigReloaded),
        ];
        for event in events {
            assert_eq!(decode(encode(&event).as_bytes()), Some(event));
        }
    }

    #[test]
    fn wire_rejects_garbage() {
        let good = encode(&conformance::mailbox_changed(conformance::scope(), 1));
        assert!(decode(good.as_bytes()).is_some());
        let bad = [
            String::new(),
            format!("2{}", &good[1..]),
            format!("{good}|x"),
            good.replace("|mc:", "|zz:"),
            good.replacen('|', "|-", 1),
            "1|x|-|qr".to_owned(),
        ];
        for b in bad {
            assert_eq!(decode(b.as_bytes()), None, "{b}");
        }
        assert_eq!(decode(&[0xff, 0xfe]), None);
    }

    #[test]
    fn channel_names() {
        let c = ValkeyConfig::default();
        let (t, a) = (TenantId::DEFAULT, AccountId::new());
        assert_eq!(c.account_channel(t, a), format!("thepost:acct:{t}:{a}"));
        assert_eq!(c.system_channel(), "thepost:sys");
    }

    #[test]
    fn legacy_config_converts() {
        let legacy = ValkeyBusConfig {
            url: "rediss://:secret@cache.internal:6379".to_owned(),
            connect_timeout_ms: 1_500,
            ..ValkeyBusConfig::default()
        };
        let c = ValkeyConfig::from(legacy.clone());
        assert_eq!(c.connect_timeout, Duration::from_millis(1_500));
        assert_eq!(c.password, None);
        assert_eq!(
            ValkeyConfig::from(ValkeyBusConfig::default()),
            ValkeyConfig::default()
        );
        // Прежний путь: пароль в адресе принимается и вне dev (его ограничивает секция приложения).
        assert_eq!(legacy.check(Profile::Prod).ok(), Some(Transport::Tls));
        assert!(!format!("{legacy:?}").contains("secret"));
        assert!(matches!(
            ValkeyBus::start(legacy, Profile::Prod),
            Err(Error::Internal(_))
        ));
    }

    #[test]
    fn debug_hides_url() {
        let c = ValkeyConfig {
            url: "redis://:secret@host:6379".to_owned(),
            ..ValkeyConfig::default()
        };
        assert!(!format!("{c:?}").contains("secret"));
    }

    #[test]
    fn rejects_bad_url_and_missing_runtime() {
        let bad = ValkeyConfig {
            url: "not a url".to_owned(),
            ..ValkeyConfig::default()
        };
        assert!(matches!(
            ValkeyBus::launch(bad, Profile::Dev, None),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            ValkeyBus::launch(ValkeyConfig::default(), Profile::Prod, None),
            Err(Error::Internal(_))
        ));
    }

    #[test]
    fn plaintext_only_in_dev() {
        let open = ValkeyConfig {
            url: "redis://cache:6379".to_owned(),
            ..ValkeyConfig::default()
        };
        // Без allow_plaintext адрес redis:// отклоняется в любом профиле.
        assert!(matches!(open.check(Profile::Prod), Err(Error::Invalid(_))));
        assert!(matches!(open.check(Profile::Dev), Err(Error::Invalid(_))));
        assert!(matches!(
            ValkeyBus::launch(open.clone(), Profile::Prod, None),
            Err(Error::Invalid(_))
        ));

        let dev = ValkeyConfig {
            tls: dev_plaintext(),
            ..open
        };
        assert_eq!(dev.check(Profile::Dev).ok(), Some(Transport::Plaintext));
        // allow_plaintext вне dev — ошибка конфигурации, даже с адресом rediss://.
        assert!(matches!(dev.check(Profile::Prod), Err(Error::Invalid(_))));
        let tls_dev = ValkeyConfig {
            url: "rediss://cache:6379".to_owned(),
            ..dev
        };
        assert!(matches!(
            tls_dev.check(Profile::Prod),
            Err(Error::Invalid(_))
        ));
        assert_eq!(tls_dev.check(Profile::Dev).ok(), Some(Transport::Tls));
    }

    #[test]
    fn tls_address_checks() {
        let tls = |url: &str, server_name: Option<&str>| ValkeyConfig {
            url: url.to_owned(),
            tls: InternalTls {
                server_name: server_name.map(str::to_owned),
                ..InternalTls::default()
            },
            ..ValkeyConfig::default()
        };
        for url in [
            "rediss://cache.internal:6379",
            "rediss://thepost@cache.internal:6380/2",
            "valkeys://cache.internal",
        ] {
            assert_eq!(
                tls(url, None).check(Profile::Prod).ok(),
                Some(Transport::Tls),
                "{url}"
            );
        }
        assert_eq!(
            tls("rediss://cache.internal", Some("cache.internal"))
                .check(Profile::Prod)
                .ok(),
            Some(Transport::Tls)
        );
        for (url, name) in [
            // Отключение проверки сертификата.
            ("rediss://cache.internal/#insecure", None),
            // Имя для проверки не совпадает с узлом в адресе.
            ("rediss://cache.internal", Some("other.internal")),
            ("redis+unix:///run/valkey.sock", None),
            ("https://cache.internal", None),
        ] {
            assert!(
                matches!(tls(url, name).check(Profile::Prod), Err(Error::Invalid(_))),
                "{url}"
            );
        }
    }

    fn secret_ref() -> SecretRef {
        SecretRef::new("thepost/valkey", "password").expect("ref")
    }

    #[test]
    fn password_only_by_reference() {
        let in_url = ValkeyConfig {
            url: "rediss://:hunter2@cache.internal:6379".to_owned(),
            ..ValkeyConfig::default()
        };
        // Пароль в адресе — только dev.
        assert!(matches!(
            in_url.check(Profile::Prod),
            Err(Error::Invalid(_))
        ));
        assert_eq!(in_url.check(Profile::Dev).ok(), Some(Transport::Tls));
        // И в адресе, и ссылкой — ошибка в любом профиле.
        let both = ValkeyConfig {
            password: Some(secret_ref()),
            ..in_url
        };
        assert!(matches!(both.check(Profile::Dev), Err(Error::Invalid(_))));
        let by_ref = ValkeyConfig {
            url: "rediss://thepost@cache.internal:6379".to_owned(),
            ..both
        };
        assert_eq!(by_ref.check(Profile::Prod).ok(), Some(Transport::Tls));
        // Ссылка без прочитанного значения и значение без ссылки — ошибка запуска.
        assert!(matches!(
            ValkeyBus::launch(by_ref, Profile::Prod, None),
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            ValkeyBus::launch(
                ValkeyConfig::default(),
                Profile::Prod,
                Some(&Secret::new("x"))
            ),
            Err(Error::Invalid(_))
        ));
    }

    #[test]
    fn parses_yaml_section() {
        let yaml = r#"
url: rediss://thepost@valkey.internal:6380/1
password: { secret: "thepost/valkey#password" }
tls:
  ca_file: /etc/thepost/tls/ca.pem
channel_prefix: mail-eu
connect_timeout: 3s
reconnect_min: 250ms
reconnect_max: 1m
"#;
        let c: ValkeyConfig = serde_yaml_ng::from_str(yaml).expect("yaml");
        let d = ValkeyConfig::default();
        assert_eq!(c.url, "rediss://thepost@valkey.internal:6380/1");
        assert_eq!(c.password, Some(secret_ref()));
        assert_eq!(
            c.tls.ca_file.as_deref(),
            Some(Path::new("/etc/thepost/tls/ca.pem"))
        );
        assert_eq!(c.channel_prefix, "mail-eu");
        assert_eq!(c.connect_timeout, Duration::from_secs(3));
        assert_eq!(c.reconnect_min, Duration::from_millis(250));
        assert_eq!(c.reconnect_max, Duration::from_secs(60));
        // Незаданные — по умолчанию.
        assert_eq!(
            (c.buffer, c.command_timeout, c.ping_interval),
            (d.buffer, d.command_timeout, d.ping_interval)
        );
        assert_eq!(c.check(Profile::Prod).ok(), Some(Transport::Tls));
        assert_eq!(
            serde_yaml_ng::from_str::<ValkeyConfig>("{}").expect("empty"),
            d
        );

        for bad in [
            // Пароль открытым текстом вместо ссылки.
            "password: hunter2",
            // Длительность без единицы и старое имя поля в миллисекундах.
            "connect_timeout: 5000",
            "connect_timeout_ms: 5000",
            "unknown: 1",
        ] {
            assert!(
                serde_yaml_ng::from_str::<ValkeyConfig>(bad).is_err(),
                "{bad}"
            );
        }
        let schema = schemars::schema_for!(ValkeyConfig).as_value().to_string();
        assert!(schema.contains("ping_interval"), "{schema}");
        assert!(!schema.contains("_ms"), "{schema}");
    }

    /// Сервер, отдающий первые байты от клиента и отвечающий ошибкой.
    async fn capture_server() -> (std::net::SocketAddr, mpsc::UnboundedReceiver<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut buf = vec![0; 4096];
                let n = conn.read(&mut buf).await.unwrap_or(0);
                buf.truncate(n);
                let _ = tx.send(buf);
                let _ = conn.write_all(b"-ERR nope\r\n").await;
            }
        });
        (addr, rx)
    }

    #[tokio::test]
    async fn connect_reads_password_from_store() {
        let (addr, mut seen) = capture_server().await;
        let store = tp_testkit::MemSecretStore::new(Arc::new(tp_testkit::SystemClock));
        let cfg = ValkeyConfig {
            password: Some(secret_ref()),
            ..config(format!("redis://{addr}"))
        };
        // Секрета ещё нет — Unavailable, к Valkey не подключаемся.
        assert!(matches!(
            ValkeyBus::connect(cfg.clone(), Profile::Dev, &store).await,
            Err(Error::Unavailable { .. })
        ));
        store.set(&secret_ref(), "s3cr3t-пароль");
        let bus = ValkeyBus::connect(cfg, Profile::Dev, &store)
            .await
            .expect("connect");
        assert!(!format!("{bus:?}").contains("s3cr3t"));
        let _ = bus.subscribe(conformance::scope()).await;
        let hello = tokio::time::timeout(Duration::from_secs(5), seen.recv())
            .await
            .expect("timeout")
            .expect("bytes");
        let hello = String::from_utf8_lossy(&hello);
        assert!(hello.contains("AUTH"), "{hello}");
        assert!(hello.contains("s3cr3t-пароль"), "{hello}");
    }

    #[tokio::test]
    async fn bad_tls_files_fail_start() {
        let dir = std::env::temp_dir().join(format!("tp-cluster-{}", AccountId::new()));
        std::fs::create_dir_all(&dir).expect("dir");
        let garbage = dir.join("ca.pem");
        std::fs::write(&garbage, "не сертификат").expect("write");
        for ca in [dir.join("missing.pem"), garbage] {
            let cfg = ValkeyConfig {
                tls: InternalTls {
                    ca_file: Some(ca.clone()),
                    ..InternalTls::default()
                },
                ..ValkeyConfig::default()
            };
            assert!(
                matches!(
                    ValkeyBus::launch(cfg, Profile::Prod, None),
                    Err(Error::Invalid(_))
                ),
                "{}",
                ca.display()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tls_to_plain_server_is_unavailable() {
        // Сервер без TLS: принимает соединение и отвечает не TLS — рукопожатие не проходит.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let server = tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                use tokio::io::AsyncWriteExt;
                let _ = conn.write_all(b"-ERR plaintext\r\n").await;
            }
        });
        let cfg = ValkeyConfig {
            url: format!("rediss://{addr}"),
            ..config(String::new())
        };
        let cfg = ValkeyConfig {
            tls: InternalTls::default(),
            ..cfg
        };
        let bus = ValkeyBus::launch(cfg, Profile::Prod, None).expect("start");
        assert!(matches!(
            bus.subscribe(conformance::scope()).await,
            Err(Error::Unavailable { .. })
        ));
        assert!(matches!(
            bus.publish(conformance::system(EventKind::QueueReady))
                .await,
            Err(Error::Unavailable { .. })
        ));
        let m = bus.metrics();
        assert!(!m.connected);
        assert_eq!((m.published, m.publish_errors), (0, 1));
        server.abort();
    }

    #[tokio::test]
    async fn unreachable_valkey_is_unavailable() {
        // Порт слушающего и сразу закрытого сокета: соединение отвергается.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let bus = ValkeyBus::launch(config(format!("redis://{addr}")), Profile::Dev, None)
            .expect("start");
        assert!(matches!(
            bus.subscribe(conformance::scope()).await,
            Err(Error::Unavailable { .. })
        ));
        assert!(matches!(
            bus.publish(conformance::system(EventKind::QueueReady))
                .await,
            Err(Error::Unavailable { .. })
        ));
        let m = bus.metrics();
        assert!(!m.connected);
        assert_eq!(m.publish_errors, 1);
    }

    #[tokio::test]
    async fn conformance_suite_valkey() {
        let Some(url) = valkey_url() else {
            eprintln!("TEST_VALKEY_URL не задан — пропуск");
            return;
        };
        let bus = ValkeyBus::launch(config(url), Profile::Dev, None).expect("start");
        conformance::run(&bus).await;
        let m = bus.metrics();
        assert!(m.connected);
        assert_eq!(m.decode_errors, 0);
        assert!(m.received > 0 && m.published >= m.received / 2);
        assert!(m.delivered > 0);
    }

    #[tokio::test]
    async fn delivers_between_nodes() {
        let Some(url) = valkey_url() else {
            eprintln!("TEST_VALKEY_URL не задан — пропуск");
            return;
        };
        let cfg = config(url);
        let node_a = ValkeyBus::launch(cfg.clone(), Profile::Dev, None).expect("start");
        let node_b = ValkeyBus::launch(cfg, Profile::Dev, None).expect("start");
        let scope = conformance::scope();
        let mut on_b = node_b.subscribe(scope).await.expect("subscribe");
        let mut sys_b = node_b.subscribe_system().await.expect("subscribe");
        node_a
            .publish(conformance::mailbox_changed(scope, 3))
            .await
            .expect("publish");
        node_a
            .publish(conformance::system(EventKind::ConfigReloaded))
            .await
            .expect("publish");
        assert_eq!(
            conformance::next(&mut on_b).await,
            Some(conformance::mailbox_changed(scope, 3))
        );
        assert_eq!(
            conformance::next(&mut sys_b).await,
            Some(conformance::system(EventKind::ConfigReloaded))
        );
    }

    /// TCP-прокси к Valkey, умеющий рвать установленные соединения.
    struct Proxy {
        addr: std::net::SocketAddr,
        conns: Arc<Mutex<Vec<JoinHandle<()>>>>,
        accept: JoinHandle<()>,
    }

    impl Proxy {
        async fn start(upstream: String) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
            let addr = listener.local_addr().expect("addr");
            let conns: Arc<Mutex<Vec<JoinHandle<()>>>> = Arc::default();
            let tracked = Arc::clone(&conns);
            let accept = tokio::spawn(async move {
                while let Ok((mut client, _)) = listener.accept().await {
                    let upstream = upstream.clone();
                    let conn = tokio::spawn(async move {
                        if let Ok(mut server) = TcpStream::connect(upstream).await {
                            let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                        }
                    });
                    tracked.lock().expect("lock").push(conn);
                }
            });
            Self {
                addr,
                conns,
                accept,
            }
        }

        fn cut(&self) {
            for conn in self.conns.lock().expect("lock").drain(..) {
                conn.abort();
            }
        }
    }

    impl Drop for Proxy {
        fn drop(&mut self) {
            self.accept.abort();
            self.cut();
        }
    }

    #[tokio::test]
    async fn reconnects_and_ends_streams() {
        let Some(url) = valkey_url() else {
            eprintln!("TEST_VALKEY_URL не задан — пропуск");
            return;
        };
        let info = redis::Client::open(url.as_str())
            .expect("url")
            .get_connection_info()
            .addr()
            .to_string();
        let proxy = Proxy::start(info).await;
        let bus = ValkeyBus::launch(
            config(format!("redis://{}", proxy.addr)),
            Profile::Dev,
            None,
        )
        .expect("start");
        let scope = conformance::scope();
        let mut sub = bus.subscribe(scope).await.expect("subscribe");
        bus.publish(conformance::mailbox_changed(scope, 1))
            .await
            .expect("publish");
        assert_eq!(
            conformance::next(&mut sub).await,
            Some(conformance::mailbox_changed(scope, 1))
        );

        proxy.cut();
        // Разрыв завершает поток подписчика.
        assert_eq!(conformance::next(&mut sub).await, None);
        drop(sub);

        // Переподписка и публикация проходят после переподключения.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut sub = loop {
            match bus.subscribe(scope).await {
                Ok(s) => break s,
                Err(_) if Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => panic!("переподключение не удалось: {e}"),
            }
        };
        while bus
            .publish(conformance::mailbox_changed(scope, 2))
            .await
            .is_err()
        {
            assert!(Instant::now() < deadline, "публикация не восстановилась");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(
            conformance::next(&mut sub).await,
            Some(conformance::mailbox_changed(scope, 2))
        );
        let m = bus.metrics();
        assert!(m.connected);
        assert!(m.reconnects >= 1);
    }
}
