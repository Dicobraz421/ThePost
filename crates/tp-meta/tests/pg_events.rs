//! Журнал событий ядра на PostgreSQL: чтение, позиции, лизы разделов, очистка (META-06).

mod common;

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use tp_contracts::events::{
    CoreEventKind, EventLog, EventPos, EventSink, LoggedEvent, Partition, partition_of,
};
use tp_contracts::meta::{AccountScope, MailboxStore, NewMailbox};
use tp_contracts::{Error, MailboxId, Result};
use tp_meta::events::{EventConsumer, EventsConfig};
use tp_meta::{MetaConfig, PgMeta, schema};
use uuid::Uuid;

/// Разделов больше, чем у остальных тестов (64): разделы этих тестов почти наверняка свои.
const PARTITIONS: u32 = 1 << 20;

async fn meta() -> Option<PgMeta> {
    let pool = common::pool().await?;
    Some(PgMeta::new(
        pool,
        MetaConfig {
            event_partitions: PARTITIONS,
            ..MetaConfig::default()
        },
    ))
}

/// Учётная запись в разделе, которого нет у тестов с 64 разделами.
async fn account(meta: &PgMeta) -> (AccountScope, Partition) {
    loop {
        let scope = common::account(meta).await;
        let p = partition_of(scope.account, PARTITIONS);
        if p.0 >= 64 {
            return (scope, p);
        }
    }
}

fn subscriber() -> String {
    format!("test-{}", Uuid::now_v7().simple())
}

/// Подписчик, запоминающий пачки; первые `fail` вызовов завершаются ошибкой.
#[derive(Default)]
struct Recorder {
    name: String,
    fail: AtomicUsize,
    delay: Duration,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
    calls: AtomicUsize,
    seen: Mutex<Vec<LoggedEvent>>,
}

impl Recorder {
    fn new(name: &str) -> Self {
        Self {
            name: name.into(),
            ..Self::default()
        }
    }

    fn seen(&self, scope: AccountScope) -> Vec<LoggedEvent> {
        self.seen
            .lock()
            .expect("lock")
            .iter()
            .filter(|e| e.event.account == scope.account)
            .cloned()
            .collect()
    }
}

#[async_trait]
impl EventSink for Recorder {
    fn name(&self) -> &str {
        &self.name
    }

    async fn handle(&self, batch: &[LoggedEvent]) -> Result<()> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_in_flight.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.in_flight.fetch_sub(1, Ordering::SeqCst);
        if self.fail.load(Ordering::SeqCst) > 0 {
            self.fail.fetch_sub(1, Ordering::SeqCst);
            return Err(Error::UNAVAILABLE);
        }
        self.seen.lock().expect("lock").extend_from_slice(batch);
        Ok(())
    }
}

fn consumer(meta: &PgMeta, holder: &str, batch: usize) -> EventConsumer {
    let shared = Arc::new(meta.clone());
    EventConsumer::new(
        shared.clone(),
        shared,
        holder,
        EventsConfig {
            batch,
            lease_ttl: Duration::from_secs(30),
            ..EventsConfig::default()
        },
    )
}

/// Обрабатывает раздел до конца; возвращает число пачек.
async fn drain(c: &EventConsumer, sink: &Recorder, p: Partition) -> usize {
    let mut batches = 0;
    while c
        .poll_partition(sink, p)
        .await
        .expect("poll_partition")
        .expect("лиза")
        > 0
    {
        batches += 1;
    }
    batches
}

async fn account_events(meta: &PgMeta, scope: AccountScope) -> i64 {
    let mut tx = schema::begin_system(meta.pool()).await.expect("pool");
    let n = sqlx::query_scalar("SELECT count(*) FROM events WHERE account_id = $1")
        .bind(scope.account.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .expect("fetch_one");
    tx.commit().await.expect("commit");
    n
}

async fn create_mailboxes(meta: &PgMeta, scope: AccountScope, n: usize) -> Vec<MailboxId> {
    let mut ids = Vec::new();
    for i in 0..n {
        let mb = meta
            .create(
                scope,
                NewMailbox {
                    parent: None,
                    name: format!("m{i}"),
                    role: None,
                },
            )
            .await
            .expect("ok");
        ids.push(mb.id);
    }
    ids
}

#[tokio::test]
async fn at_least_once_and_commit() {
    let Some(meta) = meta().await else {
        return;
    };
    let (scope, p) = account(&meta).await;
    let total = account_events(&meta, scope).await;
    assert!(total > 0, "создание учётной записи пишет события папок");

    let sink = Recorder::new(&subscriber());
    sink.fail.store(1, Ordering::SeqCst);
    let c = consumer(&meta, "a", 1000);
    assert!(c.poll_partition(&sink, p).await.is_err());
    assert_eq!(
        meta.position(&sink.name, p).await.expect("position"),
        EventPos(0)
    );

    // Пачка после сбоя приходит снова и фиксируется.
    assert!(
        c.poll_partition(&sink, p)
            .await
            .expect("poll_partition")
            .expect("poll_partition")
            > 0
    );
    let seen = sink.seen(scope);
    assert_eq!(i64::try_from(seen.len()).expect("len"), total);
    let last = meta.position(&sink.name, p).await.expect("position");
    assert!(last >= seen.last().expect("last").pos);
    assert_eq!(
        c.poll_partition(&sink, p).await.expect("poll_partition"),
        Some(0)
    );

    // Позиция не уменьшается.
    meta.commit(&sink.name, p, EventPos(0)).await.expect("read");
    assert_eq!(meta.position(&sink.name, p).await.expect("position"), last);
    assert!(meta.head(p).await.expect("head") >= last);
}

#[tokio::test]
async fn order_within_mailbox() {
    let Some(meta) = meta().await else {
        return;
    };
    let (scope, p) = account(&meta).await;
    let ids = create_mailboxes(&meta, scope, 5).await;

    let sink = Recorder::new(&subscriber());
    let c = consumer(&meta, "a", 2);
    assert!(drain(&c, &sink, p).await >= 3);
    let seen = sink.seen(scope);
    assert!(seen.windows(2).all(|w| w[0].pos < w[1].pos));
    let created: Vec<_> = seen
        .iter()
        .filter(|e| e.event.kind == CoreEventKind::MailboxCreated)
        .filter_map(|e| e.event.mailbox)
        .filter(|m| ids.contains(m))
        .collect();
    assert_eq!(created, ids);
    assert!(
        seen.iter()
            .all(|e| e.partition == p && e.event.tenant == scope.tenant)
    );
}

#[tokio::test]
async fn two_instances_do_not_share_partition() {
    let Some(meta) = meta().await else {
        return;
    };
    let (_, p) = account(&meta).await;
    let mut sink = Recorder::new(&subscriber());
    sink.delay = Duration::from_millis(200);
    let a = consumer(&meta, "a", 1);
    let b = consumer(&meta, "b", 1);

    let (ra, rb) = tokio::join!(a.poll_partition(&sink, p), b.poll_partition(&sink, p));
    let (ra, rb) = (ra.expect("a"), rb.expect("b"));
    assert!(
        ra.is_some() != rb.is_some(),
        "раздел достался ровно одному: {ra:?} {rb:?}"
    );
    assert_eq!(sink.max_in_flight.load(Ordering::SeqCst), 1);

    // Лиза остаётся у держателя: второй экземпляр не читает раздел и следующими вызовами.
    let (owner, other) = if ra.is_some() { (&a, &b) } else { (&b, &a) };
    assert_eq!(
        other
            .poll_partition(&sink, p)
            .await
            .expect("poll_partition"),
        None
    );
    assert!(
        owner
            .poll_partition(&sink, p)
            .await
            .expect("poll_partition")
            .is_some()
    );
    assert_eq!(sink.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn purge_events_read_by_all() {
    let Some(meta) = meta().await else {
        return;
    };
    let (scope, p) = account(&meta).await;
    let total = account_events(&meta, scope).await;
    assert!(total >= 3);

    let s1 = Recorder::new(&subscriber());
    let s2 = Recorder::new(&subscriber());
    drain(&consumer(&meta, "a", 1000), &s1, p).await;
    let mine = s1.seen(scope);
    meta.commit(&s2.name, p, mine[1].pos).await.expect("commit");

    meta.purge_events(&[], 1000).await.expect("purge_events");
    assert_eq!(account_events(&meta, scope).await, total);
    meta.purge_events(&[&s1.name, &s2.name], 1000)
        .await
        .expect("purge_events");
    assert_eq!(account_events(&meta, scope).await, total - 2);
    let rest = meta.read(p, EventPos(0), 1000).await.expect("read");
    assert!(rest.iter().all(|e| e.pos > mine[1].pos));

    meta.purge_events(&[&s1.name], 1000)
        .await
        .expect("purge_events");
    assert_eq!(account_events(&meta, scope).await, 0);
    // Позиции раздела продолжаются после очистки.
    create_mailboxes(&meta, scope, 1).await;
    let next = meta.read(p, EventPos(0), 1000).await.expect("read");
    assert!(!next.is_empty());
    assert!(next.iter().all(|e| e.pos > mine.last().expect("last").pos));
}

#[tokio::test]
async fn partition_out_of_range() {
    let Some(meta) = meta().await else {
        return;
    };
    assert!(matches!(
        meta.read(Partition(PARTITIONS), EventPos(0), 1).await,
        Err(Error::Invalid(_))
    ));
}
