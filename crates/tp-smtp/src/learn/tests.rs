//! Обучение антиспама на фейках: хранилище `tp-mailstore` поверх in-memory контрактов, журнал событий
//! `MemEventLog`, подменный controller; клиент Rspamd — против тестового HTTP-сервера на loopback.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tp_contracts::TenantId;
use tp_contracts::mailstore::{Delivery as MailDelivery, IngestRequest};
use tp_contracts::secrets::SecretRef;
use tp_testkit::{
    Clock, MemBlobIndex, MemBlobStore, MemEventBus, MemEventLog, MemMetadata, MemQuota,
    MemSecretStore, MemTrash, SystemClock,
};

use super::*;

/// Подменный controller: вызовы и заданное число сбоев.
#[derive(Default)]
struct FakeController {
    calls: Mutex<Vec<(Learn, Option<String>, Bytes)>>,
    fail: Mutex<usize>,
}

impl FakeController {
    fn calls(&self) -> Vec<(Learn, Option<String>, Bytes)> {
        lock(&self.calls).clone()
    }
}

#[async_trait]
impl Controller for FakeController {
    async fn learn(&self, learn: Learn, password: Option<&str>, raw: Bytes) -> Result<()> {
        {
            let mut fail = lock(&self.fail);
            if *fail > 0 {
                *fail -= 1;
                return Err(Error::UNAVAILABLE);
            }
        }
        lock(&self.calls).push((learn, password.map(str::to_owned), raw));
        Ok(())
    }
}

struct Env {
    meta: MemMetadata,
    events: MemEventLog,
    store: Arc<dyn MailStore>,
    secrets: MemSecretStore,
    controller: Arc<FakeController>,
    scope: AccountScope,
}

const MAIL: &[u8] = b"From: spammer@example.net\r\nSubject: buy\r\n\r\ncheap\r\n";

impl Env {
    async fn new() -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let blobs = MemBlobStore::new(clock.clone());
        let index = MemBlobIndex::new(clock.clone());
        let events = MemEventLog::new(4);
        let meta = MemMetadata::new(clock.clone())
            .with_blob_index(index.clone())
            .with_event_log(events.clone());
        let store = tp_mailstore::Store::new(tp_mailstore::Deps {
            blobs: Arc::new(blobs),
            index: Arc::new(index),
            messages: Arc::new(meta.clone()),
            changes: Arc::new(meta.clone()),
            trash: Arc::new(MemTrash::new(&meta)),
            quota: Arc::new(MemQuota::new(&meta)),
            bus: Arc::new(MemEventBus::default()),
            hooks: Default::default(),
        });
        let scope = meta.account("alice@example.org").await.expect("account");
        Self {
            meta,
            events,
            store: Arc::new(store),
            secrets: MemSecretStore::new(clock),
            controller: Arc::new(FakeController::default()),
            scope,
        }
    }

    fn learner(&self, cfg: LearnConfig) -> SpamLearner {
        SpamLearner::new(
            cfg,
            self.store.clone(),
            Arc::new(self.meta.clone()),
            Some(Arc::new(self.secrets.clone())),
            self.controller.clone(),
        )
    }

    fn mailbox(&self, role: SpecialUse) -> MailboxId {
        self.meta
            .mailbox_by_role(self.scope, role)
            .expect("mailbox")
    }

    async fn deliver(&self) -> MessageId {
        let ingested = self
            .store
            .ingest(IngestRequest {
                tenant: TenantId::DEFAULT,
                mail_from: "spammer@example.net".into(),
                raw: Box::pin(futures::stream::once(async {
                    Ok(Bytes::from_static(MAIL))
                })),
                deliveries: vec![MailDelivery {
                    account: self.scope.account,
                    mailbox: self.mailbox(SpecialUse::Inbox),
                    extra_headers: Vec::new(),
                    flags: Default::default(),
                }],
                received_at: time::OffsetDateTime::now_utc(),
            })
            .await
            .expect("ingest");
        ingested.delivered[0].message
    }

    async fn move_to(&self, id: MessageId, role: SpecialUse) -> MessageId {
        let moved = self
            .store
            .move_to(self.scope, &[id], self.mailbox(role))
            .await
            .expect("move");
        moved[0].id
    }

    /// События журнала после позиции `skip` (по всем разделам).
    fn batch(&self, skip: usize) -> Vec<LoggedEvent> {
        self.events.all().into_iter().skip(skip).collect()
    }
}

fn enabled() -> LearnConfig {
    LearnConfig {
        enabled: true,
        url: Some("https://rspamd.example.org:11334".into()),
        ..LearnConfig::default()
    }
}

#[tokio::test]
async fn move_to_junk_learns_spam_once() {
    let env = Env::new().await;
    let pw = SecretRef::new("thepost/rspamd", "controller").expect("ref");
    env.secrets.set(&pw, "s3cret");
    let learner = env.learner(LearnConfig {
        password: Some(pw),
        ..enabled()
    });
    let id = env.deliver().await;
    let start = env.events.all().len();
    let junk = env.move_to(id, SpecialUse::Junk).await;
    let batch = env.batch(start);
    learner.handle(&batch).await.expect("handle");
    let calls = env.controller.calls();
    assert_eq!(calls.len(), 1, "одно перемещение — одно обучение");
    assert_eq!(calls[0].0, Learn::Spam);
    assert_eq!(calls[0].1.as_deref(), Some("s3cret"));
    assert!(calls[0].2.ends_with(MAIL));
    // Повтор пачки (доставка не менее одного раза) повторно не обучает.
    learner.handle(&batch).await.expect("handle");
    assert_eq!(env.controller.calls().len(), 1);
    // Из «Спама» во «Входящие» — не спам.
    let start = env.events.all().len();
    env.move_to(junk, SpecialUse::Inbox).await;
    learner.handle(&env.batch(start)).await.expect("handle");
    let calls = env.controller.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].0, Learn::Ham);
}

#[tokio::test]
async fn other_moves_and_disabled_do_not_learn() {
    let env = Env::new().await;
    let a = env.deliver().await;
    let b = env.deliver().await;
    let start = env.events.all().len();
    let a = env.move_to(a, SpecialUse::Junk).await;
    // Выключено по умолчанию: события пропускаются.
    let off = env.learner(LearnConfig::default());
    off.handle(&env.batch(start)).await.expect("handle");
    assert!(env.controller.calls().is_empty());
    // Из «Спама» в корзину и между обычными папками — без обучения; доставка — тоже.
    let start = env.events.all().len();
    env.move_to(a, SpecialUse::Trash).await;
    env.move_to(b, SpecialUse::Archive).await;
    env.deliver().await;
    env.learner(enabled())
        .handle(&env.batch(start))
        .await
        .expect("handle");
    assert!(env.controller.calls().is_empty());
}

#[tokio::test]
async fn controller_failure_retries_batch() {
    let env = Env::new().await;
    let learner = env.learner(enabled());
    let a = env.deliver().await;
    let b = env.deliver().await;
    let start = env.events.all().len();
    env.move_to(a, SpecialUse::Junk).await;
    env.move_to(b, SpecialUse::Junk).await;
    let batch = env.batch(start);
    // Первое письмо обучено, на втором controller недоступен — пачка с ошибкой придёт снова.
    learner.handle(&batch[..1]).await.expect("first");
    *lock(&env.controller.fail) = 1;
    assert!(learner.handle(&batch).await.is_err());
    learner.handle(&batch).await.expect("retry");
    assert_eq!(env.controller.calls().len(), 2, "каждое письмо — один раз");
}

#[test]
fn controller_url_requires_https_except_loopback() {
    let cfg = |url: &str| LearnConfig {
        url: Some(url.into()),
        ..LearnConfig::default()
    };
    assert!(RspamdController::new(&cfg("https://rspamd.example.org:11334")).is_ok());
    assert!(RspamdController::new(&cfg("http://127.0.0.1:11334")).is_ok());
    assert!(RspamdController::new(&cfg("http://[::1]:11334")).is_ok());
    assert!(RspamdController::new(&cfg("http://localhost:11334")).is_ok());
    assert!(matches!(
        RspamdController::new(&cfg("http://rspamd.example.org:11334")),
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        RspamdController::new(&LearnConfig::default()),
        Err(Error::Invalid(_))
    ));
}

/// Тестовый controller: один запрос, ответ `status`; возвращает запрос целиком.
async fn http_once(status: &'static str) -> (u16, tokio::task::JoinHandle<String>) {
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = l.local_addr().expect("addr").port();
    let task = tokio::spawn(async move {
        let (mut s, _) = l.accept().await.expect("accept");
        let mut req = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            let n = s.read(&mut buf).await.expect("read");
            req.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&req);
            if let Some(end) = text.find("\r\n\r\n") {
                let len = text[..end]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                    })
                    .unwrap_or(0);
                if req.len() >= end + 4 + len {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        let resp =
            format!("HTTP/1.1 {status}\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}");
        s.write_all(resp.as_bytes()).await.expect("write");
        String::from_utf8_lossy(&req).into_owned()
    });
    (port, task)
}

#[tokio::test]
async fn rspamd_controller_posts_message_with_password() {
    let (port, task) = http_once("200 OK").await;
    let c = RspamdController::new(&LearnConfig {
        url: Some(format!("http://127.0.0.1:{port}/")),
        timeout: Duration::from_secs(5),
        ..LearnConfig::default()
    })
    .expect("controller");
    c.learn(Learn::Spam, Some("pw"), Bytes::from_static(MAIL))
        .await
        .expect("learn");
    let req = task.await.expect("join");
    assert!(req.starts_with("POST /learnspam HTTP/1.1\r\n"), "{req}");
    assert!(
        req.to_ascii_lowercase().contains("\r\npassword: pw\r\n"),
        "{req}"
    );
    assert!(req.ends_with(std::str::from_utf8(MAIL).expect("utf8")));
    // Неверный пароль — запрет; недоступность — временная ошибка.
    let (port, task) = http_once("403 Forbidden").await;
    let c = RspamdController::new(&LearnConfig {
        url: Some(format!("http://127.0.0.1:{port}")),
        ..LearnConfig::default()
    })
    .expect("controller");
    assert!(matches!(
        c.learn(Learn::Ham, None, Bytes::from_static(MAIL)).await,
        Err(Error::Forbidden)
    ));
    assert!(task.await.expect("join").starts_with("POST /learnham "));
}
