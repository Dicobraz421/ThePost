//! STORE и CONDSTORE (IMAP-05): примеры RFC 9051, 6.4.6 и RFC 7162, 3.1.

use tp_contracts::meta::SpecialUse;

use super::{Client, Env};

const PLAIN: &[u8] = b"From: a@example.org\r\nTo: b@example.org\r\nSubject: hi\r\n\r\nHello\r\n";

/// Три письма во «Входящих», папка выбрана.
async fn three() -> (Env, Client) {
    let env = Env::new().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    for _ in 0..3 {
        env.deliver(inbox, PLAIN, Default::default()).await;
    }
    let mut c = env.logged_in().await;
    assert!(c.cmd("S1 SELECT INBOX").await.contains("S1 OK"));
    (env, c)
}

/// `MODSEQ` письма с номером `seq`.
async fn modseq(c: &mut Client, seq: u32) -> u64 {
    let r = c.cmd(&format!("M{seq} FETCH {seq} (MODSEQ)")).await;
    let start = r.find("MODSEQ (").expect("modseq") + "MODSEQ (".len();
    let end = start + r[start..].find(')').expect("paren");
    r[start..end].parse().expect("number")
}

#[tokio::test]
async fn store_flags_and_silent() {
    let (_env, mut c) = three().await;
    // RFC 9051, 6.4.6: ответ — новые флаги.
    assert_eq!(
        c.cmd("A1 STORE 2:3 +FLAGS (\\Deleted)").await,
        "* 2 FETCH (FLAGS (\\Deleted))\r\n* 3 FETCH (FLAGS (\\Deleted))\r\nA1 OK STORE completed\r\n"
    );
    assert_eq!(
        c.cmd("A2 STORE 2 FLAGS ($Forwarded \\Seen)").await,
        "* 2 FETCH (FLAGS (\\Seen $Forwarded))\r\nA2 OK STORE completed\r\n"
    );
    assert_eq!(
        c.cmd("A3 UID STORE 3 -FLAGS.SILENT (\\Deleted)").await,
        "A3 OK UID STORE completed\r\n"
    );
    assert_eq!(
        c.cmd("A4 FETCH 1:3 FLAGS").await,
        "* 1 FETCH (FLAGS ())\r\n* 2 FETCH (FLAGS (\\Seen $Forwarded))\r\n* 3 FETCH (FLAGS ())\r\nA4 OK FETCH completed\r\n"
    );
    // UID STORE возвращает UID.
    assert_eq!(
        c.cmd("A5 UID STORE 1 +FLAGS (\\Answered)").await,
        "* 1 FETCH (FLAGS (\\Answered) UID 1)\r\nA5 OK UID STORE completed\r\n"
    );
    // \Recent установить нельзя, номер вне папки — BAD.
    assert!(
        c.cmd("A6 STORE 1 +FLAGS (\\Recent)")
            .await
            .contains("A6 BAD")
    );
    assert!(c.cmd("A7 STORE 9 +FLAGS (\\Seen)").await.contains("A7 BAD"));
    // UID вне папки — пусто, без ошибки.
    assert_eq!(
        c.cmd("A8 UID STORE 99 +FLAGS (\\Seen)").await,
        "A8 OK UID STORE completed\r\n"
    );
}

#[tokio::test]
async fn store_read_only() {
    let (_env, mut c) = three().await;
    assert!(
        c.cmd("E1 EXAMINE INBOX")
            .await
            .contains("E1 OK [READ-ONLY]")
    );
    assert!(
        c.cmd("E2 STORE 1 +FLAGS (\\Seen)")
            .await
            .starts_with("E2 NO")
    );
}

#[tokio::test]
async fn condstore_unchangedsince_modified() {
    let (_env, mut c) = three().await;
    let r = c.cmd("A1 STORE 1:2 +FLAGS.SILENT (\\Flagged)").await;
    assert_eq!(r, "A1 OK STORE completed\r\n");
    let x = modseq(&mut c, 1).await;
    assert_eq!(modseq(&mut c, 2).await, x);
    // Письмо 3 изменено позже.
    c.cmd("A2 STORE 3 +FLAGS.SILENT (\\Answered)").await;
    let y = modseq(&mut c, 3).await;
    assert!(y > x);
    // RFC 7162, 3.1.3, пример: .SILENT при CONDSTORE — FETCH с MODSEQ для изменённых, конфликт — MODIFIED.
    let r = c
        .cmd(&format!(
            "A3 UID STORE 1:3 (UNCHANGEDSINCE {x}) +FLAGS.SILENT (\\Seen)"
        ))
        .await;
    let z = modseq(&mut c, 1).await;
    assert!(z > y);
    assert_eq!(
        r,
        format!(
            "* 1 FETCH (UID 1 MODSEQ ({z}))\r\n* 2 FETCH (UID 2 MODSEQ ({z}))\r\n\
             A3 OK [MODIFIED 3] Conditional STORE failed\r\n"
        )
    );
    // Без .SILENT — FLAGS и MODSEQ; номера в MODIFIED — номера последовательности.
    let r = c
        .cmd(&format!(
            "A4 STORE 2:3 (UNCHANGEDSINCE {y}) -FLAGS (\\Flagged)"
        ))
        .await;
    // Письмо 3 прошло проверку, но \Flagged у него не было: флаги и MODSEQ прежние.
    assert_eq!(
        r,
        format!(
            "* 3 FETCH (FLAGS (\\Answered) MODSEQ ({y}))\r\n\
             A4 OK [MODIFIED 2] Conditional STORE failed\r\n"
        )
    );
    // UNCHANGEDSINCE 0 — всегда конфликт (RFC 7162, 3.1.3).
    assert_eq!(
        c.cmd("A5 STORE 1 (UNCHANGEDSINCE 0) +FLAGS (\\Draft)")
            .await,
        "A5 OK [MODIFIED 1] Conditional STORE failed\r\n"
    );
}

#[tokio::test]
async fn condstore_changedsince_and_select() {
    let env = Env::new().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    for _ in 0..3 {
        env.deliver(inbox, PLAIN, Default::default()).await;
    }
    let mut c = env.logged_in().await;
    // SELECT (CONDSTORE) — HIGHESTMODSEQ (RFC 7162, 3.1.8).
    let r = c.cmd("S1 SELECT INBOX (CONDSTORE)").await;
    assert!(r.contains("* OK [HIGHESTMODSEQ "), "{r}");
    let base = modseq(&mut c, 3).await;
    c.cmd("A1 STORE 2 +FLAGS.SILENT (\\Seen)").await;
    // RFC 7162, 3.1.4.1: только изменённые после CHANGEDSINCE, с MODSEQ.
    let r = c
        .cmd(&format!("A2 FETCH 1:* (FLAGS) (CHANGEDSINCE {base})"))
        .await;
    assert_eq!(
        r,
        format!(
            "* 2 FETCH (FLAGS (\\Seen) MODSEQ ({}))\r\nA2 OK FETCH completed\r\n",
            base + 1
        )
    );
    // STATUS HIGHESTMODSEQ, ENABLE CONDSTORE.
    let r = c.cmd("A3 STATUS INBOX (HIGHESTMODSEQ)").await;
    assert!(r.contains(&format!("HIGHESTMODSEQ {}", base + 1)), "{r}");
    let mut c2 = env.logged_in().await;
    assert_eq!(
        c2.cmd("B1 ENABLE CONDSTORE").await,
        "* ENABLED CONDSTORE\r\nB1 OK ENABLE completed\r\n"
    );
    assert!(c2.cmd("B2 SELECT INBOX").await.contains("[HIGHESTMODSEQ "));
}
