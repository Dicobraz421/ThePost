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

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use redis::aio::{MultiplexedConnection, PubSubSink, PubSubStream};
use schemars::JsonSchema;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, MissedTickBehavior};
use tp_contracts::bus::{Event, EventBus, EventKind, EventStream};
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, Error, ModSeq, Result, TenantId};

use crate::local_bus::{LocalBus, LocalBusConfig};

/// Конфигурация шины Valkey.
#[derive(Clone, PartialEq, Eq, JsonSchema)]
pub struct ValkeyBusConfig {
    /// Адрес Valkey (`redis://[:пароль@]узел:порт[/база]`); в логи не попадает.
    pub url: String,
    /// Префикс имён каналов: разные кластеры на одном Valkey не слышат друг друга.
    pub channel_prefix: String,
    /// Буфер уведомлений подписчика на узле (как `capacity` шины в процессе).
    pub buffer: usize,
    /// Таймаут установки соединения, мс.
    pub connect_timeout_ms: u64,
    /// Таймаут команды (PUBLISH, SUBSCRIBE, PING), мс.
    pub command_timeout_ms: u64,
    /// Начальная задержка переподключения, мс; удваивается до `reconnect_max_ms`.
    pub reconnect_min_ms: u64,
    /// Предельная задержка переподключения, мс.
    pub reconnect_max_ms: u64,
    /// Период проверки pub/sub-соединения командой PING, мс.
    pub ping_interval_ms: u64,
}

impl Default for ValkeyBusConfig {
    fn default() -> Self {
        Self {
            url: "redis://127.0.0.1:6379".to_owned(),
            channel_prefix: "thepost".to_owned(),
            buffer: LocalBusConfig::default().capacity,
            connect_timeout_ms: 5_000,
            command_timeout_ms: 2_000,
            reconnect_min_ms: 100,
            reconnect_max_ms: 10_000,
            ping_interval_ms: 15_000,
        }
    }
}

impl fmt::Debug for ValkeyBusConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ValkeyBusConfig")
            .field("url", &"<скрыт>")
            .field("channel_prefix", &self.channel_prefix)
            .field("buffer", &self.buffer)
            .field("connect_timeout_ms", &self.connect_timeout_ms)
            .field("command_timeout_ms", &self.command_timeout_ms)
            .field("reconnect_min_ms", &self.reconnect_min_ms)
            .field("reconnect_max_ms", &self.reconnect_max_ms)
            .field("ping_interval_ms", &self.ping_interval_ms)
            .finish()
    }
}

impl ValkeyBusConfig {
    fn connect_timeout(&self) -> Duration {
        Duration::from_millis(self.connect_timeout_ms)
    }

    fn command_timeout(&self) -> Duration {
        Duration::from_millis(self.command_timeout_ms)
    }

    fn reconnect_min(&self) -> Duration {
        Duration::from_millis(self.reconnect_min_ms.max(1))
    }

    fn reconnect_max(&self) -> Duration {
        Duration::from_millis(self.reconnect_max_ms).max(self.reconnect_min())
    }

    fn ping_interval(&self) -> Duration {
        Duration::from_millis(self.ping_interval_ms.max(1))
    }

    fn account_channel(&self, tenant: TenantId, account: AccountId) -> String {
        format!("{}:acct:{tenant}:{account}", self.channel_prefix)
    }

    fn system_channel(&self) -> String {
        format!("{}:sys", self.channel_prefix)
    }
}

/// Снимок метрик шины Valkey.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ValkeyBusMetrics {
    /// Есть pub/sub-соединение.
    pub connected: bool,
    /// Опубликовано уведомлений.
    pub published: u64,
    /// Неудачных публикаций (нет связи, таймаут).
    pub publish_errors: u64,
    /// Получено уведомлений из Valkey.
    pub received: u64,
    /// Получено нераспознанных сообщений.
    pub decode_errors: u64,
    /// Потерь pub/sub-соединения.
    pub reconnects: u64,
    /// Подписчиков, отключённых за отставание.
    pub lagged: u64,
}

#[derive(Debug, Default)]
struct Counters {
    connected: AtomicBool,
    published: AtomicU64,
    publish_errors: AtomicU64,
    received: AtomicU64,
    decode_errors: AtomicU64,
    reconnects: AtomicU64,
}

fn inc(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}

/// Ошибка связи с Valkey.
#[derive(Debug, thiserror::Error)]
enum LinkError {
    #[error("таймаут")]
    Timeout,
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

struct Shared {
    config: ValkeyBusConfig,
    client: redis::Client,
    local: LocalBus,
    publisher: tokio::sync::Mutex<Option<MultiplexedConnection>>,
    counters: Counters,
}

impl Shared {
    async fn publisher(&self) -> Result<MultiplexedConnection> {
        let mut slot = self.publisher.lock().await;
        if let Some(conn) = slot.as_ref() {
            return Ok(conn.clone());
        }
        match within(
            self.config.connect_timeout(),
            self.client.get_multiplexed_async_connection(),
        )
        .await
        {
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
        let pubsub = within(
            self.config.connect_timeout(),
            self.client.get_async_pubsub(),
        )
        .await?;
        let (mut sink, stream) = pubsub.split();
        for channel in channels.keys() {
            within(self.config.command_timeout(), sink.subscribe(channel)).await?;
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
    /// Запускает шину: фоновая задача подключается к Valkey и держит подписки.
    /// Вызывать внутри runtime tokio.
    pub fn start(config: ValkeyBusConfig) -> Result<Self> {
        let client = redis::Client::open(config.url.as_str())
            .map_err(|_| Error::Invalid("шина Valkey: неверный адрес url".to_owned()))?;
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| Error::Internal("шина Valkey: запуск вне runtime tokio".to_owned()))?;
        let shared = Arc::new(Shared {
            local: LocalBus::new(&LocalBusConfig {
                capacity: config.buffer,
            }),
            config,
            client,
            publisher: tokio::sync::Mutex::new(None),
            counters: Counters::default(),
        });
        let (cmds, rx) = mpsc::unbounded_channel();
        let task = runtime.spawn(run(Arc::clone(&shared), rx));
        Ok(Self { shared, cmds, task })
    }

    /// Снимок метрик.
    pub fn metrics(&self) -> ValkeyBusMetrics {
        let c = &self.shared.counters;
        ValkeyBusMetrics {
            connected: c.connected.load(Ordering::Relaxed),
            published: c.published.load(Ordering::Relaxed),
            publish_errors: c.publish_errors.load(Ordering::Relaxed),
            received: c.received.load(Ordering::Relaxed),
            decode_errors: c.decode_errors.load(Ordering::Relaxed),
            reconnects: c.reconnects.load(Ordering::Relaxed),
            lagged: self.shared.local.lagged(),
        }
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
        let wait = self.shared.config.connect_timeout() + self.shared.config.command_timeout();
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
            config.command_timeout(),
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
                shared.counters.connected.store(true, Ordering::Relaxed);
                tracing::info!("шина Valkey: соединение установлено");
                backoff = shared.config.reconnect_min();
                let stop = serve(&shared, &mut cmds, &mut channels, sink, stream).await;
                shared.counters.connected.store(false, Ordering::Relaxed);
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
    let timeout = shared.config.command_timeout();
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

    fn valkey_url() -> Option<String> {
        std::env::var("TEST_VALKEY_URL").ok()
    }

    /// Свой префикс на тест: тесты идут параллельно на одном Valkey.
    fn config(url: String) -> ValkeyBusConfig {
        ValkeyBusConfig {
            url,
            channel_prefix: format!("tp-test-{}", AccountId::new()),
            connect_timeout_ms: 1_000,
            command_timeout_ms: 1_000,
            reconnect_min_ms: 20,
            reconnect_max_ms: 200,
            ..ValkeyBusConfig::default()
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
        let c = ValkeyBusConfig::default();
        let (t, a) = (TenantId::DEFAULT, AccountId::new());
        assert_eq!(c.account_channel(t, a), format!("thepost:acct:{t}:{a}"));
        assert_eq!(c.system_channel(), "thepost:sys");
    }

    #[test]
    fn debug_hides_url() {
        let c = ValkeyBusConfig {
            url: "redis://:secret@host:6379".to_owned(),
            ..ValkeyBusConfig::default()
        };
        assert!(!format!("{c:?}").contains("secret"));
    }

    #[test]
    fn rejects_bad_url_and_missing_runtime() {
        let bad = ValkeyBusConfig {
            url: "not a url".to_owned(),
            ..ValkeyBusConfig::default()
        };
        assert!(matches!(ValkeyBus::start(bad), Err(Error::Invalid(_))));
        assert!(matches!(
            ValkeyBus::start(ValkeyBusConfig::default()),
            Err(Error::Internal(_))
        ));
    }

    #[tokio::test]
    async fn unreachable_valkey_is_unavailable() {
        // Порт слушающего и сразу закрытого сокета: соединение отвергается.
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        let bus = ValkeyBus::start(config(format!("redis://{addr}"))).expect("start");
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
        let bus = ValkeyBus::start(config(url)).expect("start");
        conformance::run(&bus).await;
        let m = bus.metrics();
        assert!(m.connected);
        assert_eq!(m.decode_errors, 0);
        assert!(m.received > 0 && m.published >= m.received / 2);
    }

    #[tokio::test]
    async fn delivers_between_nodes() {
        let Some(url) = valkey_url() else {
            eprintln!("TEST_VALKEY_URL не задан — пропуск");
            return;
        };
        let cfg = config(url);
        let node_a = ValkeyBus::start(cfg.clone()).expect("start");
        let node_b = ValkeyBus::start(cfg).expect("start");
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
        let bus = ValkeyBus::start(config(format!("redis://{}", proxy.addr))).expect("start");
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
