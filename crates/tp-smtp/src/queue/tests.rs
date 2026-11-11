//! Очередь на фейках testkit (`MemQueue`, `MemBlobStore`, `MemBlobIndex`) и управляемых часах: повторы,
//! отказы, срок доставки, учёт ссылок; отправка `MxTransport` на тестовый SMTP-сервер `127.0.0.1`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use mail_auth::hickory_resolver::config::{ResolverConfig, ResolverOpts};
use mail_auth::{MX, RecordSet};
use tokio::net::TcpListener;
use tp_contracts::config::duration::HumanDuration;
use tp_contracts::queue::{DsnNotify, QueueClaim, QueueStore};
use tp_contracts::role::CancellationToken;
use tp_contracts::transport::{DestinationPolicy, TlsRequirement, TransportStore};
use tp_contracts::{BlobKey, TenantId};
use tp_testkit::{
    Clock, ManualClock, MemBlobIndex, MemBlobStore, MemEventBus, MemQueue, MemTransport,
};

use super::send::MxTransport;
use super::*;
use crate::config::OutboundTls;
use crate::routes::Routes;
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

pub(crate) const MAIL: &[u8] =
    b"From: alice@example.org\r\nTo: x@example.net\r\nSubject: hi\r\n\r\nbody\r\n";

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
    // Запись ждёт повтора для c; уведомление об отказе a — отдельной записью.
    assert_eq!(env.queue.len(), 2);
    assert_eq!(env.refs(&blob), 1);
    // Срок доставки истёк: временная неудача становится отказом, запись завершается. По сроку попытки
    // первым идёт уведомление (доставлено), затем письмо.
    env.clock.advance(cfg.expire);
    t.push(RcptResult::Delivered("250 ok".into()));
    t.push(RcptResult::Temporary("452 4.2.2 full".into()));
    let one = QueueConfig {
        concurrency: 1,
        ..cfg.clone()
    };
    assert_eq!(worker.run_once(&one).await.expect("run"), 2);
    assert_eq!(env.refs(&blob), 0);
    // Осталось уведомление об отказе c.
    let left = due(&env).await;
    assert_eq!(left.len(), 1);
    assert!(
        left[0]
            .2
            .contains("c@example.com\r\nAction: failed\r\nStatus: 4.4.7")
    );
}

#[tokio::test]
async fn queue_metrics() {
    let env = Env::new();
    let t = Arc::new(Scripted::default());
    let registry = tp_platform::Metrics::default();
    let metrics = Arc::new(crate::SmtpMetrics::new());
    metrics.register(&registry);
    let worker =
        QueueWorker::new(env.deps.clone(), t.clone(), "mx.example.org").with_metrics(metrics);
    Queue::new(env.deps.clone())
        .enqueue(
            TenantId::DEFAULT,
            "alice@example.org",
            &[
                "a@example.com".into(),
                "b@example.com".into(),
                "c@example.com".into(),
            ],
            Bytes::from_static(MAIL),
        )
        .await
        .expect("enqueue");
    t.push(RcptResult::Delivered("250 ok".into()));
    t.push(RcptResult::Temporary("451 later".into()));
    t.push(RcptResult::Permanent("550 no".into()));
    assert_eq!(worker.run_once(&cfg()).await.expect("run"), 1);
    let text = registry.encode().expect("encode");
    for line in [
        "tp_smtp_queue_claimed_total 1",
        "tp_smtp_queue_inflight 0",
        "tp_smtp_queue_recipients_total{result=\"delivered\"} 1",
        "tp_smtp_queue_recipients_total{result=\"deferred\"} 1",
        "tp_smtp_queue_recipients_total{result=\"failed\"} 1",
        "tp_smtp_queue_delivery_seconds_count 1",
    ] {
        assert!(text.contains(line), "{line} in {text}");
    }
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
    smtp_server_with(rec, false).await
}

/// Тестовый сервер; `broken_tls` — предлагает STARTTLS, но после `220` закрывает соединение (сбой рукопожатия).
pub(crate) async fn smtp_server_with(rec: Arc<Recorder>, broken_tls: bool) -> u16 {
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
                send_limits: None,
                metrics: None,
                screen: None,
            };
            tokio::spawn(async move {
                let _ = Session::new(ctx, conn_info(false), broken_tls)
                    .serve(&mut s, true)
                    .await;
            });
        }
    });
    port
}

pub(crate) fn transport() -> MxTransport {
    let resolver = MessageAuthenticator::new(
        ResolverConfig::from_name_servers(vec![]),
        ResolverOpts::default(),
    )
    .expect("resolver");
    MxTransport::new(resolver, 100).expect("transport")
}

use mail_auth::MessageAuthenticator;

pub(crate) fn mx(t: &MxTransport, domain: &str, hosts: &[(u16, &str)]) {
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

pub(crate) fn a(t: &MxTransport, host: &str, ip: std::net::Ipv4Addr) {
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

fn aaaa(t: &MxTransport, host: &str, ip: std::net::Ipv6Addr) {
    let until = std::time::Instant::now() + Duration::from_secs(3600);
    t.cache().ipv6.insert(
        format!("{host}.").into(),
        RecordSet {
            rrset: Arc::from(vec![ip]),
            dnssec_status: Default::default(),
        },
        until,
    );
}

pub(crate) fn req<'a>(
    cfg: &'a QueueConfig,
    domain: &'a str,
    rcpts: &'a [String],
) -> SendRequest<'a> {
    SendRequest {
        cfg,
        helo: "out.example.org",
        domain,
        from: "alice@example.org",
        rcpts,
        message: MAIL,
        route: None,
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
async fn opportunistic_tls_failure_falls_back_to_plain() {
    let rec = Arc::new(Recorder::default());
    let port = smtp_server_with(rec.clone(), true).await;
    let t = transport();
    mx(&t, "example.net", &[(10, "mx.example.net")]);
    a(&t, "mx.example.net", [127, 0, 0, 1].into());
    let rcpts = vec!["x@example.net".to_owned()];
    // Обязательный TLS: сбой рукопожатия — временная неудача, письмо не передано.
    let strict = QueueConfig {
        port,
        tls: OutboundTls::Require,
        ..cfg()
    };
    let res = t.send(req(&strict, "example.net", &rcpts)).await;
    assert!(matches!(&res[0], RcptResult::Temporary(_)), "{res:?}");
    assert!(rec.mails().is_empty());
    // Оппортунистический: повтор без TLS на том же адресе.
    let cfg = QueueConfig { port, ..cfg() };
    let res = t.send(req(&cfg, "example.net", &rcpts)).await;
    assert!(matches!(&res[0], RcptResult::Delivered(_)), "{res:?}");
    assert_eq!(rec.mails().len(), 1);
}

#[tokio::test]
async fn egress_address_and_family() {
    let rec = Arc::new(Recorder::default());
    let port = smtp_server(rec.clone()).await;
    let t = transport();
    mx(&t, "example.net", &[(10, "mx.example.net")]);
    a(&t, "mx.example.net", [127, 0, 0, 1].into());
    mx(&t, "example.com", &[(10, "mx6.example.com")]);
    aaaa(&t, "mx6.example.com", std::net::Ipv6Addr::LOCALHOST);
    // A-записей нет (пустой ответ в кэше).
    t.cache().ipv4.insert(
        "mx6.example.com.".into(),
        RecordSet {
            rrset: Arc::from(Vec::new()),
            dnssec_status: Default::default(),
        },
        std::time::Instant::now() + Duration::from_secs(3600),
    );
    let cfg = QueueConfig {
        port,
        source_ip: Some([127, 0, 0, 1].into()),
        ..cfg()
    };
    let rcpts = vec!["x@example.net".to_owned()];
    let res = t.send(req(&cfg, "example.net", &rcpts)).await;
    assert!(matches!(&res[0], RcptResult::Delivered(_)), "{res:?}");
    // Узел только с IPv6 при исходящем IPv4: временная неудача, а не «домен не найден».
    let rcpts = vec!["x@example.com".to_owned()];
    let res = t.send(req(&cfg, "example.com", &rcpts)).await;
    assert!(
        matches!(&res[0], RcptResult::Temporary(r) if r.contains("no IPv4 address")),
        "{res:?}"
    );
    assert_eq!(rec.mails().len(), 1);
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

// ---------- Уведомления ----------

/// Записи очереди со сроком попытки сейчас: (отправитель, получатели, письмо).
async fn due(env: &Env) -> Vec<(String, Vec<String>, String)> {
    let claimed = env
        .queue
        .claim(env.clock.now(), 100, Duration::from_secs(1))
        .await
        .expect("claim");
    let mut out = Vec::new();
    for c in claimed {
        let raw = env.deps.read(&c.blob).await.expect("read");
        out.push((
            c.from.clone(),
            c.rcpts.iter().map(|r| r.rcpt.address.clone()).collect(),
            String::from_utf8_lossy(&raw).into_owned(),
        ));
    }
    out
}

#[tokio::test]
async fn failure_dsn_is_queued_before_completion() {
    let env = Env::new();
    let t = Arc::new(Scripted::default());
    let worker = QueueWorker::new(env.deps.clone(), t.clone(), "mx.example.org");
    Queue::new(env.deps.clone())
        .enqueue(
            TenantId::DEFAULT,
            "alice@example.org",
            &["x@example.net".into()],
            Bytes::from_static(MAIL),
        )
        .await
        .expect("enqueue");
    t.push(RcptResult::Permanent("550 5.1.1 no such user".into()));
    worker.run_once(&cfg()).await.expect("run");
    let dsn = due(&env).await;
    assert_eq!(dsn.len(), 1);
    assert_eq!(dsn[0].0, "", "уведомление — с пустым отправителем");
    assert_eq!(dsn[0].1, ["alice@example.org"]);
    assert!(dsn[0].2.contains("Action: failed\r\nStatus: 5.1.1\r\n"));
    assert!(
        dsn[0].2.contains("Subject: hi\r\n"),
        "заголовки исходного письма"
    );
    // Уведомление не доставлено — уведомления о нём нет (пустой отправитель).
    env.clock.advance(Duration::from_secs(1));
    t.push(RcptResult::Permanent("550 5.1.1 no such user".into()));
    worker.run_once(&cfg()).await.expect("run");
    assert!(env.queue.is_empty());
}

#[tokio::test]
async fn delay_dsn_once_and_expiry_status() {
    let env = Env::new();
    let t = Arc::new(Scripted::default());
    let worker = QueueWorker::new(env.deps.clone(), t.clone(), "mx.example.org");
    Queue::new(env.deps.clone())
        .enqueue(
            TenantId::DEFAULT,
            "alice@example.org",
            &["x@example.net".into()],
            Bytes::from_static(MAIL),
        )
        .await
        .expect("enqueue");
    let cfg = QueueConfig {
        retry: vec![HumanDuration(Duration::from_secs(600))],
        delay_notify: Duration::from_secs(25 * 60),
        expire: Duration::from_secs(50 * 60),
        ..cfg()
    };
    let mut delays = 0;
    let mut failures = 0;
    // Попытки в 0, 10, 20, 30, 40, 50 мин: задержка — после 30-й минуты, отказ — на 50-й.
    for _ in 0..6 {
        for _ in 0..2 {
            t.push(RcptResult::Temporary("451 4.3.0 busy".into()));
        }
        worker.run_once(&cfg).await.expect("run");
        for (from, _, raw) in due(&env).await {
            assert_eq!(from, "");
            if raw.contains("Action: delayed") {
                delays += 1;
                assert!(raw.contains("Will-Retry-Until: "));
            } else {
                failures += 1;
                assert!(raw.contains("Action: failed\r\nStatus: 4.4.7\r\n"), "{raw}");
            }
        }
        env.clock.advance(Duration::from_secs(600));
    }
    assert_eq!((delays, failures), (1, 1));
}

#[tokio::test]
async fn success_dsn_only_on_request() {
    let env = Env::new();
    let t = Arc::new(Scripted::default());
    let worker = QueueWorker::new(env.deps.clone(), t.clone(), "mx.example.org");
    let rcpts = vec![
        OutRcpt {
            notify: Some(DsnNotify {
                success: true,
                ..DsnNotify::default()
            }),
            ..OutRcpt::new("x@example.net")
        },
        OutRcpt::new("y@example.net"),
    ];
    env.deps
        .put(
            TenantId::DEFAULT,
            "alice@example.org",
            rcpts,
            Bytes::from_static(MAIL),
        )
        .await
        .expect("put");
    worker.run_once(&cfg()).await.expect("run");
    let dsn = due(&env).await;
    assert_eq!(dsn.len(), 1);
    assert!(
        dsn[0]
            .2
            .contains("Final-Recipient: rfc822; x@example.net\r\nAction: delivered")
    );
    assert!(!dsn[0].2.contains("y@example.net\r\nAction"));
}

// ---------- Планировщик по доменам ----------

/// Домен → (отправок сейчас, наибольшее); домены вызовов по порядку.
type TimedState = (
    std::collections::HashMap<String, (usize, usize)>,
    Vec<String>,
);

/// Отправка с задержкой `delay`; домены из `down` недоступны. Учёт одновременных отправок по доменам.
#[derive(Default)]
struct Timed {
    delay: Duration,
    down: Vec<String>,
    state: Mutex<TimedState>,
}

impl Timed {
    /// Наибольшее число одновременных отправок в домен.
    fn peak(&self, domain: &str) -> usize {
        self.state
            .lock()
            .expect("lock")
            .0
            .get(domain)
            .map_or(0, |d| d.1)
    }

    /// Домены вызовов по порядку.
    fn calls(&self, domain: &str) -> usize {
        let st = self.state.lock().expect("lock");
        st.1.iter().filter(|d| *d == domain).count()
    }
}

#[async_trait]
impl Transport for Timed {
    async fn send(&self, req: SendRequest<'_>) -> Vec<RcptResult> {
        {
            let mut st = self.state.lock().expect("lock");
            st.1.push(req.domain.to_owned());
            let d = st.0.entry(req.domain.to_owned()).or_default();
            d.0 += 1;
            d.1 = d.1.max(d.0);
        }
        tokio::time::sleep(self.delay).await;
        self.state
            .lock()
            .expect("lock")
            .0
            .get_mut(req.domain)
            .expect("domain")
            .0 -= 1;
        let r = if self.down.iter().any(|d| d == req.domain) {
            RcptResult::Temporary(format!("{}: connection refused", req.domain))
        } else {
            RcptResult::Delivered("250 2.0.0 OK".into())
        };
        vec![r; req.rcpts.len()]
    }
}

async fn enqueue(env: &Env, rcpts: &[&str]) {
    let rcpts: Vec<String> = rcpts.iter().map(|r| (*r).to_owned()).collect();
    Queue::new(env.deps.clone())
        .enqueue(
            TenantId::DEFAULT,
            "alice@example.org",
            &rcpts,
            Bytes::from_static(MAIL),
        )
        .await
        .expect("enqueue");
}

#[tokio::test]
async fn unreachable_domain_does_not_delay_others() {
    let env = Env::new();
    let t = Arc::new(Timed {
        down: vec!["dead.example".into()],
        ..Timed::default()
    });
    let worker = QueueWorker::new(env.deps.clone(), t.clone(), "out.example.org");
    enqueue(&env, &["a@dead.example", "b@ok.example"]).await;
    enqueue(&env, &["c@dead.example"]).await;
    enqueue(&env, &["d@ok.example"]).await;
    let cfg = QueueConfig {
        concurrency: 1,
        ..cfg()
    };
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 3);
    // Недоступный домен — одна попытка, затем перенос без соединения; другой домен доставлен.
    assert_eq!(t.calls("dead.example"), 1);
    assert_eq!(t.calls("ok.example"), 2);
    let left = due(&env).await;
    assert!(left.is_empty(), "до конца паузы попыток нет");
    let claimed = env
        .queue
        .claim(
            env.clock.now() + cfg.scheduler.backoff_min,
            10,
            Duration::from_secs(1),
        )
        .await
        .expect("claim");
    // Перенесённый получатель — на конец паузы, получивший попытку — по расписанию повторов (5 мин).
    let rcpts: Vec<&str> = claimed
        .iter()
        .flat_map(|c| c.rcpts.iter().map(|r| r.rcpt.address.as_str()))
        .collect();
    assert_eq!(rcpts, ["c@dead.example"]);
    // Перенос без попытки (`postpone`): попытки и причина неудачи не меняются.
    assert_eq!(claimed[0].rcpts[0].attempts, 0);
    assert_eq!(claimed[0].rcpts[0].last_error, None);
    // Пауза удваивается с каждой неудачей подряд.
    env.clock
        .advance(cfg.scheduler.backoff_min + Duration::from_secs(5));
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 1);
    assert_eq!(t.calls("dead.example"), 2);
    // Новое письмо в домен в течение паузы 2 мин — перенос на её конец.
    enqueue(&env, &["e@dead.example"]).await;
    env.clock.advance(Duration::from_secs(60));
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 1);
    assert_eq!(t.calls("dead.example"), 2);
    let at = env.clock.now() + Duration::from_secs(60);
    let claimed = env
        .queue
        .claim(at - Duration::from_secs(1), 10, Duration::from_secs(1))
        .await
        .expect("claim");
    assert!(claimed.is_empty());
    let claimed = env
        .queue
        .claim(at, 10, Duration::from_secs(1))
        .await
        .expect("claim");
    assert_eq!(claimed[0].rcpts[0].rcpt.address, "e@dead.example");
}

/// Очередь без переноса планировщиком: `postpone` — реализация по умолчанию (`Error::Internal`).
struct NoPostpone(MemQueue);

#[async_trait]
impl QueueStore for NoPostpone {
    async fn enqueue(&self, msg: OutMessage) -> Result<tp_contracts::QueueId> {
        self.0.enqueue(msg).await
    }

    async fn claim(
        &self,
        now: OffsetDateTime,
        limit: usize,
        lease: Duration,
    ) -> Result<Vec<Claimed>> {
        self.0.claim(now, limit, lease).await
    }

    async fn renew(&self, claim: &QueueClaim, until: OffsetDateTime) -> Result<()> {
        self.0.renew(claim, until).await
    }

    async fn reschedule(
        &self,
        claim: &QueueClaim,
        rcpt: &str,
        at: OffsetDateTime,
        reason: &str,
    ) -> Result<()> {
        self.0.reschedule(claim, rcpt, at, reason).await
    }

    async fn complete(&self, claim: &QueueClaim, rcpt: &str, status: RcptStatus) -> Result<bool> {
        self.0.complete(claim, rcpt, status).await
    }
}

#[tokio::test]
async fn deferred_recipients_are_postponed_without_attempt() {
    let env = Env::new();
    let t = Arc::new(Timed {
        down: vec!["dead.example".into()],
        ..Timed::default()
    });
    let worker = QueueWorker::new(env.deps.clone(), t.clone(), "out.example.org");
    let cfg = QueueConfig {
        concurrency: 1,
        ..cfg()
    };
    enqueue(&env, &["a@dead.example"]).await;
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 1);
    // Письмо в домен на паузе — перенос на её конец без попытки: `attempts` и причина неудачи не меняются.
    enqueue(&env, &["b@dead.example"]).await;
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 1);
    assert_eq!(t.calls("dead.example"), 1);
    let left = env
        .queue
        .claim(
            env.clock.now() + cfg.scheduler.backoff_max,
            10,
            Duration::from_secs(1),
        )
        .await
        .expect("claim");
    let b: Vec<_> = left
        .iter()
        .flat_map(|c| c.rcpts.iter())
        .filter(|r| r.rcpt.address == "b@dead.example")
        .collect();
    assert_eq!(b.len(), 1);
    assert_eq!((b[0].attempts, b[0].last_error.as_deref()), (0, None));
    // Очередь без переноса: повтор как после неудачи (тот же срок, попытка учтена).
    let queue = MemQueue::new(env.clock.shared());
    let deps = QueueDeps {
        store: Arc::new(NoPostpone(queue.clone())),
        ..env.deps.clone()
    };
    // Тот же планировщик: домен ещё на паузе.
    let mut other = QueueWorker::new(deps.clone(), t.clone(), "out.example.org");
    other.sched = worker.sched.clone();
    Queue::new(deps)
        .enqueue(
            TenantId::DEFAULT,
            "alice@example.org",
            &["c@dead.example".to_owned()],
            Bytes::from_static(MAIL),
        )
        .await
        .expect("enqueue");
    assert_eq!(other.run_once(&cfg).await.expect("run"), 1);
    assert_eq!(t.calls("dead.example"), 1);
    let left = queue
        .claim(
            env.clock.now() + cfg.scheduler.backoff_max,
            10,
            Duration::from_secs(1),
        )
        .await
        .expect("claim");
    assert_eq!(left[0].rcpts[0].attempts, 1);
    assert!(
        left[0].rcpts[0]
            .last_error
            .as_deref()
            .is_some_and(|e| e.contains("unreachable"))
    );
}

#[tokio::test]
async fn domain_concurrency_limit_and_slow_start() {
    let env = Env::new();
    let t = Arc::new(Timed {
        delay: Duration::from_millis(50),
        ..Timed::default()
    });
    let worker = QueueWorker::new(env.deps.clone(), t.clone(), "out.example.org");
    for i in 0..12 {
        enqueue(
            &env,
            &[
                &format!("u{i}@busy.example"),
                &format!("u{i}@other.example"),
            ],
        )
        .await;
    }
    let mut cfg = QueueConfig {
        concurrency: 12,
        ..cfg()
    };
    cfg.scheduler.domain_concurrency = 3;
    cfg.scheduler.initial_concurrency = 1;
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 12);
    assert!(env.queue.is_empty());
    assert_eq!(t.calls("busy.example"), 12);
    assert_eq!(t.peak("busy.example"), 3, "лимит параллельности домена");
    // Без ожидания места: медленный старт нового домена — одна отправка, остальные перенесены.
    for i in 0..4 {
        enqueue(&env, &[&format!("u{i}@new.example")]).await;
    }
    cfg.scheduler.wait = Duration::ZERO;
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 4);
    assert_eq!(t.calls("new.example"), 1);
    assert_eq!(env.queue.len(), 3);
    let claimed = env
        .queue
        .claim(
            env.clock.now() + cfg.scheduler.busy_delay,
            10,
            Duration::from_secs(1),
        )
        .await
        .expect("claim");
    assert_eq!(claimed.len(), 3, "перенос на busy_delay");
}

#[tokio::test]
async fn internal_pool_is_separate() {
    let env = Env::new();
    let t = Arc::new(Timed {
        delay: Duration::from_millis(50),
        ..Timed::default()
    });
    let worker = QueueWorker::new(env.deps.clone(), t.clone(), "out.example.org");
    for i in 0..3 {
        enqueue(&env, &[&format!("u{i}@ext{i}.example")]).await;
    }
    enqueue(&env, &["x@corp.example"]).await;
    let mut cfg = QueueConfig {
        concurrency: 8,
        ..cfg()
    };
    cfg.scheduler.wait = Duration::ZERO;
    cfg.scheduler.external_concurrency = 1;
    cfg.scheduler.internal_domains = vec!["corp.example".into()];
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 4);
    // Пул внешней доставки занят одной отправкой, внутренняя — в своём пуле.
    assert_eq!(t.calls("corp.example"), 1);
    let external: usize = (0..3).map(|i| t.calls(&format!("ext{i}.example"))).sum();
    assert_eq!(external, 1);
    assert_eq!(env.queue.len(), 2);
}

// ---------- Политики доменов назначения ----------

fn route(pattern: &str, tls: TlsRequirement, smart_host: Option<String>) -> DestinationPolicy {
    DestinationPolicy {
        pattern: pattern.into(),
        tls,
        smart_host,
        egress_ip: None,
    }
}

#[tokio::test]
async fn required_tls_route_refuses_plain_server() {
    let rec = Arc::new(Recorder::default());
    let port = smtp_server(rec.clone()).await;
    let t = transport();
    mx(&t, "partner.example", &[(10, "mx.partner.example")]);
    a(&t, "mx.partner.example", [127, 0, 0, 1].into());
    let cfg = QueueConfig { port, ..cfg() };
    let rcpts = vec!["x@partner.example".to_owned()];
    // Сервер без STARTTLS: обязательный TLS — временная неудача, открытым текстом письмо не передаётся.
    let p = route("partner.example", TlsRequirement::Required, None);
    let res = t
        .send(SendRequest {
            route: Some(&p),
            ..req(&cfg, "partner.example", &rcpts)
        })
        .await;
    assert!(
        matches!(&res[0], RcptResult::Temporary(r) if r.contains("STARTTLS not offered")),
        "{res:?}"
    );
    let p = route(
        "partner.example",
        TlsRequirement::Pinned {
            spki_sha256: vec!["0".repeat(64)],
        },
        None,
    );
    let res = t
        .send(SendRequest {
            route: Some(&p),
            ..req(&cfg, "partner.example", &rcpts)
        })
        .await;
    assert!(matches!(&res[0], RcptResult::Temporary(_)), "{res:?}");
    assert!(rec.mails().is_empty());
    // Оппортунистическая политика — доставка открытым текстом.
    let p = route("partner.example", TlsRequirement::Opportunistic, None);
    let res = t
        .send(SendRequest {
            route: Some(&p),
            ..req(&cfg, "partner.example", &rcpts)
        })
        .await;
    assert!(matches!(&res[0], RcptResult::Delivered(_)), "{res:?}");
    assert_eq!(rec.mails().len(), 1);
}

#[tokio::test]
async fn smart_host_route_and_reload_on_signal() {
    let rec = Arc::new(Recorder::default());
    let port = smtp_server(rec.clone()).await;
    let env = Env::new();
    let bus: Arc<dyn tp_contracts::bus::EventBus> = Arc::new(MemEventBus::default());
    let store = MemTransport::new().with_bus(bus.clone());
    let tenant = TenantId::DEFAULT;
    // Все внешние домены — через промежуточный узел (relayhost); MX доменов неизвестны.
    let v = store
        .put_route(
            tenant,
            0,
            &route(
                "*",
                TlsRequirement::Opportunistic,
                Some(format!("127.0.0.1:{port}")),
            ),
        )
        .await
        .expect("route");
    // Принятый домен кластера — внутренняя доставка без политики (его MX неизвестен: временная неудача).
    store
        .put_domain(
            0,
            &tp_contracts::transport::AcceptedDomain {
                id: tp_contracts::DomainId::new(),
                tenant: TenantId::new(),
                name: "corp.example".into(),
                kind: Default::default(),
            },
        )
        .await
        .expect("domain");
    let routes = Arc::new(Routes::new(Arc::new(store.clone())));
    let stop = CancellationToken::new();
    tokio::spawn(
        routes
            .clone()
            .watch(bus.clone(), stop.clone(), Duration::from_millis(50)),
    );
    let worker = QueueWorker::new(env.deps.clone(), Arc::new(transport()), "out.example.org")
        .with_routes(routes.clone(), Some(bus));
    enqueue(&env, &["x@example.net", "y@example.com", "z@corp.example"]).await;
    let cfg = cfg();
    assert_eq!(worker.run_once(&cfg).await.expect("run"), 1);
    assert_eq!(rec.mails().len(), 2, "по соединению на внешний домен");
    assert_eq!(env.queue.len(), 1, "внутренний домен ждёт повтора");
    // Политика изменена: обязательный TLS на узле без STARTTLS — отказ после сигнала.
    let v2 = store
        .put_route(
            tenant,
            v,
            &route(
                "*",
                TlsRequirement::Required,
                Some(format!("127.0.0.1:{port}")),
            ),
        )
        .await
        .expect("route");
    tokio::time::timeout(Duration::from_secs(5), async {
        while routes.tenant(tenant).await.expect("tenant").version != v2 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("политики перечитаны по сигналу");
    enqueue(&env, &["w@example.org"]).await;
    worker.run_once(&cfg).await.expect("run");
    assert_eq!(rec.mails().len(), 2);
    stop.cancel();
}

/// Домен внутренней ретрансляции (SMTP-21): получатели — на `next_hop` домена, MX не ищется.
#[tokio::test]
async fn internal_relay_goes_to_next_hop() {
    let rec = Arc::new(Recorder::default());
    let port = smtp_server(rec.clone()).await;
    let env = Env::new();
    let store = MemTransport::new();
    store
        .put_domain(
            0,
            &tp_contracts::transport::AcceptedDomain {
                id: tp_contracts::DomainId::new(),
                tenant: TenantId::DEFAULT,
                name: "legacy.example".into(),
                kind: tp_contracts::transport::DomainKind::InternalRelay {
                    next_hop: format!("127.0.0.1:{port}"),
                },
            },
        )
        .await
        .expect("domain");
    let routes = Arc::new(Routes::new(Arc::new(store)));
    let worker = QueueWorker::new(env.deps.clone(), Arc::new(transport()), "out.example.org")
        .with_routes(routes, None);
    enqueue(&env, &["old@legacy.example"]).await;
    assert_eq!(worker.run_once(&cfg()).await.expect("run"), 1);
    assert_eq!(rec.mails().len(), 1, "доставлено на next_hop");
    assert_eq!(env.queue.len(), 0);
}

/// Контракт `Outbound` (CTR-28): `Queue` — все получатели в очереди, статусы в порядке запроса.
#[tokio::test]
async fn outbound_suite() {
    let env = Env::new();
    let q = Queue::new(env.deps.clone());
    let rcpts = vec!["x@example.net".to_owned(), "Y@example.com".to_owned()];
    tp_testkit::suite::outbound_submit(&q, TenantId::DEFAULT, &rcpts).await;
    assert_eq!(env.queue.len(), 2);
}
