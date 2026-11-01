//! SEARCH и ESEARCH (IMAP-06): метаданные и мок-индекс (`FakeSearch`).

use tp_contracts::meta::{Flag, SpecialUse};

use super::{Client, Env};

fn mail(from: &str, subject: &str, date: &str, body: &str) -> Vec<u8> {
    format!(
        "From: {from}\r\nTo: bob@example.org\r\nSubject: {subject}\r\nDate: {date}\r\n\
         Message-ID: <{subject}@example.org>\r\nX-Project: Apollo\r\n\r\n{body}\r\n"
    )
    .into_bytes()
}

/// Три письма во «Входящих», папка выбрана.
async fn env_selected() -> (Env, Client) {
    let env = Env::new().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    env.deliver(
        inbox,
        &mail(
            "Alice <alice@example.com>",
            "Report",
            "Thu, 29 Oct 2026 10:34:00 +0300",
            "quarterly numbers",
        ),
        [Flag::Seen].into(),
    )
    .await;
    env.deliver(
        inbox,
        &mail(
            "=?utf-8?b?0JHQvtGA0LjRgQ==?= <boris@example.ru>",
            "=?utf-8?b?0J7RgtGH0ZHRgg==?=",
            "Sat, 31 Oct 2026 23:30:00 -0500",
            "draft text with attachment",
        ),
        [Flag::Flagged, Flag::Keyword("$Work".into())].into(),
    )
    .await;
    env.deliver(
        inbox,
        &mail(
            "carol@example.net",
            "Lunch",
            "Sun, 1 Nov 2026 12:00:00 +0000",
            &"x".repeat(2000),
        ),
        Default::default(),
    )
    .await;
    let mut c = env.logged_in().await;
    assert!(c.cmd("S1 SELECT INBOX").await.contains("S1 OK"));
    (env, c)
}

async fn found(c: &mut Client, criteria: &str) -> String {
    let r = c.cmd(&format!("Q SEARCH {criteria}")).await;
    assert!(r.ends_with("Q OK SEARCH completed\r\n"), "{r}");
    r.lines().next().unwrap_or_default().to_owned()
}

#[tokio::test]
async fn search_metadata() {
    let (_env, mut c) = env_selected().await;
    assert_eq!(found(&mut c, "ALL").await, "* SEARCH 1 2 3");
    assert_eq!(found(&mut c, "SEEN").await, "* SEARCH 1");
    assert_eq!(found(&mut c, "UNSEEN UNFLAGGED").await, "* SEARCH 3");
    assert_eq!(found(&mut c, "KEYWORD $work").await, "* SEARCH 2");
    assert_eq!(found(&mut c, "OR SEEN FLAGGED").await, "* SEARCH 1 2");
    assert_eq!(found(&mut c, "NOT 2:*").await, "* SEARCH 1");
    assert_eq!(found(&mut c, "LARGER 1000").await, "* SEARCH 3");
    assert_eq!(found(&mut c, "SMALLER 1000 UID 2:*").await, "* SEARCH 2");
    // Поля конверта — подстрока без учёта регистра, текст декодирован.
    assert_eq!(found(&mut c, "FROM ALICE").await, "* SEARCH 1");
    assert_eq!(found(&mut c, "FROM {10+}\r\nборис").await, "* SEARCH 2");
    assert_eq!(found(&mut c, "SUBJECT {6+}\r\nотч").await, "* SEARCH 2");
    assert_eq!(
        found(&mut c, "HEADER Message-ID <lunch@").await,
        "* SEARCH 3"
    );
    assert_eq!(found(&mut c, "TO nobody").await, "* SEARCH");
    // Дата письма — в его часовом поясе (RFC 9051, 6.4.4).
    assert_eq!(found(&mut c, "SENTON 31-Oct-2026").await, "* SEARCH 2");
    assert_eq!(found(&mut c, "SENTSINCE 1-Nov-2026").await, "* SEARCH 3");
    assert_eq!(found(&mut c, "SENTBEFORE 30-Oct-2026").await, "* SEARCH 1");
    // Дата приёма у всех писем тестов — 21-Sep-2026.
    assert_eq!(found(&mut c, "ON 21-Sep-2026").await, "* SEARCH 1 2 3");
    assert_eq!(found(&mut c, "BEFORE 21-Sep-2026").await, "* SEARCH");
    // \Recent нет.
    assert_eq!(found(&mut c, "NEW").await, "* SEARCH");
    // UID SEARCH — UID; неизвестная кодировка — BADCHARSET.
    assert_eq!(
        c.cmd("U1 UID SEARCH FLAGGED").await,
        "* SEARCH 2\r\nU1 OK UID SEARCH completed\r\n"
    );
    assert!(
        c.cmd("U2 SEARCH CHARSET KOI8-R ALL")
            .await
            .starts_with("U2 NO [BADCHARSET (US-ASCII UTF-8)]")
    );
    assert!(c.cmd("U3 SEARCH CHARSET UTF-8 ALL").await.contains("U3 OK"));
}

#[tokio::test]
async fn search_text_through_index() {
    let (_env, mut c) = env_selected().await;
    // BODY, TEXT и прочие HEADER — через SearchIndex.
    assert_eq!(found(&mut c, "BODY quarterly").await, "* SEARCH 1");
    assert_eq!(found(&mut c, "TEXT attachment").await, "* SEARCH 2");
    assert_eq!(found(&mut c, "TEXT report").await, "* SEARCH 1");
    assert_eq!(
        found(&mut c, "HEADER X-Project apollo").await,
        "* SEARCH 1 2 3"
    );
    assert_eq!(found(&mut c, "NOT BODY quarterly SEEN").await, "* SEARCH");
    assert_eq!(
        found(&mut c, "OR BODY quarterly BODY draft").await,
        "* SEARCH 1 2"
    );
}

#[tokio::test]
async fn esearch_and_modseq() {
    let (env, mut c) = env_selected().await;
    // RFC 4731, 3.1.
    assert_eq!(
        c.cmd("E1 SEARCH RETURN (MIN COUNT) UNSEEN").await,
        "* ESEARCH (TAG \"E1\") MIN 2 COUNT 2\r\nE1 OK SEARCH completed\r\n"
    );
    assert_eq!(
        c.cmd("E2 UID SEARCH RETURN () ALL").await,
        "* ESEARCH (TAG \"E2\") UID ALL 1:3\r\nE2 OK UID SEARCH completed\r\n"
    );
    assert_eq!(
        c.cmd("E3 SEARCH RETURN (MAX) DELETED").await,
        "* ESEARCH (TAG \"E3\")\r\nE3 OK SEARCH completed\r\n"
    );
    assert!(
        c.cmd("E4 SEARCH RETURN (SAVE) ALL")
            .await
            .starts_with("E4 BAD")
    );
    // MODSEQ (RFC 7162, 3.1.5): изменённые не раньше, ответ — с наибольшим MODSEQ.
    c.cmd("E5 STORE 3 +FLAGS.SILENT (\\Answered)").await;
    let r = c.cmd("E6 FETCH 3 (MODSEQ)").await;
    let m: u64 = r
        .split("MODSEQ (")
        .nth(1)
        .and_then(|s| s.split(')').next())
        .and_then(|s| s.parse().ok())
        .expect("modseq");
    assert_eq!(
        c.cmd(&format!("E7 SEARCH MODSEQ {m}")).await,
        format!("* SEARCH 3 (MODSEQ {m})\r\nE7 OK SEARCH completed\r\n")
    );
    // IMAP4rev2: всегда ESEARCH.
    let mut c2 = env.logged_in().await;
    c2.cmd("R1 ENABLE IMAP4rev2").await;
    c2.cmd("R2 SELECT INBOX").await;
    assert_eq!(
        c2.cmd("R3 SEARCH SEEN").await,
        "* ESEARCH (TAG \"R3\") ALL 1\r\nR3 OK SEARCH completed\r\n"
    );
    // Без выбранной папки — BAD.
    let mut c3 = env.logged_in().await;
    assert!(
        c3.cmd("N1 SEARCH RETURN () ALL")
            .await
            .starts_with("N1 BAD")
    );
}
