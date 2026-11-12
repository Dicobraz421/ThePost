//! Локальная доставка на фейках tp-testkit: хранилище `tp-mailstore` поверх in-memory контрактов.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures::StreamExt;
use tp_contracts::auth::{DirAccount, DirAlias, DirDomain, DirGroup};
use tp_contracts::ext::{Augment, CopyTo, Decision, DecisionHooks};
use tp_contracts::mailstore::MailStore;
use tp_contracts::meta::{AccountScope, MessageQuery, MessageStore, SpecialUse};
use tp_contracts::transport::TransportStore;
use tp_contracts::{Result, TenantId};
use tp_testkit::{
    BlobOp, Clock, FakeDirectory, MemBlobIndex, MemBlobStore, MemEventBus, MemGroups, MemMetadata,
    MemQuota, MemTransport, MemTrash, RecordingDecisionHook, SystemClock,
};

use super::*;
use crate::server::tests::{Client, conn_info};
use crate::server::{Session, SessionCtx};

/// Очередь исходящей почты, запоминающая постановки.
#[derive(Default)]
struct Queue(Mutex<Vec<(String, Vec<String>, Bytes)>>);

#[async_trait]
impl Outbound for Queue {
    async fn enqueue(
        &self,
        _tenant: TenantId,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<()> {
        self.0
            .lock()
            .expect("lock")
            .push((mail_from.to_owned(), rcpts.to_vec(), raw));
        Ok(())
    }
}

struct Env {
    meta: MemMetadata,
    blobs: MemBlobStore,
    index: MemBlobIndex,
    groups: MemGroups,
    quota: MemQuota,
    store: Arc<dyn MailStore>,
    dir: FakeDirectory,
    /// Настройки транспорта: `example.org` — принятый домен организации.
    transport: MemTransport,
    queue: Arc<Queue>,
}

fn dir_account(id: &str, login: &str, emails: &[&str]) -> DirAccount {
    DirAccount {
        external_id: id.into(),
        login: login.into(),
        display_name: login.into(),
        emails: emails.iter().map(|e| (*e).to_owned()).collect(),
        enabled: true,
        attributes: BTreeMap::new(),
        org: Default::default(),
    }
}

impl Env {
    async fn new(hooks: DecisionHooks) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let blobs = MemBlobStore::new(clock.clone());
        let index = MemBlobIndex::new(clock.clone());
        let meta = MemMetadata::new(clock).with_blob_index(index.clone());
        let quota = MemQuota::new(&meta);
        let store = tp_mailstore::Store::new(tp_mailstore::Deps {
            blobs: Arc::new(blobs.clone()),
            index: Arc::new(index.clone()),
            messages: Arc::new(meta.clone()),
            changes: Arc::new(meta.clone()),
            trash: Arc::new(MemTrash::new(&meta)),
            quota: Arc::new(quota.clone()),
            bus: Arc::new(MemEventBus::default()),
            hooks,
        });
        for login in ["alice@example.org", "bob@example.org", "carol@example.org"] {
            meta.account(login).await.expect("account");
        }
        let dir = FakeDirectory::default();
        let t = TenantId::DEFAULT;
        dir.set_domains(
            t,
            vec![DirDomain {
                name: "Example.org".into(),
                default: true,
            }],
        );
        dir.upsert_account(
            t,
            dir_account(
                "a",
                "alice@example.org",
                &["alice@example.org", "a.smith@example.org"],
            ),
        );
        dir.upsert_account(t, dir_account("b", "bob@example.org", &["bob@example.org"]));
        dir.upsert_account(
            t,
            dir_account("c", "carol@example.org", &["carol@example.org"]),
        );
        dir.upsert_group(
            t,
            DirGroup {
                external_id: "g-all".into(),
                name: "all".into(),
                email: Some("all@example.org".into()),
                members: vec!["g-team".into(), "c".into()],
            },
        );
        dir.upsert_group(
            t,
            DirGroup {
                external_id: "g-team".into(),
                name: "team".into(),
                email: Some("team@example.org".into()),
                members: vec!["a".into(), "b".into(), "g-all".into()],
            },
        );
        dir.set_aliases(
            t,
            vec![DirAlias {
                address: "postmaster@example.org".into(),
                target: "a".into(),
            }],
        );
        let transport = MemTransport::new();
        transport
            .put_domain(
                0,
                &AcceptedDomain {
                    id: tp_contracts::DomainId::new(),
                    tenant: t,
                    name: "example.org".into(),
                    kind: DomainKind::Authoritative,
                },
            )
            .await
            .expect("accepted domain");
        Self {
            meta,
            blobs,
            index,
            groups: MemGroups::new(),
            quota,
            store: Arc::new(store),
            dir,
            transport,
            queue: Arc::new(Queue::default()),
        }
    }

    /// Снимки настроек транспорта окружения.
    fn routes(&self) -> Arc<Routes> {
        Arc::new(Routes::new(Arc::new(self.transport.clone())))
    }

    fn delivery(&self, outbound: bool) -> Delivery {
        self.delivery_with(
            outbound,
            SmtpConfig {
                hostname: "mx.example.org".into(),
                ..SmtpConfig::default()
            },
        )
    }

    fn delivery_with(&self, outbound: bool, cfg: SmtpConfig) -> Delivery {
        Delivery::new(
            Deps {
                tenant: TenantId::DEFAULT,
                store: self.store.clone(),
                accounts: Arc::new(self.meta.clone()),
                mailboxes: Arc::new(self.meta.clone()),
                directory: Some(Arc::new(self.dir.clone())),
                quota: Some(Arc::new(self.quota.clone())),
                outbound: outbound.then(|| self.queue.clone() as Arc<dyn Outbound>),
                authcheck: None,
                milter: None,
                hooks: DecisionHooks::default(),
            },
            &cfg,
        )
        .with_transport(self.routes())
    }

    async fn scope(&self, login: &str) -> AccountScope {
        let acc = self
            .meta
            .by_login(TenantId::DEFAULT, login)
            .await
            .expect("by_login")
            .expect("account");
        AccountScope {
            tenant: acc.tenant,
            account: acc.id,
        }
    }

    /// Письма во «Входящих» учётной записи (исходный текст).
    async fn inbox(&self, login: &str) -> Vec<String> {
        let scope = self.scope(login).await;
        let inbox = self
            .meta
            .mailbox_by_role(scope, SpecialUse::Inbox)
            .expect("inbox");
        let mut out = Vec::new();
        for m in MessageStore::list(&self.meta, scope, inbox, MessageQuery::All, 100)
            .await
            .expect("list")
        {
            let mut s = self
                .store
                .fetch_raw(scope, m.id, None)
                .await
                .expect("fetch");
            let mut raw = Vec::new();
            while let Some(chunk) = s.next().await {
                raw.extend_from_slice(&chunk.expect("chunk"));
            }
            out.push(String::from_utf8(raw).expect("utf8"));
        }
        out
    }
}

fn envelope(mode: Mode, rcpts: &[&str]) -> Envelope {
    Envelope {
        id: "test1".into(),
        mode,
        conn: conn_info(true),
        helo: "client.example.net".into(),
        esmtp: true,
        principal: None,
        mail_from: "sender@example.net".into(),
        rcpts: rcpts.iter().map(|r| (*r).to_owned()).collect(),
        smtputf8: false,
        started_at: OffsetDateTime::now_utc(),
        source: None,
        gateway: None,
    }
}

const MAIL: &[u8] = b"From: sender@example.net\r\nSubject: hello\r\n\r\nbody\r\n";

#[tokio::test]
async fn resolves_accounts_aliases_and_groups() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.delivery(false);
    let logins = |r: Resolved| match r {
        Resolved::Local(a) => a.into_iter().map(|a| a.login).collect::<Vec<_>>(),
        other => panic!("not local: {other:?}"),
    };
    assert_eq!(
        logins(d.resolve("Alice@Example.org").await.expect("r")),
        ["alice@example.org"]
    );
    assert_eq!(
        logins(d.resolve("a.smith@example.org").await.expect("r")),
        ["alice@example.org"]
    );
    assert_eq!(
        logins(d.resolve("postmaster@example.org").await.expect("r")),
        ["alice@example.org"]
    );
    // Вложенные группы с циклом раскрываются один раз.
    let mut team = logins(d.resolve("team@example.org").await.expect("r"));
    team.sort();
    assert_eq!(
        team,
        ["alice@example.org", "bob@example.org", "carol@example.org"]
    );
    assert_eq!(
        d.resolve("nobody@example.org").await.expect("r"),
        Resolved::Unknown
    );
    assert_eq!(
        d.resolve("x@example.net").await.expect("r"),
        Resolved::External
    );
    assert_eq!(d.resolve("no-domain").await.expect("r"), Resolved::Unknown);
}

/// Правило переписывания для обеих сторон конверта.
fn rewrite_rule(from: &str, to: &str) -> tp_contracts::transport::RewriteRule {
    use tp_contracts::transport::AddressPattern;
    tp_contracts::transport::RewriteRule {
        from: AddressPattern::new(from).expect("from"),
        to: AddressPattern::new(to).expect("to"),
        sender: true,
        recipient: true,
    }
}

#[tokio::test]
async fn subaddress_catch_all_and_rewrite() {
    let env = Env::new(DecisionHooks::default()).await;
    env.dir.set_aliases(
        TenantId::DEFAULT,
        vec![DirAlias {
            address: "@example.org".into(),
            target: "b".into(),
        }],
    );
    // Таблица переписывания — в настройках транспорта организации (SMTP-29).
    let transport = env.transport.clone();
    let mut v = 0;
    for (from, to) in [
        ("@old.example.org", "@example.org"),
        ("partner@example.net", "partner@example.com"),
    ] {
        v = transport
            .put_rewrite(TenantId::DEFAULT, v, &rewrite_rule(from, to))
            .await
            .expect("rewrite");
    }
    let d = env.delivery_with(
        true,
        SmtpConfig {
            hostname: "mx.example.org".into(),
            ..SmtpConfig::default()
        },
    );
    let logins = |r: Resolved| match r {
        Resolved::Local(a) => a.into_iter().map(|a| a.login).collect::<Vec<_>>(),
        other => panic!("not local: {other:?}"),
    };
    // Подадрес — ящик без тега; адрес учётной записи точнее catch-all.
    assert_eq!(
        logins(d.resolve("Alice+News@example.org").await.expect("r")),
        ["alice@example.org"]
    );
    assert_eq!(
        logins(d.resolve("carol@example.org").await.expect("r")),
        ["carol@example.org"]
    );
    // Неизвестный адрес домена (и неизвестный с тегом) — catch-all.
    for a in ["nobody@example.org", "nobody+x@example.org"] {
        assert_eq!(logins(d.resolve(a).await.expect("r")), ["bob@example.org"]);
    }
    // Получатель старого домена переписан до разрешения.
    assert_eq!(
        logins(d.resolve("alice@old.example.org").await.expect("r")),
        ["alice@example.org"]
    );
    assert_eq!(
        d.resolve("x@example.net").await.expect("r"),
        Resolved::External
    );
    // Письмо: тег — в Delivered-To копии, отправитель и внешний получатель переписаны.
    let mut e = envelope(
        Mode::Submission,
        &["alice+news@example.org", "partner@example.net"],
    );
    e.mail_from = "sender@old.example.org".into();
    let r = d.data(&e, Bytes::from_static(MAIL)).await;
    assert_eq!(r.code, 250, "{r:?}");
    let inbox = env.inbox("alice@example.org").await;
    assert_eq!(inbox.len(), 1);
    assert!(
        inbox[0].starts_with("Delivered-To: alice+news@example.org\r\n"),
        "{}",
        inbox[0]
    );
    assert!(inbox[0].contains("Return-Path: <sender@example.org>"));
    let queued = env.queue.0.lock().expect("lock").clone();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0, "sender@example.org");
    assert_eq!(queued[0].1, ["partner@example.com"]);
    // Отправка вне сессии: статус — по исходному адресу.
    let sent = d
        .submit(
            TenantId::DEFAULT,
            "sender@old.example.org",
            &["partner@example.net".into()],
            Bytes::from_static(MAIL),
        )
        .await
        .expect("submit");
    assert_eq!(sent.rcpts[0].1, SubmitStatus::Queued);
    assert_eq!(env.queue.0.lock().expect("lock")[1].0, "sender@example.org");
}

#[tokio::test]
async fn rewrite_table_reloads_on_transport_signal() {
    use std::time::Duration;
    use tp_contracts::bus::EventBus;
    use tp_contracts::role::CancellationToken;
    let env = Env::new(DecisionHooks::default()).await;
    let bus: Arc<dyn EventBus> = Arc::new(MemEventBus::default());
    let transport = env.transport.clone().with_bus(bus.clone());
    // Снимок недоступен и прежнего нет — ошибка (временный отказ), а не доставка без переписывания.
    transport.fail_next(Error::Unavailable { retry_after: None });
    let routes = Arc::new(Routes::new(Arc::new(transport.clone())));
    let d = env.delivery(false).with_transport(routes.clone());
    assert!(d.resolve("alice@old.example.org").await.is_err());
    assert_eq!(
        d.resolve("alice@old.example.org").await.expect("r"),
        Resolved::External,
        "правила ещё нет"
    );
    let stop = CancellationToken::new();
    tokio::spawn(
        routes
            .clone()
            .watch(bus.clone(), stop.clone(), Duration::from_millis(50)),
    );
    // Правило добавлено через хранилище (Admin API) — сигнал; до подписки — сверка при подписке.
    let v = transport
        .put_rewrite(
            TenantId::DEFAULT,
            0,
            &rewrite_rule("@old.example.org", "@example.org"),
        )
        .await
        .expect("rewrite");
    tokio::time::timeout(Duration::from_secs(5), async {
        while routes
            .tenant(TenantId::DEFAULT)
            .await
            .expect("tenant")
            .version
            != v
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("таблица перечитана по сигналу");
    match d.resolve("alice@old.example.org").await.expect("r") {
        Resolved::Local(a) => assert_eq!(a[0].login, "alice@example.org"),
        other => panic!("not local: {other:?}"),
    }
    // Правило только для отправителя к получателям не применяется.
    let sender_only = tp_contracts::transport::RewriteRule {
        recipient: false,
        ..rewrite_rule("@old.example.org", "@example.org")
    };
    let v = transport
        .put_rewrite(TenantId::DEFAULT, v, &sender_only)
        .await
        .expect("rewrite");
    tokio::time::timeout(Duration::from_secs(5), async {
        while routes
            .tenant(TenantId::DEFAULT)
            .await
            .expect("tenant")
            .version
            != v
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("таблица перечитана по сигналу");
    assert_eq!(
        d.resolve("alice@old.example.org").await.expect("r"),
        Resolved::External
    );
    // Перечитать не удалось — действует прежний снимок.
    transport.fail_next(Error::Unavailable { retry_after: None });
    routes.resync();
    assert_eq!(
        d.resolve("alice@old.example.org").await.expect("r"),
        Resolved::External
    );
    stop.cancel();
}

#[tokio::test]
async fn rcpt_replies() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.delivery(false);
    let inbound = envelope(Mode::Inbound, &[]);
    let submission = envelope(Mode::Submission, &[]);
    assert_eq!(d.rcpt(&inbound, "bob@example.org").await.code, 250);
    assert_eq!(d.rcpt(&inbound, "nobody@example.org").await.code, 550);
    // smtp-in — не открытый релей.
    let r = d.rcpt(&inbound, "x@example.net").await;
    assert_eq!((r.code, r.esc), (550, [5, 7, 1]));
    // Submission без очереди исходящей почты внешних получателей не принимает (SMTP-06).
    assert_eq!(d.rcpt(&submission, "x@example.net").await.code, 451);
    assert_eq!(
        env.delivery(true)
            .rcpt(&submission, "x@example.net")
            .await
            .code,
        250
    );
    // Переполненный ящик — временный отказ на RCPT.
    let bob = envelope(Mode::Inbound, &["bob@example.org"]);
    assert_eq!(d.data(&bob, Bytes::from_static(MAIL)).await.code, 250);
    env.quota
        .set_limit(env.scope("bob@example.org").await, Some(1));
    let r = d.rcpt(&inbound, "bob@example.org").await;
    assert_eq!((r.code, r.esc), (452, [4, 2, 2]));
    // В группе переполненный ящик одного члена адрес не отклоняет.
    assert_eq!(d.rcpt(&inbound, "team@example.org").await.code, 250);
    // При приёме письма переполненный ящик пропускается, остальные получают письмо.
    let r = d
        .data(
            &envelope(Mode::Inbound, &["bob@example.org", "carol@example.org"]),
            Bytes::from_static(MAIL),
        )
        .await;
    assert_eq!(r.code, 250);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    assert_eq!(env.inbox("carol@example.org").await.len(), 1);
    let r = d.data(&bob, Bytes::from_static(MAIL)).await;
    assert_eq!((r.code, r.esc), (552, [5, 2, 2]));
}

#[tokio::test]
async fn one_ingest_for_many_recipients() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.delivery(false);
    let rcpts = ["alice@example.org", "team@example.org", "carol@example.org"];
    let r = d
        .data(&envelope(Mode::Inbound, &rcpts), Bytes::from_static(MAIL))
        .await;
    assert_eq!(r.code, 250, "{}", r.message);
    // Каждой учётной записи — одна копия, хотя alice получает письмо и напрямую, и через группу.
    for login in ["alice@example.org", "bob@example.org", "carol@example.org"] {
        let inbox = env.inbox(login).await;
        assert_eq!(inbox.len(), 1, "{login}");
        let raw = &inbox[0];
        assert!(raw.starts_with("Return-Path: <sender@example.net>\r\nReceived: from client.example.net ([192.0.2.10])\r\n\tby mx.example.org (The Post) with ESMTPS id test1;\r\n\t"), "{raw}");
        assert!(raw.ends_with("\r\nFrom: sender@example.net\r\nSubject: hello\r\n\r\nbody\r\n"));
        // Получателей несколько: в Received их нет.
        assert!(!raw.contains("for <"));
    }
    // Один вызов ingest: объектов загружено столько же, сколько для одного получателя.
    let single = Env::new(DecisionHooks::default()).await;
    let r = single
        .delivery(false)
        .data(
            &envelope(Mode::Inbound, &["bob@example.org"]),
            Bytes::from_static(MAIL),
        )
        .await;
    assert_eq!(r.code, 250);
    assert_eq!(
        env.blobs.calls(BlobOp::Put),
        single.blobs.calls(BlobOp::Put)
    );
}

#[tokio::test]
async fn single_recipient_received_for() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.delivery(false);
    let mut e = envelope(Mode::Submission, &["bob@example.org"]);
    e.principal = Some(tp_testkit::principal(
        TenantId::DEFAULT,
        "alice@example.org",
    ));
    e.conn.client = "[2001:db8::1]:5000".parse().expect("addr");
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    let raw = &env.inbox("bob@example.org").await[0];
    assert!(raw.contains("Received: from client.example.net ([IPv6:2001:db8::1])\r\n\tby mx.example.org (The Post) with ESMTPSA id test1\r\n\tfor <bob@example.org>;\r\n\t"), "{raw}");
}

#[tokio::test]
async fn submission_external_recipients_go_to_queue() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.delivery(true);
    let e = envelope(
        Mode::Submission,
        &["bob@example.org", "x@example.net", "y@example.com"],
    );
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    let queued = env.queue.0.lock().expect("lock").clone();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0, "sender@example.net");
    assert_eq!(queued[0].1, ["x@example.net", "y@example.com"]);
    assert!(
        queued[0]
            .2
            .starts_with(b"Return-Path: <sender@example.net>\r\nReceived: ")
    );
}

#[tokio::test]
async fn forward_from_decision_hook_is_queued() {
    let hook = Arc::new(RecordingDecisionHook::new("journal"));
    hook.set_delivery(Decision::Augment(Augment {
        headers: vec![],
        copies: vec![CopyTo::Address("archive@example.com".into())],
    }));
    let env = Env::new(DecisionHooks::new(vec![hook])).await;
    let d = env.delivery(true);
    let e = envelope(Mode::Inbound, &["carol@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.inbox("carol@example.org").await.len(), 1);
    let queued = env.queue.0.lock().expect("lock").clone();
    assert_eq!(queued[0].1, ["archive@example.com"]);
    // Без очереди пересылка не ломает доставку (узел без smtp-out).
    let d = env.delivery(false);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.inbox("carol@example.org").await.len(), 2);
}

#[tokio::test]
async fn denied_by_hook() {
    let hook = Arc::new(RecordingDecisionHook::new("dlp"));
    hook.set_delivery(Decision::Deny("policy".into()));
    let env = Env::new(DecisionHooks::new(vec![hook])).await;
    let d = env.delivery(false);
    let r = d
        .data(
            &envelope(Mode::Inbound, &["bob@example.org"]),
            Bytes::from_static(MAIL),
        )
        .await;
    assert_eq!(r.code / 100, 5);
    assert!(env.inbox("bob@example.org").await.is_empty());
}

#[tokio::test]
async fn session_delivers_to_several_recipients() {
    let env = Env::new(DecisionHooks::default()).await;
    let ctx = SessionCtx {
        mode: Mode::Inbound,
        config: Arc::new(SmtpConfig {
            hostname: "mx.example.org".into(),
            ..SmtpConfig::default()
        }),
        handler: Arc::new(env.delivery(false)),
        sasl: None,
        limiter: None,
        send_limits: None,
        metrics: None,
        screen: None,
    };
    let (client, mut server) = tokio::io::duplex(64 * 1024);
    let task = tokio::spawn(async move {
        Session::new(ctx, conn_info(false), false)
            .serve(&mut server, true)
            .await
    });
    let mut c = Client::new(client);
    assert_eq!(c.code().await, 220);
    assert_eq!(c.cmd("EHLO client.example.net").await, 250);
    assert_eq!(c.cmd("MAIL FROM:<sender@example.net>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<alice@example.org>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<bob@example.org>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<nobody@example.org>").await, 550);
    assert_eq!(c.cmd("RCPT TO:<x@example.net>").await, 550);
    assert_eq!(c.cmd("DATA").await, 354);
    c.send("Subject: hi\r\n\r\nhello\r\n.\r\n").await;
    assert_eq!(c.code().await, 250);
    assert_eq!(c.cmd("QUIT").await, 221);
    task.await.expect("join").expect("io");
    for login in ["alice@example.org", "bob@example.org"] {
        let inbox = env.inbox(login).await;
        assert_eq!(inbox.len(), 1);
        assert!(inbox[0].contains("with ESMTP id "));
        assert!(inbox[0].ends_with("Subject: hi\r\n\r\nhello\r\n"));
    }
    assert!(env.inbox("carol@example.org").await.is_empty());
}

#[tokio::test]
async fn dmarc_failure_is_delivered_to_junk_with_results() {
    use crate::authcheck::tests as fx;
    let env = Env::new(DecisionHooks::default()).await;
    let check = fx::checker();
    let signer = fx::Signer::new("example.com", "s1");
    signer.publish(check.cache());
    fx::publish_sender(check.cache(), "quarantine");
    let mut d = env.delivery(false);
    d.deps.authcheck = Some(Arc::new(check));
    // Подписанное письмо с адреса из SPF — во «Входящие».
    let good = signer.sign(fx::MAIL);
    let r = d
        .data(&fx::envelope("192.0.2.10:1000"), Bytes::from(good))
        .await;
    assert_eq!(r.code, 250);
    let inbox = env.inbox("bob@example.org").await;
    assert_eq!(inbox.len(), 1);
    assert!(
        inbox[0].starts_with("Authentication-Results: mx.example.org;"),
        "{}",
        inbox[0]
    );
    assert!(inbox[0].contains("dmarc=pass"));
    assert!(
        inbox[0].contains("\r\nReturn-Path: <sender@example.com>\r\nReceived: from example.com")
    );
    // Подделка: подпись не сходится, адрес вне SPF — в «Спам».
    let forged = signer.sign(fx::MAIL).replace("body", "forged");
    let r = d
        .data(&fx::envelope("203.0.113.5:1000"), Bytes::from(forged))
        .await;
    assert_eq!(r.code, 250);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    let scope = env.scope("bob@example.org").await;
    let junk = env
        .meta
        .mailbox_by_role(scope, SpecialUse::Junk)
        .expect("junk");
    let msgs = MessageStore::list(&env.meta, scope, junk, MessageQuery::All, 10)
        .await
        .expect("list");
    assert_eq!(msgs.len(), 1);
    // Submission не проверяется: письмо пользователя — во «Входящие» получателя.
    let mut e = fx::envelope("203.0.113.5:1000");
    e.mode = Mode::Submission;
    let forged = signer.sign(fx::MAIL).replace("body", "forged");
    assert_eq!(d.data(&e, Bytes::from(forged)).await.code, 250);
    assert_eq!(env.inbox("bob@example.org").await.len(), 2);
}

#[tokio::test]
async fn submission_decision_hook() {
    let hook = Arc::new(RecordingDecisionHook::new("journal"));
    hook.set_submission(Decision::Augment(Augment {
        headers: vec![("X-Journal".into(), "yes".into())],
        copies: vec![CopyTo::Address("archive@example.com".into())],
    }));
    let env = Env::new(DecisionHooks::default()).await;
    let mut d = env.delivery(true);
    d.deps.hooks = DecisionHooks::new(vec![hook.clone()]);
    let mut e = envelope(Mode::Submission, &["bob@example.org", "x@example.net"]);
    e.principal = Some(tp_testkit::principal(
        TenantId::DEFAULT,
        "alice@example.org",
    ));
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    let inbox = env.inbox("bob@example.org").await;
    assert!(inbox[0].starts_with("X-Journal: yes\r\n"), "{}", inbox[0]);
    let queued = env.queue.0.lock().expect("lock").clone();
    assert_eq!(queued[0].1, ["x@example.net", "archive@example.com"]);
    // Запрет — отказ всему письму, ничего не доставлено и не поставлено в очередь.
    hook.set_submission(Decision::Deny("dlp".into()));
    let r = d.data(&e, Bytes::from_static(MAIL)).await;
    assert_eq!((r.code, r.esc), (550, [5, 7, 1]));
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    assert_eq!(env.queue.0.lock().expect("lock").len(), 1);
}

#[tokio::test]
async fn bounce_for_full_mailbox_after_accept() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.delivery(true);
    env.quota
        .set_limit(env.scope("bob@example.org").await, Some(1));
    let e = envelope(Mode::Inbound, &["bob@example.org", "carol@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.inbox("carol@example.org").await.len(), 1);
    let queued = env.queue.0.lock().expect("lock").clone();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0, "");
    assert_eq!(queued[0].1, ["sender@example.net"]);
    let dsn = String::from_utf8_lossy(&queued[0].2).into_owned();
    assert!(
        dsn.contains("Final-Recipient: rfc822; bob@example.org\r\nAction: failed\r\nStatus: 5.2.2"),
        "{dsn}"
    );
    // Локальному отправителю уведомление — во «Входящие».
    let mut e = envelope(Mode::Submission, &["bob@example.org", "carol@example.org"]);
    e.mail_from = "alice@example.org".into();
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    let inbox = env.inbox("alice@example.org").await;
    assert_eq!(inbox.len(), 1);
    assert!(inbox[0].contains("Subject: Undelivered Mail Returned to Sender"));
}

fn rcpts(list: &[&str]) -> Vec<String> {
    list.iter().map(|r| (*r).to_owned()).collect()
}

const OWN: &[u8] = b"From: alice@example.org\r\nSubject: hi\r\n\r\nbody\r\n";

#[tokio::test]
async fn submit_local_recipient_delivered_without_queue() {
    let env = Env::new(DecisionHooks::default()).await;
    // Без очереди: локальная доставка не зависит от smtp-out (MX, DNS, MTA-STS, DANE).
    let d = env.delivery(false);
    let out = d
        .submit(
            TenantId::DEFAULT,
            "alice@example.org",
            &rcpts(&["Bob@Example.org"]),
            Bytes::from_static(OWN),
        )
        .await
        .expect("submit");
    assert_eq!(
        out.rcpts,
        [("Bob@Example.org".to_owned(), SubmitStatus::Delivered)]
    );
    let inbox = env.inbox("bob@example.org").await;
    assert_eq!(inbox.len(), 1);
    assert!(
        inbox[0].starts_with(
            "Return-Path: <alice@example.org>\r\nReceived: by mx.example.org (The Post) id "
        ),
        "{}",
        inbox[0]
    );
    assert!(inbox[0].contains("\tfor <Bob@Example.org>;"));
    assert!(inbox[0].ends_with("Subject: hi\r\n\r\nbody\r\n"));
    assert!(env.queue.0.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn submit_mixed_recipients() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.delivery(true);
    // Через `Outbound`, как подключает JMAP.
    let ob: Arc<dyn Outbound> = Arc::new(env.delivery(true));
    let list = rcpts(&[
        "team@example.org",
        "x@example.net",
        "postmaster@example.org",
    ]);
    let out = d
        .submit(
            TenantId::DEFAULT,
            "carol@example.org",
            &list,
            Bytes::from_static(OWN),
        )
        .await
        .expect("submit");
    assert_eq!(
        out.rcpts,
        [
            ("team@example.org".to_owned(), SubmitStatus::Delivered),
            ("x@example.net".to_owned(), SubmitStatus::Queued),
            ("postmaster@example.org".to_owned(), SubmitStatus::Delivered),
        ]
    );
    // Группа и псевдоним: одна копия на учётную запись.
    assert_eq!(env.inbox("alice@example.org").await.len(), 1);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    assert_eq!(env.inbox("carol@example.org").await.len(), 1);
    {
        let queued = env.queue.0.lock().expect("lock");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].0, "carol@example.org");
        assert_eq!(queued[0].1, ["x@example.net"]);
        // Во внешнюю очередь — письмо без заголовков локальной доставки (DKIM подписывает очередь).
        assert_eq!(queued[0].2, Bytes::from_static(OWN));
    }
    ob.enqueue(
        TenantId::DEFAULT,
        "carol@example.org",
        &rcpts(&["bob@example.org", "y@example.com"]),
        Bytes::from_static(OWN),
    )
    .await
    .expect("enqueue");
    assert_eq!(env.inbox("bob@example.org").await.len(), 2);
    let queued = env.queue.0.lock().expect("lock");
    assert_eq!(queued.len(), 2);
    assert_eq!(queued[1].1, ["y@example.com"]);
}

#[tokio::test]
async fn submit_unknown_local_address_bounces() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.delivery(true);
    let out = d
        .submit(
            TenantId::DEFAULT,
            "alice@example.org",
            &rcpts(&["nobody@example.org", "bob@example.org"]),
            Bytes::from_static(OWN),
        )
        .await
        .expect("submit");
    assert_eq!(
        out.rcpts,
        [
            (
                "nobody@example.org".to_owned(),
                SubmitStatus::Failed("550 5.1.1 mailbox unavailable".into())
            ),
            ("bob@example.org".to_owned(), SubmitStatus::Delivered),
        ]
    );
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    // Уведомление локальному отправителю — во «Входящие», не в очередь.
    let inbox = env.inbox("alice@example.org").await;
    assert_eq!(inbox.len(), 1);
    assert!(inbox[0].contains(
        "Final-Recipient: rfc822; nobody@example.org\r\nAction: failed\r\nStatus: 5.1.1"
    ));
    assert!(env.queue.0.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn submit_full_mailbox_bounces() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.delivery(true);
    env.quota
        .set_limit(env.scope("bob@example.org").await, Some(1));
    let out = d
        .submit(
            TenantId::DEFAULT,
            "sender@example.net",
            &rcpts(&["bob@example.org"]),
            Bytes::from_static(MAIL),
        )
        .await
        .expect("submit");
    assert_eq!(
        out.rcpts,
        [(
            "bob@example.org".to_owned(),
            SubmitStatus::Failed("552 5.2.2 mailbox full".into())
        )]
    );
    assert!(env.inbox("bob@example.org").await.is_empty());
    let queued = env.queue.0.lock().expect("lock");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0, "");
    assert_eq!(queued[0].1, ["sender@example.net"]);
    let dsn = String::from_utf8_lossy(&queued[0].2).into_owned();
    assert!(dsn.contains("Status: 5.2.2"), "{dsn}");
}

#[tokio::test]
async fn submit_mixed_without_queue() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.delivery(false);
    let out = d
        .submit(
            TenantId::DEFAULT,
            "alice@example.org",
            &rcpts(&["bob@example.org", "x@example.net"]),
            Bytes::from_static(OWN),
        )
        .await
        .expect("submit");
    // Локальный доставлен, внешнему — временная ошибка; уведомления отправителю нет (статус — у него).
    assert_eq!(
        out.rcpts,
        [
            ("bob@example.org".to_owned(), SubmitStatus::Delivered),
            (
                "x@example.net".to_owned(),
                SubmitStatus::Failed("451 4.3.0 outbound queue unavailable".into())
            ),
        ]
    );
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    assert!(env.inbox("alice@example.org").await.is_empty());
    // Через `Outbound`: часть доставлена — `Ok`; только внешние — ошибка (повтор безопасен).
    let ob: Arc<dyn Outbound> = Arc::new(env.delivery(false));
    ob.enqueue(
        TenantId::DEFAULT,
        "alice@example.org",
        &rcpts(&["bob@example.org", "x@example.net"]),
        Bytes::from_static(OWN),
    )
    .await
    .expect("enqueue");
    assert_eq!(env.inbox("bob@example.org").await.len(), 2);
    let err = ob
        .enqueue(
            TenantId::DEFAULT,
            "alice@example.org",
            &rcpts(&["x@example.net"]),
            Bytes::from_static(OWN),
        )
        .await
        .expect_err("no queue");
    assert!(err.is_retryable(), "{err}");
    // Другая организация без очереди — все получатели `Failed`.
    let out = d
        .submit(
            TenantId::new(),
            "alice@example.org",
            &rcpts(&["bob@example.org"]),
            Bytes::from_static(OWN),
        )
        .await
        .expect("submit");
    assert!(matches!(&out.rcpts[0].1, SubmitStatus::Failed(r) if r.starts_with("451 4.3.0")));
}

#[tokio::test]
async fn forward_to_local_address_without_queue() {
    // Пересылка модуля на локальный адрес — сразу в ящик, без очереди. Хук пересылает каждую доставку
    // (и саму пересылку) — повторно в ящик, уже получивший письмо, она не доставляется.
    let hook = Arc::new(RecordingDecisionHook::new("forward"));
    hook.set_delivery(Decision::Augment(Augment {
        headers: vec![],
        copies: vec![CopyTo::Address("Bob@example.org".into())],
    }));
    let env = Env::new(DecisionHooks::new(vec![hook])).await;
    let d = env.delivery(false);
    let e = envelope(Mode::Inbound, &["carol@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.inbox("carol@example.org").await.len(), 1);
    let bob = env.inbox("bob@example.org").await;
    assert_eq!(bob.len(), 1);
    assert!(
        bob[0].starts_with("Return-Path: <sender@example.net>\r\n"),
        "{}",
        bob[0]
    );
    // Отправка вне сессии: пересылка на локальный адрес — так же сразу.
    let out = d
        .submit(
            TenantId::DEFAULT,
            "alice@example.org",
            &rcpts(&["carol@example.org"]),
            Bytes::from_static(OWN),
        )
        .await
        .expect("submit");
    assert_eq!(
        out.rcpts,
        [("carol@example.org".to_owned(), SubmitStatus::Delivered)]
    );
    assert_eq!(env.inbox("carol@example.org").await.len(), 2);
    assert_eq!(env.inbox("bob@example.org").await.len(), 2);
    assert!(env.queue.0.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn submission_copy_to_local_address() {
    // Копия точки принятия решений на локальный адрес — в ящик, не в очередь.
    let hook = Arc::new(RecordingDecisionHook::new("journal"));
    hook.set_submission(Decision::Augment(Augment {
        headers: vec![],
        copies: vec![
            CopyTo::Address("postmaster@example.org".into()),
            CopyTo::Address("nobody@example.org".into()),
        ],
    }));
    let env = Env::new(DecisionHooks::default()).await;
    let mut d = env.delivery(false);
    d.deps.hooks = DecisionHooks::new(vec![hook]);
    let mut e = envelope(Mode::Submission, &["bob@example.org"]);
    e.principal = Some(tp_testkit::principal(
        TenantId::DEFAULT,
        "carol@example.org",
    ));
    e.mail_from = "carol@example.org".into();
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    assert_eq!(env.inbox("alice@example.org").await.len(), 1);
    // Неизвестный адрес копии — без уведомления отправителю (адрес копии ему не раскрывается).
    assert!(env.inbox("carol@example.org").await.is_empty());
}

mod imip;
mod moderation;

mod groups {
    //! SMTP-16: квота до `DATA`, ограничения отправителей и модерация групп рассылки.

    use tp_contracts::groups::{DistributionGroup, GroupMember, GroupStore, SenderRestriction};
    use tp_contracts::{GroupId, TenantId};

    use super::*;
    use crate::config::{DeliveryConfig, QuotaReject};
    use crate::groups::Groups;

    impl Env {
        fn with_groups(&self, d: Delivery) -> Delivery {
            d.with_groups(Groups::new(
                Arc::new(self.groups.clone()),
                Arc::new(self.blobs.clone()),
                Arc::new(self.index.clone()),
            ))
        }

        async fn group(
            &self,
            address: &str,
            members: &[&str],
            senders: SenderRestriction,
            moderators: &[&str],
        ) -> DistributionGroup {
            let mut ids = Vec::new();
            for m in members {
                ids.push(GroupMember::Account(self.scope(m).await.account));
            }
            let mut mods = Vec::new();
            for m in moderators {
                mods.push(self.scope(m).await.account);
            }
            let g = DistributionGroup {
                id: GroupId::new(),
                tenant: TenantId::DEFAULT,
                name: address.into(),
                address: Some(address.into()),
                external_id: None,
                members: ids,
                senders,
                moderators: mods,
                hidden_from_gal: false,
            };
            self.groups.put(g.clone()).await.expect("put group");
            g
        }

        async fn principal(&self, login: &str) -> tp_contracts::auth::Principal {
            tp_contracts::auth::Principal {
                account: self.scope(login).await.account,
                ..tp_testkit::principal(TenantId::DEFAULT, login)
            }
        }
    }

    #[tokio::test]
    async fn full_mailbox_rejected_before_data() {
        let env = Env::new(DecisionHooks::default()).await;
        let bob = envelope(Mode::Inbound, &["bob@example.org"]);
        let r = env
            .delivery(false)
            .data(&bob, Bytes::from_static(MAIL))
            .await;
        assert_eq!(r.code, 250);
        env.quota
            .set_limit(env.scope("bob@example.org").await, Some(1));
        let inbound = envelope(Mode::Inbound, &[]);
        // По умолчанию — временный отказ.
        let r = env.delivery(false).rcpt(&inbound, "bob@example.org").await;
        assert_eq!((r.code, r.esc), (452, [4, 2, 2]));
        // Постоянный — по настройке.
        let d = env.delivery_with(
            false,
            SmtpConfig {
                delivery: DeliveryConfig {
                    quota_reject: QuotaReject::Permanent,
                    ..DeliveryConfig::default()
                },
                ..SmtpConfig::default()
            },
        );
        let r = d.rcpt(&inbound, "bob@example.org").await;
        assert_eq!((r.code, r.esc), (552, [5, 2, 2]));
        // В сессии: получатель отклонён на RCPT, до DATA дело не доходит.
        let ctx = SessionCtx {
            mode: Mode::Inbound,
            config: Arc::new(SmtpConfig::default()),
            handler: Arc::new(d),
            sasl: None,
            limiter: None,
            send_limits: None,
            metrics: None,
            screen: None,
        };
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(async move {
            Session::new(ctx, conn_info(false), false)
                .serve(&mut server, true)
                .await
        });
        let mut c = Client::new(client);
        assert_eq!(c.code().await, 220);
        assert_eq!(c.cmd("EHLO client.example.net").await, 250);
        assert_eq!(c.cmd("MAIL FROM:<s@example.net>").await, 250);
        assert_eq!(c.cmd("RCPT TO:<bob@example.org>").await, 552);
        assert_eq!(c.cmd("DATA").await, 554);
        assert_eq!(c.cmd("QUIT").await, 221);
        task.await.expect("join").expect("io");
        assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    }

    #[tokio::test]
    async fn closed_group_rejects_external_sender() {
        let env = Env::new(DecisionHooks::default()).await;
        let d = env.with_groups(env.delivery(false));
        env.group(
            "staff@example.org",
            &["alice@example.org", "bob@example.org"],
            SenderRestriction::Internal,
            &[],
        )
        .await;
        let bob = env.scope("bob@example.org").await.account;
        env.group(
            "board@example.org",
            &["alice@example.org"],
            SenderRestriction::Only {
                accounts: vec![bob],
                groups: Vec::new(),
            },
            &[],
        )
        .await;
        // Внешний отправитель (smtp-in) — отказ до DATA.
        let inbound = envelope(Mode::Inbound, &[]);
        let r = d.rcpt(&inbound, "staff@example.org").await;
        assert_eq!((r.code, r.esc), (550, [5, 7, 1]));
        // Пользователь организации — принят; группа раскрыта.
        let mut sub = envelope(Mode::Submission, &[]);
        sub.principal = Some(env.principal("carol@example.org").await);
        assert_eq!(d.rcpt(&sub, "staff@example.org").await.code, 250);
        // Список отправителей: carol не в нём, bob — в нём.
        let r = d.rcpt(&sub, "board@example.org").await;
        assert_eq!((r.code, r.esc), (550, [5, 7, 1]));
        sub.principal = Some(env.principal("bob@example.org").await);
        assert_eq!(d.rcpt(&sub, "board@example.org").await.code, 250);
        sub.rcpts = vec!["staff@example.org".into()];
        sub.mail_from = "bob@example.org".into();
        assert_eq!(d.data(&sub, Bytes::from_static(MAIL)).await.code, 250);
        assert_eq!(env.inbox("alice@example.org").await.len(), 1);
        assert_eq!(env.inbox("bob@example.org").await.len(), 1);
        // Отправка вне сессии от внешнего адреса в закрытую группу — отказ с уведомлением.
        let sent = d
            .submit(
                TenantId::DEFAULT,
                "x@example.net",
                &rcpts(&["staff@example.org"]),
                Bytes::from_static(MAIL),
            )
            .await
            .expect("submit");
        assert!(
            matches!(&sent.rcpts[0].1, SubmitStatus::Failed(r) if r.starts_with("550 5.7.1")),
            "{sent:?}"
        );
        assert_eq!(env.inbox("alice@example.org").await.len(), 1);
    }

    #[tokio::test]
    async fn moderated_group_holds_message() {
        let env = Env::new(DecisionHooks::default()).await;
        let d = env.with_groups(env.delivery(false));
        let g = env
            .group(
                "news@example.org",
                &["alice@example.org", "bob@example.org"],
                SenderRestriction::Anyone,
                &["carol@example.org"],
            )
            .await;
        let mut e = envelope(Mode::Inbound, &["news@example.org", "bob@example.org"]);
        assert_eq!(d.rcpt(&e, "news@example.org").await.code, 250);
        let raw = b"From: sender@example.net\r\nMessage-ID: <m1@example.net>\r\n\r\nbody\r\n";
        assert_eq!(d.data(&e, Bytes::from_static(raw)).await.code, 250);
        // Группе — в очередь модерации, прямому получателю — сразу.
        assert!(env.inbox("alice@example.org").await.is_empty());
        assert_eq!(env.inbox("bob@example.org").await.len(), 1);
        let held = env
            .groups
            .held(TenantId::DEFAULT, Some(g.id), 10)
            .await
            .expect("held");
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].sender, "sender@example.net");
        assert_eq!(held[0].sender_account, None);
        assert_eq!(held[0].message_id.as_deref(), Some("m1@example.net"));
        assert!(env.blobs.contains(&held[0].blob));
        let stored = env.blobs.bytes(&held[0].blob.path()).expect("blob");
        assert!(stored.ends_with(raw));
        // Письмо модератора не задерживается.
        e.mode = Mode::Submission;
        e.principal = Some(env.principal("carol@example.org").await);
        e.mail_from = "carol@example.org".into();
        e.rcpts = vec!["news@example.org".into()];
        assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
        assert_eq!(env.inbox("alice@example.org").await.len(), 1);
        // Отправка вне сессии не модератором — статус «в очереди», письмо ждёт модератора.
        let sent = d
            .submit(
                TenantId::DEFAULT,
                "alice@example.org",
                &rcpts(&["news@example.org"]),
                Bytes::from_static(MAIL),
            )
            .await
            .expect("submit");
        assert_eq!(sent.rcpts[0].1, SubmitStatus::Queued);
        let held = env
            .groups
            .held(TenantId::DEFAULT, Some(g.id), 10)
            .await
            .expect("held");
        assert_eq!(held.len(), 2);
        assert_eq!(
            held[1].sender_account,
            Some(env.scope("alice@example.org").await.account)
        );
    }
}

mod tracking {
    //! SMTP-17: журнал доставки — сквозной путь smtp-in, submission, smtp-out на фейках.

    use tp_contracts::delivery::{DeliveryDecision, DeliveryEvent, DeliveryEventKind};
    use tp_contracts::membership::NodeId;
    use tp_testkit::MemDeliveryLog;

    use super::*;
    use crate::config::QueueConfig;
    use crate::queue::tests::Env as QueueEnv;
    use crate::queue::{Queue as OutQueue, QueueWorker, RcptResult, SendRequest, Transport};
    use crate::tracking::Tracking;

    /// Принимающие серверы: `y@…` — постоянный отказ, остальные — приняты.
    struct ByAddress;

    #[async_trait]
    impl Transport for ByAddress {
        async fn send(&self, req: SendRequest<'_>) -> Vec<RcptResult> {
            req.rcpts
                .iter()
                .map(|r| {
                    if r.starts_with("y@") {
                        RcptResult::Permanent("550 5.1.1 no such user".into())
                    } else {
                        RcptResult::Delivered("250 2.0.0 ok".into())
                    }
                })
                .collect()
        }
    }

    fn find<'a>(
        events: &'a [DeliveryEvent],
        kind: DeliveryEventKind,
        rcpt: &str,
    ) -> Vec<&'a DeliveryEvent> {
        events
            .iter()
            .filter(|e| e.kind == kind && e.recipients.iter().any(|r| r == rcpt))
            .collect()
    }

    #[tokio::test]
    async fn all_events_recorded() {
        let env = Env::new(DecisionHooks::default()).await;
        let q = QueueEnv::new();
        let log = MemDeliveryLog::new();
        let tracking = Tracking::new(Arc::new(log.clone()), NodeId::new("node-1").expect("node"));
        let d = Delivery::new(
            Deps {
                outbound: Some(Arc::new(OutQueue::new(q.deps.clone()))),
                ..env.delivery(false).deps.clone()
            },
            &SmtpConfig {
                hostname: "mx.example.org".into(),
                ..SmtpConfig::default()
            },
        )
        .with_transport(env.routes())
        .with_tracking(tracking.clone());
        // Отказ на RCPT.
        let inbound = envelope(Mode::Inbound, &[]);
        assert_eq!(d.rcpt(&inbound, "nobody@example.org").await.code, 550);
        // Приём: carol доставлено, ящик bob переполнен — отказ получателю и уведомление отправителю.
        assert_eq!(
            d.data(
                &envelope(Mode::Inbound, &["bob@example.org"]),
                Bytes::from_static(MAIL)
            )
            .await
            .code,
            250
        );
        env.quota
            .set_limit(env.scope("bob@example.org").await, Some(1));
        let raw =
            b"From: sender@example.net\r\nMessage-ID: <t1@example.net>\r\n\r\nsecret body\r\n";
        let e = envelope(Mode::Inbound, &["bob@example.org", "carol@example.org"]);
        assert_eq!(d.data(&e, Bytes::from_static(raw)).await.code, 250);
        // Отправка наружу: одному получателю принято, другому — отказ (уведомление отправителю).
        let mut sub = envelope(Mode::Submission, &["x@example.net", "y@example.net"]);
        sub.mail_from = "alice@example.org".into();
        let raw2 = b"From: alice@example.org\r\nMessage-ID: <t2@example.org>\r\n\r\nhello\r\n";
        assert_eq!(d.data(&sub, Bytes::from_static(raw2)).await.code, 250);
        let worker = QueueWorker::new(q.deps.clone(), Arc::new(ByAddress), "out.example.org")
            .with_tracking(tracking);
        worker.run_once(&QueueConfig::default()).await.expect("run");

        let events = log.events();
        let rej = find(&events, DeliveryEventKind::Reject, "nobody@example.org");
        assert_eq!(rej.len(), 1);
        assert_eq!(rej[0].decision, DeliveryDecision::Reject);
        assert_eq!(
            rej[0].reason.as_deref(),
            Some("550 5.1.1 Mailbox unavailable")
        );
        assert_eq!(rej[0].sender, "sender@example.net");
        assert_eq!(rej[0].node.as_str(), "node-1");
        assert_eq!(rej[0].message_id, None);

        let mine: Vec<_> = events
            .iter()
            .filter(|e| e.message_id.as_deref() == Some("t1@example.net"))
            .collect();
        let kinds: Vec<_> = mine.iter().map(|e| (e.kind, e.decision)).collect();
        assert!(kinds.contains(&(DeliveryEventKind::Receive, DeliveryDecision::Accept)));
        let deliver = find(&events, DeliveryEventKind::Deliver, "carol@example.org");
        assert_eq!(deliver.len(), 1);
        assert_eq!(deliver[0].recipients, ["carol@example.org"]);
        let full = mine
            .iter()
            .find(|e| e.kind == DeliveryEventKind::Reject)
            .expect("reject bob");
        assert_eq!(full.recipients, ["bob@example.org"]);
        assert_eq!(full.reason.as_deref(), Some("552 5.2.2 mailbox full"));
        let dsn = mine
            .iter()
            .find(|e| e.kind == DeliveryEventKind::Dsn)
            .expect("dsn");
        assert_eq!(dsn.sender, "");
        assert_eq!(dsn.recipients, ["sender@example.net"]);

        // smtp-out: передача наружу с решением и причиной, запись очереди, уведомление об отказе.
        let x = find(&events, DeliveryEventKind::Relay, "x@example.net");
        assert_eq!(x.len(), 1);
        assert_eq!(x[0].decision, DeliveryDecision::Accept);
        assert_eq!(x[0].message_id.as_deref(), Some("t2@example.org"));
        assert!(x[0].queue_id.is_some());
        let y = find(&events, DeliveryEventKind::Relay, "y@example.net");
        assert_eq!(y[0].decision, DeliveryDecision::Reject);
        assert_eq!(y[0].reason.as_deref(), Some("550 5.1.1 no such user"));
        let dsn = find(&events, DeliveryEventKind::Dsn, "alice@example.org");
        assert_eq!(dsn.len(), 1);
        assert_eq!(dsn[0].queue_id, x[0].queue_id);
        assert!(find(&events, DeliveryEventKind::Receive, "x@example.net").len() == 1);
        // Содержимое писем в журнал не попадает.
        let dump = format!("{events:?}");
        assert!(!dump.contains("secret body") && !dump.contains("hello"));
    }
}

/// Контракт `Outbound` (CTR-28): `Delivery` — локальные и внешние получатели, статусы в порядке запроса.
#[tokio::test]
async fn outbound_suite() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.delivery(true);
    let list = rcpts(&["x@example.net", "team@example.org", "Bob@Example.org"]);
    tp_testkit::suite::outbound_submit(&d, TenantId::DEFAULT, &list).await;
}

mod local_domains;
mod org_policy;
mod rcpt_rate;
mod rules;
mod wave;
