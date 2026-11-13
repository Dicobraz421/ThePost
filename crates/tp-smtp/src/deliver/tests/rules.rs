//! Волна smtp-12: правила Sieve при локальной доставке (SMTP-31) и автоответ (SMTP-32) на фейке `FakeRules`.

use tp_contracts::membership::NodeId;
use tp_contracts::meta::{Flag, Message, NewMailbox};
use tp_contracts::secrets::SecretRef;
use tp_contracts::sieve::{KeepTo, ReplyBody};
use tp_testkit::{FakeRules, MemDeliveryLog, MemSecretStore};

use super::*;
use crate::config::{ForwardConfig, SrsConfig};

fn forwarding() -> Arc<Forwarding> {
    let secrets = MemSecretStore::new(Arc::new(SystemClock));
    let key: SecretRef = "thepost/srs#key".parse().expect("ref");
    secrets.set(&key, "srs-secret");
    Arc::new(Forwarding::new(
        Arc::new(secrets),
        ForwardConfig {
            srs: Some(SrsConfig {
                domain: "example.org".into(),
                key,
                previous_keys: Vec::new(),
                max_age_days: 21,
            }),
            ..ForwardConfig::default()
        },
    ))
}

fn cfg() -> SmtpConfig {
    SmtpConfig {
        hostname: "mx.example.org".into(),
        ..SmtpConfig::default()
    }
}

/// Доставка с правилами `FakeRules`, журналом доставки и пересылкой SRS.
async fn setup(outbound: bool, cfg: SmtpConfig) -> (Env, FakeRules, MemDeliveryLog, Delivery) {
    let env = Env::new(DecisionHooks::default()).await;
    let rules = FakeRules::new();
    let log = MemDeliveryLog::new();
    let d = env
        .delivery_with(outbound, cfg)
        .with_rules(Arc::new(rules.clone()))
        .with_tracking(Tracking::new(
            Arc::new(log.clone()),
            NodeId::new("node-1").expect("node"),
        ))
        .with_forwarding(forwarding());
    (env, rules, log, d)
}

/// Письма папки `path` учётной записи (нет папки — пусто).
async fn folder(env: &Env, login: &str, path: &str) -> Vec<Message> {
    let scope = env.scope(login).await;
    let list = MailboxStore::list(&env.meta, scope).await.expect("list");
    let Some(id) = crate::sieve::find_path(&list, path) else {
        return Vec::new();
    };
    MessageStore::list(&env.meta, scope, id, MessageQuery::All, 100)
        .await
        .expect("messages")
}

/// Причины событий журнала доставки с получателем `rcpt`.
fn reasons(log: &MemDeliveryLog, rcpt: &str) -> Vec<String> {
    log.events()
        .into_iter()
        .filter(|e| e.recipients.iter().any(|r| r == rcpt))
        .filter_map(|e| e.reason)
        .collect()
}

fn keep(mailbox: MailboxRef, flags: &[Flag]) -> KeepTo {
    KeepTo {
        mailbox,
        flags: flags.iter().cloned().collect(),
    }
}

fn named(name: &str, create: bool) -> MailboxRef {
    MailboxRef::Name {
        name: name.into(),
        create,
    }
}

fn keeps(keep: Vec<KeepTo>) -> RulesOutcome {
    RulesOutcome {
        keep,
        ..RulesOutcome::default()
    }
}

// ---------- SMTP-31: правила при доставке ----------

#[tokio::test]
async fn fileinto_creates_folder_with_flags() {
    let (env, rules, log, d) = setup(false, cfg()).await;
    let label = Flag::Keyword("$label1".into());
    rules.set(
        "alice@example.org",
        keeps(vec![keep(
            named("Archive/2026", true),
            &[Flag::Seen, label.clone()],
        )]),
    );
    // Нет папки и нет `:create` — «Входящие».
    rules.set(
        "bob@example.org",
        keeps(vec![keep(named("Missing", false), &[])]),
    );
    let e = envelope(Mode::Inbound, &["alice@example.org", "bob@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);

    let archived = folder(&env, "alice@example.org", "Archive/2026").await;
    assert_eq!(archived.len(), 1);
    assert!(
        archived[0].flags.contains(&Flag::Seen),
        "{:?}",
        archived[0].flags
    );
    assert!(
        archived[0].flags.contains(&label),
        "{:?}",
        archived[0].flags
    );
    assert!(env.inbox("alice@example.org").await.is_empty());
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    assert!(folder(&env, "bob@example.org", "Missing").await.is_empty());

    // Правила — для каждого получателя, с отправителем конверта.
    let calls = rules.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].scope, env.scope("alice@example.org").await);
    assert!(calls.iter().all(|c| c.mail_from == "sender@example.net"));
    // Журнал доставки: решение правил.
    assert!(
        reasons(&log, "alice@example.org")
            .iter()
            .any(|r| r == "sieve: fileinto Archive/2026"),
        "{:?}",
        log.events()
    );
}

#[tokio::test]
async fn two_folders_one_message() {
    let (env, rules, _log, d) = setup(false, cfg()).await;
    let scope = env.scope("alice@example.org").await;
    let work = MailboxStore::create(
        &env.meta,
        scope,
        NewMailbox {
            parent: None,
            name: "Work".into(),
            role: None,
        },
    )
    .await
    .expect("create");
    rules.set(
        "alice@example.org",
        keeps(vec![
            keep(MailboxRef::Inbox, &[]),
            keep(MailboxRef::Id(work.id), &[Flag::Flagged]),
        ]),
    );
    let e = envelope(Mode::Inbound, &["alice@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    let inbox = env.inbox("alice@example.org").await;
    assert_eq!(inbox.len(), 1);
    assert!(inbox[0].ends_with("Subject: hello\r\n\r\nbody\r\n"));
    let in_work = folder(&env, "alice@example.org", "Work").await;
    assert_eq!(in_work.len(), 1);
    assert!(in_work[0].flags.contains(&Flag::Flagged));
}

#[tokio::test]
async fn discard_without_delivery() {
    let (env, rules, log, d) = setup(false, cfg()).await;
    rules.set(
        "alice@example.org",
        RulesOutcome {
            discard: true,
            ..RulesOutcome::default()
        },
    );
    let e = envelope(Mode::Inbound, &["alice@example.org", "bob@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert!(env.inbox("alice@example.org").await.is_empty());
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    let events = log.events();
    let alice: Vec<_> = events
        .iter()
        .filter(|e| e.kind == Ek::Deliver && e.recipients.iter().any(|r| r == "alice@example.org"))
        .collect();
    assert_eq!(alice.len(), 1, "{events:?}");
    assert_eq!(alice[0].decision, Dd::Discard);
    assert_eq!(alice[0].reason.as_deref(), Some("sieve: discard"));
}

#[tokio::test]
async fn redirect_external_via_srs_and_local() {
    let (env, rules, log, d) = setup(true, cfg()).await;
    // Без копии: письмо уходит наружу через пересылку (SRS).
    rules.set(
        "alice@example.org",
        RulesOutcome {
            redirect: vec!["carol@partner.example".into()],
            ..RulesOutcome::default()
        },
    );
    // С копией (`:copy`): себе и локальному адресу.
    rules.set(
        "bob@example.org",
        RulesOutcome {
            redirect: vec!["carol@example.org".into()],
            ..RulesOutcome::implicit_keep()
        },
    );
    let e = envelope(Mode::Inbound, &["alice@example.org", "bob@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);

    let queued = env.queue.0.lock().expect("lock").clone();
    assert_eq!(queued.len(), 1);
    assert!(queued[0].0.starts_with("SRS0="), "{}", queued[0].0);
    assert_eq!(queued[0].1, ["carol@partner.example"]);
    assert!(env.inbox("alice@example.org").await.is_empty());
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    assert_eq!(env.inbox("carol@example.org").await.len(), 1);
    // Получатель перенаправления — тоже через правила (SMTP-34).
    assert_eq!(rules.calls().len(), 3);
    // Наружу — с цепочкой перенаправлений: петля через внешний сервер обрывается так же.
    assert!(queued[0].2.starts_with(b"X-ThePost-Redirected: "));
    assert!(
        reasons(&log, "alice@example.org")
            .iter()
            .any(|r| r == "sieve: redirect carol@partner.example")
    );
}

fn redirect(to: &str, keep: bool) -> RulesOutcome {
    RulesOutcome {
        redirect: vec![to.into()],
        ..if keep {
            RulesOutcome::implicit_keep()
        } else {
            RulesOutcome::default()
        }
    }
}

// ---------- SMTP-34: перенаправленное письмо — через правила получателя, петля обрывается ----------

#[tokio::test]
async fn redirect_loop_breaks_in_inbox() {
    let (env, rules, log, d) = setup(false, cfg()).await;
    // alice → bob → alice без копий: письмо не теряется и не ходит по кругу.
    rules.set("alice@example.org", redirect("bob@example.org", false));
    rules.set("bob@example.org", redirect("alice@example.org", false));
    let e = envelope(Mode::Inbound, &["alice@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);

    // Правила: alice, затем bob; вернувшееся к alice письмо правилами не обрабатывается.
    let calls = rules.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].scope, env.scope("bob@example.org").await);
    let alice = env.inbox("alice@example.org").await;
    assert_eq!(alice.len(), 1);
    assert_eq!(alice[0].matches("X-ThePost-Redirected: ").count(), 2);
    assert!(env.inbox("bob@example.org").await.is_empty());
    assert!(
        reasons(&log, "alice@example.org").iter().any(|r| r
            == "sieve: rules skipped, mailbox already in redirect chain; default folder"),
        "{:?}",
        reasons(&log, "alice@example.org")
    );
}

#[tokio::test]
async fn redirected_mail_is_sorted_by_recipient_rules() {
    let (env, rules, log, d) = setup(false, cfg()).await;
    rules.set("alice@example.org", redirect("bob@example.org", true));
    rules.set(
        "bob@example.org",
        keeps(vec![keep(named("From Alice", true), &[Flag::Flagged])]),
    );
    let e = envelope(Mode::Inbound, &["alice@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);

    assert_eq!(env.inbox("alice@example.org").await.len(), 1);
    assert!(env.inbox("bob@example.org").await.is_empty());
    let sorted = folder(&env, "bob@example.org", "From Alice").await;
    assert_eq!(sorted.len(), 1);
    assert!(sorted[0].flags.contains(&Flag::Flagged));
    // Правила bob видят письмо с цепочкой и исходным отправителем конверта.
    let calls = rules.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].mail_from, "sender@example.net");
    assert!(
        reasons(&log, "bob@example.org")
            .iter()
            .any(|r| r == "sieve: fileinto From Alice")
    );
}

#[tokio::test]
async fn redirect_hop_limit() {
    let mut c = cfg();
    c.delivery.redirect_hops = 1;
    let (env, rules, log, d) = setup(false, c).await;
    // alice → bob → carol → dave при пределе 1: правила carol уже не применяются.
    rules.set("alice@example.org", redirect("bob@example.org", false));
    rules.set("bob@example.org", redirect("carol@example.org", false));
    rules.set("carol@example.org", redirect("bob@example.org", false));
    let e = envelope(Mode::Inbound, &["alice@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(rules.calls().len(), 2);
    assert_eq!(env.inbox("carol@example.org").await.len(), 1);
    assert!(env.inbox("alice@example.org").await.is_empty());
    assert!(
        reasons(&log, "carol@example.org")
            .iter()
            .any(|r| r == "sieve: rules skipped, redirect hop limit; default folder")
    );
}

#[tokio::test]
async fn redirect_total_limit() {
    let mut c = cfg();
    c.delivery.redirect_total = 1;
    let (env, rules, log, d) = setup(false, c).await;
    rules.set("alice@example.org", redirect("bob@example.org", false));
    rules.set("bob@example.org", redirect("carol@example.org", false));
    rules.set("carol@example.org", RulesOutcome::default());
    let e = envelope(Mode::Inbound, &["alice@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    // Второе перенаправление — сверх предела: carol получает письмо без правил (её правило — `discard`).
    assert_eq!(rules.calls().len(), 2);
    assert_eq!(env.inbox("carol@example.org").await.len(), 1);
    assert!(
        reasons(&log, "carol@example.org")
            .iter()
            .any(|r| r == "sieve: rules skipped, redirect limit per message; default folder")
    );
}

#[tokio::test]
async fn rules_failure_delivers_to_inbox() {
    let (env, rules, log, d) = setup(false, cfg()).await;
    // Сбой хранилища правил — «Входящие» и запись в журнал.
    rules.fail_next(Error::UNAVAILABLE);
    let e = envelope(Mode::Inbound, &["alice@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.inbox("alice@example.org").await.len(), 1);
    assert!(
        reasons(&log, "alice@example.org")
            .iter()
            .any(|r| r.starts_with("sieve: rules failed")),
        "{:?}",
        log.events()
    );
    // Ошибка скрипта — implicit keep правил, ошибка — в журнал.
    rules.set(
        "bob@example.org",
        RulesOutcome::failed("line 3: unknown command"),
    );
    let e = envelope(Mode::Inbound, &["bob@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    assert!(
        reasons(&log, "bob@example.org")
            .iter()
            .any(|r| r == "sieve: errors: line 3: unknown command")
    );
}

#[tokio::test]
async fn internal_mail_goes_through_rules() {
    let (env, rules, _log, d) = setup(false, cfg()).await;
    rules.set(
        "bob@example.org",
        keeps(vec![keep(named("Colleagues", true), &[])]),
    );
    // Отправка вне сессии (JMAP).
    let sent = d
        .submit(
            TenantId::DEFAULT,
            None,
            "alice@example.org",
            &rcpts(&["bob@example.org"]),
            Bytes::from_static(MAIL),
        )
        .await
        .expect("submit");
    assert_eq!(
        sent.rcpts,
        [("bob@example.org".to_owned(), SubmitStatus::Delivered)]
    );
    assert_eq!(folder(&env, "bob@example.org", "Colleagues").await.len(), 1);
    // Сессия submission.
    let mut e = envelope(Mode::Submission, &["bob@example.org"]);
    e.mail_from = "alice@example.org".into();
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(folder(&env, "bob@example.org", "Colleagues").await.len(), 2);
    assert!(env.inbox("bob@example.org").await.is_empty());
    let calls = rules.calls();
    assert_eq!(calls.len(), 2);
    assert!(
        calls
            .iter()
            .all(|c| c.recipient == "bob@example.org" && c.mail_from == "alice@example.org")
    );
}

// ---------- SMTP-32: автоответ ----------

const ORIGINAL: &[u8] = b"From: sender@example.net\r\nTo: bob@example.org\r\nSubject: hello\r\n\
Message-ID: <orig-1@example.net>\r\n\r\nbody\r\n";

fn vacation(to: &str, from: &str) -> RulesOutcome {
    RulesOutcome {
        vacation: Some(AutoReply {
            to: to.into(),
            from: from.into(),
            subject: "Автоответ: hello".into(),
            body: ReplyBody::Text("Я в отпуске".into()),
            in_reply_to: Some("<orig-1@example.net>".into()),
            references: vec!["<orig-1@example.net>".into()],
        }),
        ..RulesOutcome::implicit_keep()
    }
}

#[tokio::test]
async fn auto_reply_to_external_sender_is_queued() {
    let (env, rules, log, d) = setup(true, cfg()).await;
    rules.set(
        "bob@example.org",
        vacation("sender@example.net", "bob@example.org"),
    );
    let e = envelope(Mode::Inbound, &["bob@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(ORIGINAL)).await.code, 250);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);

    let queued = env.queue.0.lock().expect("lock").clone();
    assert_eq!(queued.len(), 1);
    let (mail_from, to, raw) = &queued[0];
    // RFC 3834: конверт `<>`.
    assert_eq!(mail_from, "");
    assert_eq!(to, &["sender@example.net"]);
    let msg = mail_parser::MessageParser::new()
        .parse(raw.as_ref())
        .expect("parse");
    assert_eq!(
        msg.header_raw("Auto-Submitted").map(str::trim),
        Some("auto-replied")
    );
    assert_eq!(msg.in_reply_to().as_text(), Some("orig-1@example.net"));
    assert_eq!(msg.references().as_text(), Some("orig-1@example.net"));
    assert_eq!(msg.subject(), Some("Автоответ: hello"));
    assert_eq!(
        msg.body_text(0).as_deref().map(str::trim),
        Some("Я в отпуске")
    );
    // Журнал доставки: событие автоответа (без содержимого).
    let events = log.events();
    assert!(
        events.iter().any(|e| e.sender.is_empty()
            && e.recipients == ["sender@example.net"]
            && e.decision == Dd::Accept
            && e.reason.as_deref() == Some("auto-reply queued")),
        "{events:?}"
    );
}

#[tokio::test]
async fn auto_reply_to_internal_sender_goes_to_mailbox() {
    // Без очереди исходящей почты: локальный ответ от неё не зависит.
    let (env, rules, log, d) = setup(false, cfg()).await;
    rules.set(
        "bob@example.org",
        vacation("alice@example.org", "bob@example.org"),
    );
    d.submit(
        TenantId::DEFAULT,
        None,
        "alice@example.org",
        &rcpts(&["bob@example.org"]),
        Bytes::from_static(ORIGINAL),
    )
    .await
    .expect("submit");
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    let alice = env.inbox("alice@example.org").await;
    assert_eq!(alice.len(), 1);
    assert!(
        alice[0].contains("Auto-Submitted: auto-replied\r\n"),
        "{}",
        alice[0]
    );
    assert!(alice[0].contains("In-Reply-To: <orig-1@example.net>\r\n"));
    assert!(env.queue.0.lock().expect("lock").is_empty());
    // Автоответ доставлен через правила получателя (SMTP-34), с пустым отправителем конверта.
    let calls = rules.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].scope, env.scope("alice@example.org").await);
    assert_eq!(calls[1].mail_from, "");
    assert!(
        reasons(&log, "alice@example.org")
            .iter()
            .any(|r| r == "auto-reply delivered")
    );
}

#[tokio::test]
async fn auto_reply_redirected_back_stays_in_inbox() {
    let (env, rules, log, d) = setup(false, cfg()).await;
    // bob отвечает автоответом, alice перенаправляет всё bob: ответ возвращается к bob и остаётся во «Входящих».
    rules.set(
        "bob@example.org",
        vacation("alice@example.org", "bob@example.org"),
    );
    rules.set("alice@example.org", redirect("bob@example.org", false));
    d.submit(
        TenantId::DEFAULT,
        None,
        "alice@example.org",
        &rcpts(&["bob@example.org"]),
        Bytes::from_static(ORIGINAL),
    )
    .await
    .expect("submit");
    // Правила: bob (письмо), alice (автоответ); к вернувшемуся автоответу правила bob не применяются.
    assert_eq!(rules.calls().len(), 2);
    let bob = env.inbox("bob@example.org").await;
    assert_eq!(bob.len(), 2, "{bob:?}");
    assert_eq!(
        bob.iter()
            .filter(|m| m.contains("Auto-Submitted: auto-replied\r\n"))
            .count(),
        1
    );
    assert!(env.inbox("alice@example.org").await.is_empty());
    assert!(
        reasons(&log, "bob@example.org").iter().any(|r| r
            == "sieve: rules skipped, mailbox already in redirect chain; default folder")
    );
}

#[tokio::test]
async fn no_reply_to_auto_reply_loop() {
    let (env, rules, log, d) = setup(true, cfg()).await;
    // Оба в отпуске и отвечают друг другу.
    rules.set(
        "bob@example.org",
        vacation("alice@example.org", "bob@example.org"),
    );
    rules.set(
        "alice@example.org",
        vacation("bob@example.org", "alice@example.org"),
    );
    d.submit(
        TenantId::DEFAULT,
        None,
        "alice@example.org",
        &rcpts(&["bob@example.org"]),
        Bytes::from_static(ORIGINAL),
    )
    .await
    .expect("submit");
    let reply = env.inbox("alice@example.org").await;
    assert_eq!(reply.len(), 1);
    // Автоответ bob вернулся к alice через приём (как от внешнего сервера): с пустым конвертом и со своим —
    // alice на него не отвечает.
    for sender in ["", "bob@example.org"] {
        let mut e = envelope(Mode::Inbound, &["alice@example.org"]);
        e.mail_from = sender.into();
        let raw = Bytes::from(reply[0].clone().into_bytes());
        assert_eq!(d.data(&e, raw).await.code, 250);
    }
    assert_eq!(env.inbox("alice@example.org").await.len(), 3);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    assert!(env.queue.0.lock().expect("lock").is_empty());
    let why = reasons(&log, "bob@example.org");
    assert!(
        why.iter()
            .any(|r| r == "auto-reply suppressed: empty envelope sender"),
        "{why:?}"
    );
    assert!(
        why.iter()
            .any(|r| r == "auto-reply suppressed: auto-submitted message"),
        "{why:?}"
    );
    // Письмо списка рассылки — без ответа.
    rules.set(
        "bob@example.org",
        vacation("sender@example.net", "bob@example.org"),
    );
    let list =
        b"From: sender@example.net\r\nList-Id: <team.example.net>\r\nSubject: news\r\n\r\nx\r\n";
    let e = envelope(Mode::Inbound, &["bob@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(list)).await.code, 250);
    assert!(env.queue.0.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn auto_reply_rate_per_account() {
    let (env, rules, log, d) = setup(
        true,
        SmtpConfig {
            delivery: DeliveryConfig {
                vacation_rate: 1,
                ..DeliveryConfig::default()
            },
            ..cfg()
        },
    )
    .await;
    rules.set(
        "bob@example.org",
        vacation("sender@example.net", "bob@example.org"),
    );
    for _ in 0..2 {
        let e = envelope(Mode::Inbound, &["bob@example.org"]);
        assert_eq!(d.data(&e, Bytes::from_static(ORIGINAL)).await.code, 250);
    }
    assert_eq!(env.inbox("bob@example.org").await.len(), 2);
    assert_eq!(env.queue.0.lock().expect("lock").len(), 1);
    assert!(
        reasons(&log, "sender@example.net")
            .iter()
            .any(|r| r == "auto-reply suppressed: rate limit")
    );
}
