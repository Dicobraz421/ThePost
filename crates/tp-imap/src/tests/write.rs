//! APPEND, COPY, MOVE, EXPUNGE (IMAP-07): UIDPLUS (RFC 4315), MOVE (RFC 6851), корзина (SRS 4.7).

use std::time::Duration;

use futures::StreamExt;
use tp_contracts::bus::{EventBus, EventKind, EventStream};
use tp_contracts::ext::Decision;
use tp_contracts::meta::{MailboxStore, MessageQuery, MessageStore, SpecialUse};
use tp_contracts::retention::{TrashPurge, TrashPurgeRule, TrashStore};
use tp_contracts::{MailboxId, ModSeq};
use tp_testkit::{HookCall, PolicyLevel};

use super::{Client, Env};

const PLAIN: &str = "From: a@example.org\r\nTo: b@example.org\r\nSubject: hi\r\n\r\nHello\r\n";

async fn three() -> (Env, Client) {
    let env = Env::new().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    for _ in 0..3 {
        env.deliver(inbox, PLAIN.as_bytes(), Default::default())
            .await;
    }
    let mut c = env.logged_in().await;
    assert!(c.cmd("S1 SELECT INBOX").await.contains("S1 OK"));
    (env, c)
}

async fn count(env: &Env, role: SpecialUse) -> usize {
    MessageStore::list(
        &env.meta,
        env.scope(),
        env.mailbox(role),
        MessageQuery::All,
        100,
    )
    .await
    .expect("list")
    .len()
}

async fn uid_validity(env: &Env, role: SpecialUse) -> u32 {
    env.meta
        .status(env.scope(), env.mailbox(role))
        .await
        .expect("status")
        .uid_validity
        .0
}

#[tokio::test]
async fn append_appenduid() {
    let env = Env::new().await;
    let mut c = env.logged_in().await;
    let len = PLAIN.len();
    let v = uid_validity(&env, SpecialUse::Drafts).await;
    // RFC 9051, 6.3.12 (литерал LITERAL+), флаги и дата.
    let r = c
        .cmd(&format!(
            "A1 APPEND Drafts (\\Seen \\Draft) \"05-Oct-2026 14:15:00 +0300\" {{{len}+}}\r\n{PLAIN}"
        ))
        .await;
    assert_eq!(r, format!("A1 OK [APPENDUID {v} 1] APPEND completed\r\n"));
    c.cmd("A2 SELECT Drafts").await;
    assert_eq!(
        c.cmd("A3 FETCH 1 (FLAGS INTERNALDATE BODY.PEEK[HEADER.FIELDS (SUBJECT)])")
            .await,
        "* 1 FETCH (FLAGS (\\Seen \\Draft) INTERNALDATE \"05-Oct-2026 11:15:00 +0000\" \
         BODY[HEADER.FIELDS (SUBJECT)] {15}\r\nSubject: hi\r\n\r\n)\r\nA3 OK FETCH completed\r\n"
    );
    // Синхронизирующий литерал: `+`, затем данные; в выбранную папку — EXISTS.
    c.send(&format!("A4 APPEND Drafts {{{len}}}\r\n")).await;
    assert!(c.line().await.starts_with("+ "));
    c.send(&format!("{PLAIN}\r\n")).await;
    assert_eq!(
        c.until("A4").await,
        format!("* 2 EXISTS\r\nA4 OK [APPENDUID {v} 2] APPEND completed\r\n")
    );
    // Нет папки — TRYCREATE; \Recent — BAD.
    let r = c
        .cmd(&format!("A5 APPEND Nope {{{len}+}}\r\n{PLAIN}"))
        .await;
    assert!(r.starts_with("A5 NO [TRYCREATE]"), "{r}");
    let r = c
        .cmd(&format!("A6 APPEND INBOX (\\Recent) {{{len}+}}\r\n{PLAIN}"))
        .await;
    assert!(r.starts_with("A6 BAD"), "{r}");
}

#[tokio::test]
async fn copy_copyuid() {
    let (env, mut c) = three().await;
    c.cmd("C0 CREATE Archive").await;
    let r = c.cmd("C1 COPY 2:3 Archive").await;
    assert!(r.starts_with("C1 OK [COPYUID "), "{r}");
    assert!(r.contains(" 2:3 1:2] COPY completed"), "{r}");
    assert_eq!(count(&env, SpecialUse::Inbox).await, 3);
    // В выбранную папку — EXISTS; пустой UID-набор — OK без COPYUID.
    let r = c.cmd("C2 UID COPY 1 INBOX").await;
    assert!(r.starts_with("* 4 EXISTS\r\nC2 OK [COPYUID "), "{r}");
    assert_eq!(
        c.cmd("C3 UID COPY 99 Archive").await,
        "C3 OK UID COPY completed\r\n"
    );
    assert!(
        c.cmd("C4 COPY 1 Nope")
            .await
            .starts_with("C4 NO [TRYCREATE]")
    );
    assert!(c.cmd("C5 COPY 9 Archive").await.starts_with("C5 BAD"));
}

#[tokio::test]
async fn move_and_move_to_trash() {
    let (env, mut c) = three().await;
    c.cmd("M0 CREATE Work").await;
    // RFC 6851, 4.3: COPYUID нетегированным, затем EXPUNGE.
    let r = c.cmd("M1 MOVE 1:2 Work").await;
    let lines: Vec<&str> = r.lines().collect();
    assert!(
        lines[0].starts_with("* OK [COPYUID ") && lines[0].contains(" 1:2 1:2]"),
        "{r}"
    );
    assert_eq!(
        &lines[1..],
        ["* 2 EXPUNGE", "* 1 EXPUNGE", "M1 OK MOVE completed"]
    );
    assert_eq!(
        c.cmd("M2 FETCH 1:* UID").await,
        "* 1 FETCH (UID 3)\r\nM2 OK FETCH completed\r\n"
    );
    // MOVE в корзину — удаление в корзину (SRS 4.7).
    let tv = uid_validity(&env, SpecialUse::Trash).await;
    let r = c.cmd("M3 UID MOVE 3 Trash").await;
    assert_eq!(
        r,
        format!("* OK [COPYUID {tv} 3 1] Moved\r\n* 1 EXPUNGE\r\nM3 OK UID MOVE completed\r\n")
    );
    assert_eq!(count(&env, SpecialUse::Trash).await, 1);
    assert_eq!(count(&env, SpecialUse::Inbox).await, 0);
    // EXAMINE — MOVE запрещён.
    c.cmd("M4 EXAMINE Work").await;
    assert!(c.cmd("M5 MOVE 1 INBOX").await.starts_with("M5 NO"));
}

#[tokio::test]
async fn expunge_to_trash() {
    let (env, mut c) = three().await;
    c.cmd("E1 STORE 1,3 +FLAGS.SILENT (\\Deleted)").await;
    assert_eq!(
        c.cmd("E2 EXPUNGE").await,
        "* 3 EXPUNGE\r\n* 1 EXPUNGE\r\nE2 OK EXPUNGE completed\r\n"
    );
    assert_eq!(count(&env, SpecialUse::Trash).await, 2);
    assert_eq!(
        c.cmd("E3 FETCH 1:* UID").await,
        "* 1 FETCH (UID 2)\r\nE3 OK FETCH completed\r\n"
    );
    // UID EXPUNGE — только указанные.
    let inbox = env.mailbox(SpecialUse::Inbox);
    env.deliver(inbox, PLAIN.as_bytes(), Default::default())
        .await;
    let mut c = env.logged_in().await;
    c.cmd("U0 SELECT INBOX").await;
    c.cmd("U1 STORE 1:2 +FLAGS.SILENT (\\Deleted)").await;
    assert_eq!(
        c.cmd("U2 UID EXPUNGE 4").await,
        "* 2 EXPUNGE\r\nU2 OK UID EXPUNGE completed\r\n"
    );
    // CLOSE — без ответов EXPUNGE, письмо в корзине.
    assert_eq!(c.cmd("U3 CLOSE").await, "U3 OK CLOSE completed\r\n");
    assert_eq!(count(&env, SpecialUse::Inbox).await, 0);
    assert_eq!(count(&env, SpecialUse::Trash).await, 4);
    assert!(c.cmd("U4 FETCH 1 UID").await.starts_with("U4 BAD"));
    // EXPUNGE в корзине — «очистить корзину»: trash_expunge_*, trash_uid_expunge_*.
    // EXAMINE — EXPUNGE запрещён.
    c.cmd("T4 EXAMINE INBOX").await;
    assert!(c.cmd("T5 EXPUNGE").await.starts_with("T5 NO"));
}

/// Три письма в корзине, выбрана корзина.
async fn trash_of_three() -> (Env, Client) {
    let (env, mut c) = three().await;
    c.cmd("P0 MOVE 1:3 Trash").await;
    assert!(c.cmd("P1 SELECT Trash").await.contains("P1 OK"));
    (env, c)
}

async fn hidden_count(env: &Env) -> usize {
    env.trash
        .list_hidden(env.scope(), 0, 100)
        .await
        .expect("hidden")
        .len()
}

/// Следующее `MailboxChanged` подписки (не дольше секунды).
async fn changed(events: &mut EventStream) -> Option<(MailboxId, ModSeq)> {
    loop {
        let e = tokio::time::timeout(Duration::from_secs(1), events.next())
            .await
            .ok()??;
        if let EventKind::MailboxChanged { mailbox, modseq } = e.kind {
            return Some((mailbox, modseq));
        }
    }
}

fn final_deletes(env: &Env) -> usize {
    env.hook
        .calls()
        .iter()
        .filter(|c| matches!(c, HookCall::FinalDelete(_)))
        .count()
}

#[tokio::test]
async fn trash_expunge_hides_by_default() {
    let (env, mut c) = trash_of_three().await;
    let trash = env.mailbox(SpecialUse::Trash);
    c.cmd("H1 STORE 1,3 +FLAGS.SILENT (\\Deleted)").await;
    let mut events = env.bus.subscribe(env.scope()).await.expect("subscribe");
    // `hide`: письма исчезают у пользователя и хранятся скрыто; точка принятия решений не нужна.
    assert_eq!(
        c.cmd("H2 EXPUNGE").await,
        "* 3 EXPUNGE\r\n* 1 EXPUNGE\r\nH2 OK EXPUNGE completed\r\n"
    );
    assert_eq!(count(&env, SpecialUse::Trash).await, 1);
    assert_eq!(hidden_count(&env).await, 2);
    assert_eq!(final_deletes(&env), 0);
    // MailboxChanged корзины публикует сессия (TrashStore в шину не пишет) с ModSeq изменения.
    let (mailbox, modseq) = changed(&mut events).await.expect("event");
    assert_eq!(mailbox, trash);
    let st = env.meta.status(env.scope(), trash).await.expect("status");
    assert_eq!(modseq, st.highest_modseq);
    // Другая сессия видит одно письмо.
    let mut other = env.logged_in().await;
    let r = other.cmd("O1 SELECT Trash").await;
    assert!(r.contains("* 1 EXISTS"), "{r}");
}

#[tokio::test]
async fn trash_uid_expunge_erases_unless_legal_hold() {
    let (env, mut c) = trash_of_three().await;
    let rule = TrashPurgeRule {
        mode: TrashPurge::Erase,
        user_can_choose: false,
    };
    env.policies
        .set_trash_purge_rule(env.scope().tenant, PolicyLevel::Tenant, Some(rule));
    c.cmd("E1 STORE 1:3 +FLAGS.SILENT (\\Deleted)").await;
    // `erase`: только указанные UID, перед стиранием — точка принятия решений.
    assert_eq!(
        c.cmd("E2 UID EXPUNGE 1").await,
        "* 1 EXPUNGE\r\nE2 OK UID EXPUNGE completed\r\n"
    );
    assert_eq!(count(&env, SpecialUse::Trash).await, 2);
    assert_eq!(hidden_count(&env).await, 0);
    assert_eq!(final_deletes(&env), 1);
    // Legal Hold: стирание запрещено — письмо исчезает у пользователя, но хранится скрыто.
    env.hook
        .set_final_delete(Decision::Deny("legal hold".into()));
    assert_eq!(
        c.cmd("E3 UID EXPUNGE 2").await,
        "* 1 EXPUNGE\r\nE3 OK UID EXPUNGE completed\r\n"
    );
    assert_eq!(count(&env, SpecialUse::Trash).await, 1);
    assert_eq!(hidden_count(&env).await, 1);
    // CLOSE в корзине — тоже «очистить корзину», без ответов EXPUNGE.
    assert_eq!(c.cmd("E4 CLOSE").await, "E4 OK CLOSE completed\r\n");
    assert_eq!(count(&env, SpecialUse::Trash).await, 0);
    assert_eq!(hidden_count(&env).await, 2);
}

#[tokio::test]
async fn append_is_not_a_delivery() {
    let env = Env::new().await;
    let mut c = env.logged_in().await;
    let len = PLAIN.len();
    let r = c
        .cmd(&format!("A1 APPEND Drafts (\\Draft) {{{len}+}}\r\n{PLAIN}"))
        .await;
    assert!(r.starts_with("A1 OK [APPENDUID "), "{r}");
    // Точки принятия решений доставки не вызваны; запрет доставки APPEND не мешает.
    assert!(env.hook.calls().is_empty(), "{:?}", env.hook.calls());
    env.hook.set_delivery(Decision::Deny("no delivery".into()));
    let r = c
        .cmd(&format!("A2 APPEND Drafts {{{len}+}}\r\n{PLAIN}"))
        .await;
    assert!(r.starts_with("A2 OK [APPENDUID "), "{r}");
    assert!(env.hook.calls().is_empty());
    assert_eq!(count(&env, SpecialUse::Drafts).await, 2);
    // Письмо хранится побайтово, без добавленных заголовков.
    c.cmd("A3 SELECT Drafts").await;
    let r = c.cmd("A4 FETCH 1 BODY.PEEK[]").await;
    assert!(r.contains(&format!("BODY[] {{{len}}}\r\n{PLAIN})")), "{r}");
}
