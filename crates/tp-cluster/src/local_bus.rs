//! Шина уведомлений в процессе: SMB, один узел (CONTRACTS.md#EventBus).
//!
//! На учётную запись — свой `broadcast`-канал с ограниченным буфером, системные уведомления — в
//! общем канале. Канал учётной записи создаётся первой подпиской и удаляется с последним подписчиком.
//! Подписчик, отставший больше чем на буфер, теряет накопленные уведомления и получает вместо них один
//! `EventKind::Resync` (сверка с хранилищем, доставка at-most-once); поток не завершается, буфер подписчика
//! освобождается, и дальше идут новые уведомления.
//! Счётчики — [`BusCounters`] (метрики платформы).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::sync::broadcast::{self, error::RecvError};
use tp_contracts::bus::{Event, EventBus, EventKind, EventStream};
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, Error, Result, TenantId};

use crate::metrics::{BusCounters, BusMetrics, inc};

/// Конфигурация шины в процессе. Незаданные поля — по умолчанию, неизвестные — ошибка разбора.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LocalBusConfig {
    /// Буфер канала учётной записи и системного канала: сколько непрочитанных уведомлений
    /// подписчик может накопить, прежде чем отстанет и получит `Resync` (не меньше 1).
    pub capacity: usize,
}

impl Default for LocalBusConfig {
    fn default() -> Self {
        Self { capacity: 256 }
    }
}

type Key = (TenantId, AccountId);

/// Канал с поколением: подписка снимает канал, только если он всё ещё тот, на который она подписана.
#[derive(Debug)]
struct Channel {
    generation: u64,
    tx: broadcast::Sender<Event>,
}

#[derive(Debug)]
struct Inner {
    capacity: usize,
    generation: AtomicU64,
    accounts: Mutex<HashMap<Key, Channel>>,
    system: Mutex<broadcast::Sender<Event>>,
    counters: Arc<BusCounters>,
}

impl Inner {
    fn accounts(&self) -> MutexGuard<'_, HashMap<Key, Channel>> {
        self.accounts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn system(&self) -> MutexGuard<'_, broadcast::Sender<Event>> {
        self.system.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Шина уведомлений в процессе.
#[derive(Debug, Clone)]
pub struct LocalBus {
    inner: Arc<Inner>,
}

impl LocalBus {
    /// Создаёт шину.
    pub fn new(config: &LocalBusConfig) -> Self {
        let counters = BusCounters::default();
        counters.set_connected(true);
        let counters = Arc::new(counters);
        // broadcast::channel паникует на 0 и на ёмкости больше usize::MAX / 2.
        let capacity = config.capacity.clamp(1, usize::MAX >> 1);
        Self {
            inner: Arc::new(Inner {
                capacity,
                generation: AtomicU64::new(0),
                accounts: Mutex::new(HashMap::new()),
                system: Mutex::new(broadcast::channel(capacity).0),
                counters,
            }),
        }
    }

    /// Сколько раз подписчики отстали и получили `Resync`.
    pub fn lagged(&self) -> u64 {
        self.inner.counters.lagged.load(Ordering::Relaxed)
    }

    /// Счётчики шины для метрик платформы.
    pub fn counters(&self) -> Arc<BusCounters> {
        Arc::clone(&self.inner.counters)
    }

    /// Снимок счётчиков.
    pub fn metrics(&self) -> BusMetrics {
        self.inner.counters.snapshot()
    }

    /// Завершает потоки всех подписчиков (после уже накопленных уведомлений): они переподписываются
    /// и сверяются с хранилищем.
    pub fn close_all(&self) {
        self.inner.accounts().clear();
        *self.inner.system() = broadcast::channel(self.inner.capacity).0;
    }

    /// Раздаёт уведомление подписчикам этого процесса.
    fn deliver(&self, event: Event) {
        match event.account {
            Some(account) => {
                let key = (event.tenant, account);
                let mut accounts = self.inner.accounts();
                if let Some(ch) = accounts.get(&key)
                    && ch.tx.send(event).is_err()
                {
                    // Подписчиков не осталось — их подписки уже отпустили канал или вот-вот отпустят.
                    accounts.remove(&key);
                }
            }
            None => {
                // Ошибка — нет подписчиков; уведомление никому не нужно.
                let _ = self.inner.system().send(event);
            }
        }
    }

    /// Подписка на учётную запись.
    fn subscribe_account(&self, scope: AccountScope) -> EventStream {
        let key = (scope.tenant, scope.account);
        let rx;
        let generation;
        {
            let mut accounts = self.inner.accounts();
            let ch = accounts.entry(key).or_insert_with(|| Channel {
                generation: self.inner.generation.fetch_add(1, Ordering::Relaxed),
                tx: broadcast::channel(self.inner.capacity).0,
            });
            rx = ch.tx.subscribe();
            generation = ch.generation;
        }
        into_stream(Subscription {
            rx,
            resync: Event {
                tenant: scope.tenant,
                account: Some(scope.account),
                kind: EventKind::Resync,
            },
            counters: self.counters(),
            inner: Arc::downgrade(&self.inner),
            channel: Some((key, generation)),
        })
    }

    /// Подписка на системные уведомления.
    fn subscribe_system_now(&self) -> EventStream {
        into_stream(Subscription {
            rx: self.inner.system().subscribe(),
            resync: Event {
                tenant: TenantId::DEFAULT,
                account: None,
                kind: EventKind::Resync,
            },
            counters: self.counters(),
            inner: Arc::downgrade(&self.inner),
            channel: None,
        })
    }

    #[cfg(test)]
    fn account_channels(&self) -> usize {
        self.inner.accounts().len()
    }
}

impl Default for LocalBus {
    fn default() -> Self {
        Self::new(&LocalBusConfig::default())
    }
}

#[async_trait]
impl EventBus for LocalBus {
    async fn publish(&self, event: Event) -> Result<()> {
        if event.kind == EventKind::Resync {
            return Err(Error::invalid(
                "Resync is inserted by the bus, not published",
            ));
        }
        let counters = &self.inner.counters;
        inc(&counters.published);
        inc(&counters.received);
        self.deliver(event);
        Ok(())
    }

    async fn subscribe(&self, scope: AccountScope) -> Result<EventStream> {
        Ok(self.subscribe_account(scope))
    }

    async fn subscribe_system(&self) -> Result<EventStream> {
        Ok(self.subscribe_system_now())
    }
}

/// Подписчик; при удалении отпускает канал учётной записи, если был на нём последним.
#[derive(Debug)]
struct Subscription {
    rx: broadcast::Receiver<Event>,
    /// Сигнал сверки этой подписки.
    resync: Event,
    counters: Arc<BusCounters>,
    inner: Weak<Inner>,
    channel: Option<(Key, u64)>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        let (Some((key, generation)), Some(inner)) = (self.channel, self.inner.upgrade()) else {
            return;
        };
        let mut accounts = inner.accounts();
        // Под блокировкой новых подписчиков нет; единственный оставшийся получатель — этот.
        if accounts
            .get(&key)
            .is_some_and(|ch| ch.generation == generation && ch.tx.receiver_count() <= 1)
        {
            accounts.remove(&key);
        }
    }
}

fn into_stream(sub: Subscription) -> EventStream {
    stream::unfold(sub, |mut sub| async move {
        match sub.rx.recv().await {
            Ok(event) => {
                inc(&sub.counters.delivered);
                Some((event, sub))
            }
            Err(RecvError::Lagged(skipped)) => {
                // Оставшиеся в буфере уведомления сверка всё равно покроет: буфер освобождается
                // (получатель с хвоста канала), и `Resync` приходит один раз на переполнение.
                sub.rx = sub.rx.resubscribe();
                inc(&sub.counters.lagged);
                tracing::debug!(skipped, "шина: подписчик отстал, Resync");
                let resync = sub.resync.clone();
                Some((resync, sub))
            }
            Err(RecvError::Closed) => None,
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance;
    use tp_contracts::bus::EventKind;

    fn changed(scope: AccountScope, modseq: u64) -> Event {
        conformance::mailbox_changed(scope, modseq)
    }

    #[tokio::test]
    async fn conformance_suite() {
        conformance::run(&LocalBus::default()).await;
    }

    #[test]
    fn parses_yaml_section() {
        let c: LocalBusConfig = serde_yaml_ng::from_str("capacity: 64").expect("yaml");
        assert_eq!(c, LocalBusConfig { capacity: 64 });
        assert_eq!(
            serde_yaml_ng::from_str::<LocalBusConfig>("{}").expect("empty"),
            LocalBusConfig::default()
        );
        for bad in ["capacity: -1", "capacity: many", "buffer: 64"] {
            assert!(
                serde_yaml_ng::from_str::<LocalBusConfig>(bad).is_err(),
                "{bad}"
            );
        }
    }

    fn resync(scope: AccountScope) -> Event {
        Event {
            tenant: scope.tenant,
            account: Some(scope.account),
            kind: EventKind::Resync,
        }
    }

    /// Следующее уведомление, если оно уже в буфере.
    async fn ready(stream: &mut EventStream) -> Option<Event> {
        tokio::time::timeout(std::time::Duration::from_millis(50), stream.next())
            .await
            .ok()
            .flatten()
    }

    #[tokio::test]
    async fn signals_suite() {
        tp_testkit::suite::signals(&LocalBus::default()).await;
    }

    #[tokio::test]
    async fn lagging_subscriber_gets_resync_then_new_events() {
        let bus = LocalBus::new(&LocalBusConfig { capacity: 2 });
        let scope = conformance::scope();
        let mut slow = bus.subscribe(scope).await.expect("subscribe");
        let mut fast = bus.subscribe(scope).await.expect("subscribe");
        for i in 1..=5 {
            bus.publish(changed(scope, i)).await.expect("publish");
            // Быстрый подписчик читает сразу и не отстаёт.
            assert_eq!(fast.next().await, Some(changed(scope, i)));
        }
        // Медленный отстал больше чем на буфер: один `Resync` вместо потерянных, буфер освобождён.
        assert_eq!(slow.next().await, Some(resync(scope)));
        assert_eq!(ready(&mut slow).await, None, "Resync один, старых нет");
        assert_eq!(bus.lagged(), 1);

        // Поток жив: дальше — новые уведомления.
        bus.publish(changed(scope, 6)).await.expect("publish");
        assert_eq!(slow.next().await, Some(changed(scope, 6)));
        assert_eq!(fast.next().await, Some(changed(scope, 6)));

        // Новое переполнение — новый `Resync`.
        for i in 7..=10 {
            bus.publish(changed(scope, i)).await.expect("publish");
        }
        assert_eq!(slow.next().await, Some(resync(scope)));
        assert_eq!(bus.lagged(), 2);
        bus.publish(changed(scope, 11)).await.expect("publish");
        assert_eq!(slow.next().await, Some(changed(scope, 11)));
    }

    #[tokio::test]
    async fn lagging_system_subscriber_gets_resync() {
        let bus = LocalBus::new(&LocalBusConfig { capacity: 1 });
        let mut sys = bus.subscribe_system().await.expect("subscribe");
        for _ in 0..3 {
            bus.publish(conformance::system(EventKind::QueueReady))
                .await
                .expect("publish");
        }
        let resync = Event {
            tenant: TenantId::DEFAULT,
            account: None,
            kind: EventKind::Resync,
        };
        assert_eq!(sys.next().await, Some(resync));
        assert_eq!(ready(&mut sys).await, None);
        assert_eq!(bus.lagged(), 1);
        bus.publish(conformance::system(EventKind::ConfigReloaded))
            .await
            .expect("publish");
        assert_eq!(
            sys.next().await,
            Some(conformance::system(EventKind::ConfigReloaded))
        );
    }

    #[tokio::test]
    async fn publish_resync_is_invalid() {
        let bus = LocalBus::default();
        let scope = conformance::scope();
        assert!(matches!(
            bus.publish(resync(scope)).await,
            Err(Error::Invalid(_))
        ));
        assert_eq!(bus.metrics().published, 0);
    }

    #[tokio::test]
    async fn channel_released_with_last_subscriber() {
        let bus = LocalBus::default();
        let scope = conformance::scope();
        let a = bus.subscribe(scope).await.expect("subscribe");
        let b = bus.subscribe(scope).await.expect("subscribe");
        assert_eq!(bus.account_channels(), 1);
        drop(a);
        assert_eq!(bus.account_channels(), 1);
        drop(b);
        assert_eq!(bus.account_channels(), 0);
    }

    #[tokio::test]
    async fn close_all_ends_streams_after_buffered() {
        let bus = LocalBus::default();
        let scope = conformance::scope();
        let mut acc = bus.subscribe(scope).await.expect("subscribe");
        let mut sys = bus.subscribe_system().await.expect("subscribe");
        bus.publish(changed(scope, 1)).await.expect("publish");
        bus.close_all();
        assert_eq!(acc.next().await, Some(changed(scope, 1)));
        assert_eq!(acc.next().await, None);
        assert_eq!(sys.next().await, None);

        // Старая подписка не снимает канал, созданный после close_all.
        let mut fresh = bus.subscribe(scope).await.expect("subscribe");
        drop(acc);
        assert_eq!(bus.account_channels(), 1);
        bus.publish(changed(scope, 2)).await.expect("publish");
        assert_eq!(fresh.next().await, Some(changed(scope, 2)));
    }

    #[tokio::test]
    async fn counters_grow_on_publish_and_receive() {
        let bus = LocalBus::default();
        let counters = bus.counters();
        assert_eq!(
            counters.snapshot(),
            BusMetrics {
                connected: true,
                ..BusMetrics::default()
            }
        );
        let scope = conformance::scope();
        let mut a = bus.subscribe(scope).await.expect("subscribe");
        let mut b = bus.subscribe(scope).await.expect("subscribe");
        let mut sys = bus.subscribe_system().await.expect("subscribe");
        bus.publish(changed(scope, 1)).await.expect("publish");
        bus.publish(conformance::system(EventKind::QueueReady))
            .await
            .expect("publish");
        assert_eq!(a.next().await, Some(changed(scope, 1)));
        assert_eq!(b.next().await, Some(changed(scope, 1)));
        assert!(sys.next().await.is_some());

        let m = counters.snapshot();
        assert_eq!((m.published, m.received, m.delivered), (2, 2, 3));
        assert_eq!(m.lagged, 0);
        assert_eq!(bus.metrics(), m);
    }

    #[test]
    fn zero_capacity_is_clamped() {
        let bus = LocalBus::new(&LocalBusConfig { capacity: 0 });
        assert_eq!(bus.inner.capacity, 1);
    }
}
