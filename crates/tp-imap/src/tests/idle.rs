//! IDLE и уведомления (IMAP-08): изменения другой сессии и доставка приходят в IDLE; NOOP; таймаут; BYE.

use std::sync::Arc;
use std::time::Duration;

use tp_contracts::bus::{Event, EventBus, EventKind};
use tp_contracts::meta::SpecialUse;
use tp_testkit::MemEventBus;

use super::{Client, Env, LOGIN, PASSWORD};
use crate::ext::Extensions;

const PLAIN: &[u8] = b"From: a@example.org\r\nTo: b@example.org\r\nSubject: hi\r\n\r\nHello\r\n";

/// Строка ответа с ограничением времени (тест не зависает, если уведомление не пришло).
async fn line(c: &mut Client) -> String {
    tokio::time::timeout(Duration::from_secs(5), c.line())
        .await
        .expect("no notification in time")
}

async fn selected(env: &Env) -> Client {
    let mut c = env.logged_in().await;
    assert!(c.cmd("S1 SELECT INBOX").await.contains("S1 OK"));
    c
}

#[tokio::test]
async fn idle_receives_changes_of_other_session() {
    let env = Env::new().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    env.deliver(inbox, PLAIN, Default::default()).await;
    let mut a = selected(&env).await;
    let mut b = selected(&env).await;
    a.send("I1 IDLE\r\n").await;
    assert_eq!(line(&mut a).await, "+ idling\r\n");
    // Флаги из другой сессии.
    b.cmd("B1 STORE 1 +FLAGS (\\Flagged)").await;
    assert_eq!(
        line(&mut a).await,
        "* 1 FETCH (FLAGS (\\Flagged) UID 1)\r\n"
    );
    // Новое письмо (доставка SMTP).
    env.deliver(inbox, PLAIN, Default::default()).await;
    assert_eq!(line(&mut a).await, "* 2 EXISTS\r\n");
    // Удаление в другой сессии.
    b.cmd("B2 STORE 1 +FLAGS.SILENT (\\Deleted)").await;
    assert_eq!(
        line(&mut a).await,
        "* 1 FETCH (FLAGS (\\Flagged \\Deleted) UID 1)\r\n"
    );
    b.cmd("B3 EXPUNGE").await;
    assert_eq!(line(&mut a).await, "* 1 EXPUNGE\r\n");
    a.send("DONE\r\n").await;
    assert_eq!(line(&mut a).await, "I1 OK IDLE terminated\r\n");
    // Снимок обновлён: одно письмо, UID 2.
    assert_eq!(
        a.cmd("A2 FETCH 1:* UID").await,
        "* 1 FETCH (UID 2)\r\nA2 OK FETCH completed\r\n"
    );
}

#[tokio::test]
async fn noop_reports_changes_and_bus_reconnect() {
    let env = Env::new().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    env.deliver(inbox, PLAIN, Default::default()).await;
    let mut a = env.logged_in().await;
    // NOOP — опрос изменений (CONDSTORE — с MODSEQ).
    a.cmd("A0 ENABLE CONDSTORE").await;
    a.cmd("A00 SELECT INBOX").await;
    let mut b = selected(&env).await;
    b.cmd("B1 STORE 1 +FLAGS.SILENT (\\Seen)").await;
    env.deliver(inbox, PLAIN, Default::default()).await;
    let r = a.cmd("A1 NOOP").await;
    assert!(
        r.starts_with("* 2 EXISTS\r\n* 1 FETCH (FLAGS (\\Seen) UID 1 MODSEQ ("),
        "{r}"
    );
    assert!(r.ends_with("A1 OK NOOP completed\r\n"), "{r}");
    assert_eq!(a.cmd("A2 NOOP").await, "A2 OK NOOP completed\r\n");
    // Обрыв шины: сессия переподписывается и сверяется с журналом.
    a.send("I1 IDLE\r\n").await;
    assert_eq!(line(&mut a).await, "+ idling\r\n");
    env.bus.disconnect();
    env.deliver(inbox, PLAIN, Default::default()).await;
    assert_eq!(line(&mut a).await, "* 3 EXISTS\r\n");
    a.send("DONE\r\n").await;
    assert_eq!(line(&mut a).await, "I1 OK IDLE terminated\r\n");
    // Не DONE — BAD.
    a.send("I2 IDLE\r\n").await;
    assert_eq!(line(&mut a).await, "+ idling\r\n");
    a.send("NOOP\r\n").await;
    assert!(line(&mut a).await.starts_with("I2 BAD"));
}

/// IMAP-17: подписчик шины отстал — вместо пропущенных `MailboxChanged` пришёл `Resync`; IDLE сверяется с журналом.
#[tokio::test]
async fn idle_resync_reports_missed_changes() {
    let env = Env::new().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    env.deliver(inbox, PLAIN, Default::default()).await;
    // Шина сессии отдельно от шины хранилища: `MailboxChanged` доставок до сессии не доходят,
    // буфер на один сигнал — два подряд дают `Resync`.
    let bus = MemEventBus::new(1);
    let mut ctx = (*env.ctx()).clone();
    ctx.deps.bus = Arc::new(bus.clone());
    let mut a = Env::client_ctx(Arc::new(ctx), true, Extensions::default()).await;
    assert!(
        a.cmd(&format!("L1 LOGIN {LOGIN} {PASSWORD}"))
            .await
            .contains("L1 OK")
    );
    assert!(a.cmd("S1 SELECT INBOX").await.contains("S1 OK"));
    let mut b = selected(&env).await;
    a.send("I1 IDLE\r\n").await;
    assert_eq!(line(&mut a).await, "+ idling\r\n");
    env.deliver(inbox, PLAIN, Default::default()).await;
    b.cmd("B1 STORE 1 +FLAGS.SILENT (\\Seen)").await;
    let lost = Event {
        tenant: env.principal.tenant,
        account: Some(env.principal.account),
        kind: EventKind::MailboxListChanged,
    };
    // Без точки ожидания между публикациями: сессия не успевает прочитать первый сигнал.
    bus.publish(lost.clone()).await.expect("publish");
    bus.publish(lost).await.expect("publish");
    assert_eq!(line(&mut a).await, "* 2 EXISTS\r\n");
    assert_eq!(line(&mut a).await, "* 1 FETCH (FLAGS (\\Seen) UID 1)\r\n");
    a.send("DONE\r\n").await;
    assert_eq!(line(&mut a).await, "I1 OK IDLE terminated\r\n");
}

#[tokio::test]
async fn idle_timeout_and_shutdown() {
    let mut env = Env::new().await;
    env.config.timeouts.idle = Duration::from_millis(100);
    let mut a = env.logged_in().await;
    // IDLE допустим и без выбранной папки.
    a.send("I1 IDLE\r\n").await;
    assert_eq!(line(&mut a).await, "+ idling\r\n");
    assert!(line(&mut a).await.starts_with("* BYE "));
    assert_eq!(a.rest().await, "");

    env.config.timeouts.idle = Duration::from_secs(600);
    let mut b = env.logged_in().await;
    b.send("I2 IDLE\r\n").await;
    assert_eq!(line(&mut b).await, "+ idling\r\n");
    env.shutdown.cancel();
    assert_eq!(
        line(&mut b).await,
        "* BYE [UNAVAILABLE] Server shutting down\r\n"
    );
}
