//! APPEND, COPY, MOVE, EXPUNGE (IMAP-07): UIDPLUS (RFC 4315), MOVE (RFC 6851), корзина (SRS 4.7).

use tp_contracts::meta::{MailboxStore, MessageQuery, MessageStore, SpecialUse};

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
    // В корзине EXPUNGE не удаляет окончательно.
    c.cmd("T1 SELECT Trash").await;
    c.cmd("T2 STORE 1 +FLAGS.SILENT (\\Deleted)").await;
    assert!(c.cmd("T3 EXPUNGE").await.starts_with("T3 NO [CANNOT]"));
    assert_eq!(count(&env, SpecialUse::Trash).await, 4);
    // EXAMINE — EXPUNGE запрещён.
    c.cmd("T4 EXAMINE INBOX").await;
    assert!(c.cmd("T5 EXPUNGE").await.starts_with("T5 NO"));
}
