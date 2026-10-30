//! Шина уведомлений в процессе: SMB, один узел (CONTRACTS.md#EventBus).
//!
//! На учётную запись — свой `broadcast`-канал с ограниченным буфером, системные уведомления — в
//! общем канале. Канал учётной записи создаётся первой подпиской и удаляется с последним подписчиком.
//! Подписчик, отставший больше чем на буфер, получает `Lagged`: его поток завершается, и подписчик,
//! переподписавшись, сверяется с хранилищем по `ModSeq` (доставка at-most-once).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};

use async_trait::async_trait;
use futures::{StreamExt, stream};
use schemars::JsonSchema;
use tokio::sync::broadcast::{self, error::RecvError};
use tp_contracts::bus::{Event, EventBus, EventStream};
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, Result, TenantId};

/// Конфигурация шины в процессе.
#[derive(Debug, Clone, PartialEq, Eq, JsonSchema)]
pub struct LocalBusConfig {
    /// Буфер канала учётной записи и системного канала: сколько непрочитанных уведомлений
    /// подписчик может накопить, прежде чем отстанет и будет отключён (не меньше 1).
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
    lagged: AtomicU64,
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
        // broadcast::channel паникует на 0 и на ёмкости больше usize::MAX / 2.
        let capacity = config.capacity.clamp(1, usize::MAX >> 1);
        Self {
            inner: Arc::new(Inner {
                capacity,
                generation: AtomicU64::new(0),
                accounts: Mutex::new(HashMap::new()),
                system: Mutex::new(broadcast::channel(capacity).0),
                lagged: AtomicU64::new(0),
            }),
        }
    }

    /// Сколько раз подписчики отстали и были отключены.
    pub fn lagged(&self) -> u64 {
        self.inner.lagged.load(Ordering::Relaxed)
    }

    /// Завершает потоки всех подписчиков (после уже накопленных уведомлений): они переподписываются
    /// и сверяются с хранилищем. Используется при потере связи с внешней шиной.
    pub fn close_all(&self) {
        self.inner.accounts().clear();
        *self.inner.system() = broadcast::channel(self.inner.capacity).0;
    }

    /// Раздаёт уведомление подписчикам этого процесса.
    pub(crate) fn deliver(&self, event: Event) {
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
    pub(crate) fn subscribe_account(&self, scope: AccountScope) -> EventStream {
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
            inner: Arc::downgrade(&self.inner),
            channel: Some((key, generation)),
        })
    }

    /// Подписка на системные уведомления.
    pub(crate) fn subscribe_system_now(&self) -> EventStream {
        into_stream(Subscription {
            rx: self.inner.system().subscribe(),
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
            Ok(event) => Some((event, sub)),
            Err(RecvError::Lagged(skipped)) => {
                if let Some(inner) = sub.inner.upgrade() {
                    inner.lagged.fetch_add(1, Ordering::Relaxed);
                }
                tracing::debug!(skipped, "шина: подписчик отстал, поток завершён");
                None
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

    #[tokio::test]
    async fn lagging_subscriber_is_cut_and_resubscribes() {
        let bus = LocalBus::new(&LocalBusConfig { capacity: 2 });
        let scope = conformance::scope();
        let mut slow = bus.subscribe(scope).await.expect("subscribe");
        let mut fast = bus.subscribe(scope).await.expect("subscribe");
        for i in 1..=5 {
            bus.publish(changed(scope, i)).await.expect("publish");
            // Быстрый подписчик читает сразу и не отстаёт.
            assert_eq!(fast.next().await, Some(changed(scope, i)));
        }
        // Медленный отстал больше чем на буфер: поток завершается без уведомлений.
        assert_eq!(slow.next().await, None);
        assert_eq!(bus.lagged(), 1);
        drop(slow);

        // Переподписка — и уведомления снова идут (после сверки по ModSeq у подписчика).
        let mut again = bus.subscribe(scope).await.expect("subscribe");
        bus.publish(changed(scope, 6)).await.expect("publish");
        assert_eq!(again.next().await, Some(changed(scope, 6)));
        assert_eq!(fast.next().await, Some(changed(scope, 6)));
    }

    #[tokio::test]
    async fn lagging_system_subscriber_is_cut() {
        let bus = LocalBus::new(&LocalBusConfig { capacity: 1 });
        let mut sys = bus.subscribe_system().await.expect("subscribe");
        for _ in 0..3 {
            bus.publish(conformance::system(EventKind::QueueReady))
                .await
                .expect("publish");
        }
        assert_eq!(sys.next().await, None);
        assert_eq!(bus.lagged(), 1);
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

    #[test]
    fn zero_capacity_is_clamped() {
        let bus = LocalBus::new(&LocalBusConfig { capacity: 0 });
        assert_eq!(bus.inner.capacity, 1);
    }
}
