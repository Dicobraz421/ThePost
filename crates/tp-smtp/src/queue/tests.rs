//! Очередь на фейках testkit (`MemQueue`, `MemBlobStore`, `MemBlobIndex`) и управляемых часах: повторы,
//! отказы, срок доставки, учёт ссылок; отправка `MxTransport` на тестовый SMTP-сервер `127.0.0.1`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use mail_auth::hickory_resolver::config::{ResolverConfig, ResolverOpts};
use mail_auth::{MX, RecordSet};
use tokio::net::TcpListener;
use tp_contracts::queue::QueueStore;
use tp_contracts::{BlobKey, TenantId};
use tp_testkit::{Clock, ManualClock, MemBlobIndex, MemBlobStore, MemQueue};

use super::send::MxTransport;
use super::*;
use crate::server::tests::{Recorder, conn_info};
use crate::server::{Mode, Session, SessionCtx};
use mail_auth::ResolverCache;

/// Отправка по сценарию: очередной ответ на каждого получателя; вызовы — в `calls`.
#[derive(Default)]
pub(crate) struct Scripted {
    pub(crate) answers: Mutex<VecDeque<RcptResult>>,
    pub(crate) calls: Mutex<Vec<(String, Vec<String>, Bytes)>>,
}

impl Scripted {
    pub(crate) fn push(&self, r: RcptResult) {
        self.answers.lock().expect("lock").push_back(r);
    }
}

#[async_trait]
impl Transport for Scripted {
    async fn send(&self, req: SendRequest<'_>) -> Vec<RcptResult> {
        self.calls.lock().expect("lock").push((
            req.from.to_owned(),
            req.rcpts.to_vec(),
            Bytes::copy_from_slice(req.message),
        ));
        let mut a = self.answers.lock().expect("lock");
        req.rcpts
            .iter()
            .map(|_| {
                a.pop_front()
                    .unwrap_or(RcptResult::Delivered("250 2.0.0 OK".into()))
            })
            .collect()
    }
}

pub(crate) struct Env {
    pub(crate) clock: ManualClock,
    pub(crate) queue: MemQueue,
    pub(crate) blobs: MemBlobStore,
    pub(crate) index: MemBlobIndex,
    pub(crate) deps: QueueDeps,
}

impl Env {
    pub(crate) fn new() -> Self {
        let clock = ManualClock::default();
        let queue = MemQueue::new(clock.shared());
        let blobs = MemBlobStore::new(clock.shared());
        let index = MemBlobIndex::new(clock.shared());
        let c = clock.clone();
        let deps = QueueDeps {
            store: Arc::new(queue.clone()),
            blobs: Arc::new(blobs.clone()),
            index: Arc::new(index.clone()),
            now: Arc::new(move || c.now()),
        };
        Self {
            clock,
            queue,
            blobs,
            index,
            deps,
        }
    }

    pub(crate) fn refs(&self, key: &BlobKey) -> u32 {
        self.index
            .row(key.tenant, &key.hash)
            .map_or(0, |r| r.refcnt)
    }
}

const MAIL: &[u8] = b"From: alice@example.org\r\nTo: x@example.net\r\nSubject: hi\r\n\r\nbody\r\n";

fn cfg() -> QueueConfig {
    QueueConfig::default()
}

#[tokio::test]
async fn retries_by_schedule_then_success() {
    let env = Env::new();
    let t = Arc::new(Scripted::default());
    let worker = QueueWorker::new(env.deps.clone(), t.clone(), "mx.example.org");
    let q = Queue::new(env.deps.clone());
    q.enqueue(
        TenantId::DEFAULT,
        "alice@example.org",
        &["x@example.net".into(), "X@example.net".into()],
        Bytes::from_static(MAIL),
    )
    .await
    .expect("enqueue");
    let blob = env.queue.blobs()[0];
    assert_eq!(env.refs(&blob), 1);
    assert!(env.blobs.contains(&blob));
    let cfg = cfg();
    t.push(RcptResult::Temporary("451 4.3.0 busy".into()));
    t.push(RcptResult::Temporary("connection refused".into()));
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 1);
    // Повтор ещё не наступил.
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 0);
    env.clock.advance(Duration::from_secs(4 * 60));
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 0);
    env.clock.advance(Duration::from_secs(60));
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 1);
    // Вторая неудача — следующий интервал (15 мин).
    env.clock.advance(Duration::from_secs(14 * 60));
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 0);
    env.clock.advance(Duration::from_secs(60));
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 1);
    assert!(env.queue.is_empty());
    assert_eq!(env.refs(&blob), 0, "ссылка очереди снята");
    let calls = t.calls.lock().expect("lock").clone();
    assert_eq!(calls.len(), 3);
    // Повтор адреса в разном регистре — один получатель.
    assert_eq!(calls[0].1, ["x@example.net"]);
    assert_eq!(calls[0].2, Bytes::from_static(MAIL));
}

#[tokio::test]
async fn recipients_grouped_by_domain_and_final_statuses() {
    let env = Env::new();
    let t = Arc::new(Scripted::default());
    let worker = QueueWorker::new(env.deps.clone(), t.clone(), "mx.example.org");
    Queue::new(env.deps.clone())
        .enqueue(
            TenantId::DEFAULT,
            "alice@example.org",
            &[
                "a@example.com".into(),
                "b@example.net".into(),
                "c@example.com".into(),
            ],
            Bytes::from_static(MAIL),
        )
        .await
        .expect("enqueue");
    let blob = env.queue.blobs()[0];
    // example.com (a, c): a — отказ, c — повтор; example.net (b) — доставлено.
    t.push(RcptResult::Permanent("550 5.1.1 no such user".into()));
    t.push(RcptResult::Temporary("452 4.2.2 full".into()));
    t.push(RcptResult::Delivered("250 ok".into()));
    let cfg = cfg();
    worker.run_once(&cfg).await.expect("run");
    let calls = t.calls.lock().expect("lock").clone();
    assert_eq!(calls[0].1, ["a@example.com", "c@example.com"]);
    assert_eq!(calls[1].1, ["b@example.net"]);
    assert_eq!(env.queue.len(), 1);
    assert_eq!(env.refs(&blob), 1);
    // Срок доставки истёк: временная неудача становится отказом, запись завершается.
    env.clock.advance(cfg.expire);
    t.push(RcptResult::Temporary("452 4.2.2 full".into()));
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 1);
    assert!(env.queue.is_empty());
    assert_eq!(env.refs(&blob), 0);
}

#[tokio::test]
async fn stale_claim_is_skipped() {
    let env = Env::new();
    let t = Arc::new(Scripted::default());
    Queue::new(env.deps.clone())
        .enqueue(
            TenantId::DEFAULT,
            "alice@example.org",
            &["a@example.com".into()],
            Bytes::from_static(MAIL),
        )
        .await
        .expect("enqueue");
    // Другой узел захватил запись, лиза истекла, запись захвачена снова: старый захват устарел.
    let cfg = cfg();
    let now = env.clock.now();
    let old = env.queue.claim(now, 10, cfg.lease).await.expect("claim");
    env.clock.advance(cfg.lease + Duration::from_secs(1));
    let worker = QueueWorker::new(env.deps.clone(), t.clone(), "mx.example.org");
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 1);
    assert!(env.queue.is_empty());
    let r = env
        .queue
        .complete(
            &old[0].claim,
            "a@example.com",
            RcptStatus::Delivered { reply: "x".into() },
        )
        .await;
    assert_eq!(r, Err(Error::Conflict));
}

#[tokio::test]
async fn enqueue_failure_releases_reference() {
    let env = Env::new();
    let q = Queue::new(env.deps.clone());
    let r = q
        .enqueue(
            TenantId::DEFAULT,
            "alice@example.org",
            &[],
            Bytes::from_static(MAIL),
        )
        .await;
    assert!(matches!(r, Err(Error::Invalid(_))));
    let hash = tp_mailstore::split::hash(MAIL);
    assert_eq!(
        env.index
            .row(TenantId::DEFAULT, &hash)
            .map_or(0, |r| r.refcnt),
        0
    );
}

// ---------- MxTransport ----------

/// Тестовый SMTP-сервер (сессия `tp-smtp` с обработчиком-регистратором) на `127.0.0.1`.
pub(crate) async fn smtp_server(rec: Arc<Recorder>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move {
        while let Ok((mut s, _)) = listener.accept().await {
            let ctx = SessionCtx {
                mode: Mode::Inbound,
                config: Arc::new(SmtpConfig::default()),
                handler: rec.clone(),
                sasl: None,
                limiter: None,
            };
            tokio::spawn(async move {
                let _ = Session::new(ctx, conn_info(false), false)
                    .serve(&mut s, true)
                    .await;
            });
        }
    });
    port
}

fn transport() -> MxTransport {
    let resolver = MessageAuthenticator::new(
        ResolverConfig::from_name_servers(vec![]),
        ResolverOpts::default(),
    )
    .expect("resolver");
    MxTransport::new(resolver, 100).expect("transport")
}

use mail_auth::MessageAuthenticator;

fn mx(t: &MxTransport, domain: &str, hosts: &[(u16, &str)]) {
    let until = std::time::Instant::now() + Duration::from_secs(3600);
    let rrset: Arc<[MX]> = hosts
        .iter()
        .map(|(pref, h)| MX {
            preference: *pref,
            exchanges: vec![Box::<str>::from(*h)].into_boxed_slice(),
        })
        .collect();
    t.cache().mx.insert(
        format!("{domain}.").into(),
        RecordSet {
            rrset,
            dnssec_status: Default::default(),
        },
        until,
    );
}

fn a(t: &MxTransport, host: &str, ip: std::net::Ipv4Addr) {
    let until = std::time::Instant::now() + Duration::from_secs(3600);
    t.cache().ipv4.insert(
        format!("{host}.").into(),
        RecordSet {
            rrset: Arc::from(vec![ip]),
            dnssec_status: Default::default(),
        },
        until,
    );
}

fn req<'a>(cfg: &'a QueueConfig, domain: &'a str, rcpts: &'a [String]) -> SendRequest<'a> {
    SendRequest {
        cfg,
        helo: "out.example.org",
        domain,
        from: "alice@example.org",
        rcpts,
        message: MAIL,
    }
}

#[tokio::test]
async fn mx_delivery_per_recipient_replies() {
    let rec = Arc::new(Recorder::default());
    rec.reject
        .lock()
        .expect("lock")
        .push("nobody@example.net".into());
    let port = smtp_server(rec.clone()).await;
    let t = transport();
    // Первый MX без адреса, второй — тестовый сервер.
    mx(
        &t,
        "example.net",
        &[(10, "mx1.example.net"), (20, "mx2.example.net")],
    );
    a(&t, "mx2.example.net", [127, 0, 0, 1].into());
    let cfg = QueueConfig { port, ..cfg() };
    let rcpts = vec!["x@example.net".to_owned(), "nobody@example.net".to_owned()];
    let res = t.send(req(&cfg, "example.net", &rcpts)).await;
    assert!(
        matches!(&res[0], RcptResult::Delivered(r) if r.starts_with("250 2.0.0")),
        "{res:?}"
    );
    assert!(
        matches!(&res[1], RcptResult::Permanent(r) if r.starts_with("550 5.1.1")),
        "{res:?}"
    );
    let mails = rec.mails();
    assert_eq!(mails.len(), 1);
    assert_eq!(mails[0].0.mail_from, "alice@example.org");
    assert_eq!(mails[0].0.rcpts, ["x@example.net"]);
    assert_eq!(mails[0].1, Bytes::from_static(MAIL));
}

#[tokio::test]
async fn null_mx_unknown_domain_and_unreachable_relay() {
    let t = transport();
    let cfg = cfg();
    let rcpts = vec!["x@example.com".to_owned()];
    mx(&t, "example.com", &[(0, ".")]);
    let res = t.send(req(&cfg, "example.com", &rcpts)).await;
    assert!(
        matches!(&res[0], RcptResult::Permanent(r) if r.starts_with("556 5.1.10")),
        "{res:?}"
    );
    // Резолвер без серверов: MX не получен — временная неудача.
    let res = t.send(req(&cfg, "example.org", &rcpts)).await;
    assert!(matches!(&res[0], RcptResult::Temporary(_)), "{res:?}");
    // Ретранслятор не отвечает — временная неудача.
    let closed = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = closed.local_addr().expect("addr").port();
    drop(closed);
    let cfg = QueueConfig {
        relay: Some(format!("127.0.0.1:{port}")),
        timeout: Duration::from_secs(3),
        ..cfg
    };
    let res = t.send(req(&cfg, "example.com", &rcpts)).await;
    assert!(matches!(&res[0], RcptResult::Temporary(_)), "{res:?}");
}

#[tokio::test]
async fn worker_delivers_through_relay() {
    let rec = Arc::new(Recorder::default());
    let port = smtp_server(rec.clone()).await;
    let env = Env::new();
    let worker = QueueWorker::new(env.deps.clone(), Arc::new(transport()), "out.example.org");
    Queue::new(env.deps.clone())
        .enqueue(
            TenantId::DEFAULT,
            "alice@example.org",
            &["x@example.net".into(), "y@example.com".into()],
            Bytes::from_static(MAIL),
        )
        .await
        .expect("enqueue");
    let cfg = QueueConfig {
        relay: Some(format!("127.0.0.1:{port}")),
        ..cfg()
    };
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 1);
    assert!(env.queue.is_empty());
    // По соединению на домен.
    assert_eq!(rec.mails().len(), 2);
}
