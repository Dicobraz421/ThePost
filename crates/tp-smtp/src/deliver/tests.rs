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
use tp_contracts::{Result, TenantId};
use tp_testkit::{
    BlobOp, Clock, FakeDirectory, MemBlobIndex, MemBlobStore, MemEventBus, MemMetadata, MemQuota,
    MemTrash, RecordingDecisionHook, SystemClock,
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
    quota: MemQuota,
    store: Arc<dyn MailStore>,
    dir: FakeDirectory,
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
            index: Arc::new(index),
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
        Self {
            meta,
            blobs,
            quota,
            store: Arc::new(store),
            dir,
            queue: Arc::new(Queue::default()),
        }
    }

    fn delivery(&self, outbound: bool) -> Delivery {
        let cfg = SmtpConfig {
            hostname: "mx.example.org".into(),
            ..SmtpConfig::default()
        };
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
