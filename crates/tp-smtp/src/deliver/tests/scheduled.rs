//! Отложенная отправка и отмена (SMTP-24): настоящая очередь (`MemQueue`, управляемые часы) и её обработчик с
//! локальной доставкой через `Delivery`.

use std::time::Duration;

use tp_testkit::{ManualClock, MemQueue};

use super::*;
use crate::config::QueueConfig;
use crate::queue::tests::Scripted;
use crate::queue::{Queue as OutQueue, QueueDeps, QueueWorker};

struct Sched {
    env: Env,
    clock: ManualClock,
    queue: MemQueue,
    delivery: Arc<Delivery>,
    transport: Arc<Scripted>,
    worker: QueueWorker,
}

impl Sched {
    async fn new() -> Self {
        let env = Env::new(DecisionHooks::default()).await;
        let clock = ManualClock::default();
        let queue = MemQueue::new(clock.shared());
        let c = clock.clone();
        let deps = QueueDeps {
            store: Arc::new(queue.clone()),
            blobs: Arc::new(env.blobs.clone()),
            index: Arc::new(env.index.clone()),
            now: Arc::new(move || c.now()),
        };
        let base = env.delivery(false);
        let delivery = Arc::new(Delivery::new(
            Deps {
                outbound: Some(Arc::new(OutQueue::new(deps.clone()))),
                ..base.deps.clone()
            },
            &SmtpConfig {
                hostname: "mx.example.org".into(),
                ..SmtpConfig::default()
            },
        ));
        let transport = Arc::new(Scripted::default());
        let worker = QueueWorker::new(deps, transport.clone(), "mx.example.org")
            .with_local(delivery.clone());
        Self {
            env,
            clock,
            queue,
            delivery,
            transport,
            worker,
        }
    }

    fn refs(&self, key: &tp_contracts::BlobKey) -> u32 {
        self.env
            .index
            .row(key.tenant, &key.hash)
            .map_or(0, |r| r.refcnt)
    }
}

#[tokio::test]
async fn released_at_time_to_local_and_external() {
    let s = Sched::new().await;
    let at = s.clock.now() + Duration::from_secs(600);
    let list = rcpts(&["bob@example.org", "x@example.net", "nobody@example.org"]);
    s.delivery
        .schedule(
            TenantId::DEFAULT,
            "alice@example.org",
            &list,
            Bytes::from_static(MAIL),
            at,
        )
        .await
        .expect("schedule");
    let cfg = QueueConfig::default();
    // До срока запись не выпускается: ни в ящик, ни наружу.
    s.clock.advance(Duration::from_secs(599));
    assert_eq!(s.worker.run_once(&cfg).await.expect("run"), 0);
    assert!(s.env.inbox("bob@example.org").await.is_empty());
    assert!(s.transport.calls.lock().expect("lock").is_empty());
    // В срок: локальному — в ящик, внешнему — по SMTP, неизвестному — отказ с уведомлением отправителю.
    s.clock.advance(Duration::from_secs(1));
    assert_eq!(s.worker.run_once(&cfg).await.expect("run"), 1);
    assert_eq!(s.env.inbox("bob@example.org").await.len(), 1);
    {
        let calls = s.transport.calls.lock().expect("lock");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "alice@example.org");
        assert_eq!(calls[0].1, ["x@example.net"]);
    }
    // Уведомление о недоставке — записью очереди отправителю (обработчик), одно.
    assert_eq!(s.worker.run_once(&cfg).await.expect("run"), 1);
    let dsn = s.env.inbox("alice@example.org").await;
    assert_eq!(dsn.len(), 1, "{dsn:?}");
    assert!(dsn[0].contains("nobody@example.org"));
    assert_eq!(s.env.inbox("bob@example.org").await.len(), 1);
    assert!(s.queue.blobs().is_empty());
}

#[tokio::test]
async fn cancelled_before_release_is_not_sent_and_body_is_freed() {
    let s = Sched::new().await;
    let at = s.clock.now() + Duration::from_secs(10);
    let list = rcpts(&["bob@example.org", "x@example.net"]);
    let id = s
        .delivery
        .schedule(
            TenantId::DEFAULT,
            "alice@example.org",
            &list,
            Bytes::from_static(MAIL),
            at,
        )
        .await
        .expect("schedule");
    let blob = s.queue.blobs()[0];
    assert_eq!(s.refs(&blob), 1);
    s.delivery
        .cancel_scheduled(TenantId::DEFAULT, id)
        .await
        .expect("cancel");
    // Ссылка на тело снята, запись удалена: после срока ничего не уходит.
    assert_eq!(s.refs(&blob), 0);
    assert!(s.queue.blobs().is_empty());
    s.clock.advance(Duration::from_secs(60));
    assert_eq!(
        s.worker
            .run_once(&QueueConfig::default())
            .await
            .expect("run"),
        0
    );
    assert!(s.env.inbox("bob@example.org").await.is_empty());
    assert!(s.transport.calls.lock().expect("lock").is_empty());
    assert_eq!(
        s.delivery.cancel_scheduled(TenantId::DEFAULT, id).await,
        Err(Error::NotFound)
    );
}

#[tokio::test]
async fn released_message_cannot_be_cancelled() {
    let s = Sched::new().await;
    let at = s.clock.now() + Duration::from_secs(10);
    let id = s
        .delivery
        .schedule(
            TenantId::DEFAULT,
            "alice@example.org",
            &rcpts(&["x@example.net"]),
            Bytes::from_static(MAIL),
            at,
        )
        .await
        .expect("schedule");
    s.transport
        .push(crate::queue::RcptResult::Temporary("451 busy".into()));
    s.clock.advance(Duration::from_secs(10));
    assert_eq!(
        s.worker
            .run_once(&QueueConfig::default())
            .await
            .expect("run"),
        1
    );
    assert_eq!(
        s.delivery.cancel_scheduled(TenantId::DEFAULT, id).await,
        Err(Error::Conflict)
    );
}

/// Контракт `Outbound` (CTR-42): `Delivery` и `Queue` — постановка со сроком и отмена.
#[tokio::test]
async fn outbound_schedule_suite() {
    let s = Sched::new().await;
    let at = s.clock.now() + Duration::from_secs(3600);
    let list = rcpts(&["x@example.net", "bob@example.org"]);
    tp_testkit::suite::outbound_schedule(s.delivery.as_ref(), TenantId::DEFAULT, &list, at).await;
    assert!(s.queue.blobs().is_empty());
}
