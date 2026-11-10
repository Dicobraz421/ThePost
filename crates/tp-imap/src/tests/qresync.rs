//! QRESYNC (IMAP-09): примеры RFC 7162, 3.2.

use tp_contracts::ModSeq;
use tp_contracts::meta::SpecialUse;

use super::{Client, Env};

const PLAIN: &[u8] = b"From: a@example.org\r\nTo: b@example.org\r\nSubject: hi\r\n\r\nHello\r\n";

/// Число после `key` в ответе (`UIDVALIDITY 5`, `HIGHESTMODSEQ 7`).
fn number(r: &str, key: &str) -> u64 {
    let start = r.find(key).expect(key) + key.len();
    let digits: String = r[start..]
        .chars()
        .skip_while(|c| *c == ' ')
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().expect("number")
}

/// Пять писем; первая сессия запомнила UIDVALIDITY и HIGHESTMODSEQ, затем другая сессия пометила
/// письмо 2 и удалила письмо 4. Возвращает `(env, uidvalidity, modseq)`.
async fn changed() -> (Env, u64, u64) {
    let env = Env::new().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    for _ in 0..5 {
        env.deliver(inbox, PLAIN, Default::default()).await;
    }
    let mut a = env.logged_in().await;
    let r = a.cmd("A1 SELECT INBOX (CONDSTORE)").await;
    let (uv, ms) = (number(&r, "UIDVALIDITY"), number(&r, "HIGHESTMODSEQ"));
    assert!(
        a.cmd("A2 STORE 2 +FLAGS (\\Flagged)")
            .await
            .contains("A2 OK")
    );
    assert!(
        a.cmd("A3 STORE 4 +FLAGS (\\Deleted)")
            .await
            .contains("A3 OK")
    );
    assert!(a.cmd("A4 EXPUNGE").await.contains("A4 OK"));
    (env, uv, ms)
}

async fn qresync_client(env: &Env) -> Client {
    let mut c = env.logged_in().await;
    assert_eq!(
        c.cmd("E1 ENABLE QRESYNC").await,
        "* ENABLED QRESYNC\r\nE1 OK ENABLE completed\r\n"
    );
    c
}

#[tokio::test]
async fn capability_and_enable_required() {
    let env = Env::new().await;
    let mut c = env.logged_in().await;
    assert!(c.cmd("C1 CAPABILITY").await.contains(" QRESYNC"));
    // RFC 7162, 3.2.5: QRESYNC без ENABLE — BAD.
    assert!(
        c.cmd("C2 SELECT INBOX (QRESYNC (1 1))")
            .await
            .contains("C2 BAD")
    );
    assert!(c.cmd("C3 SELECT INBOX").await.contains("C3 OK"));
    // VANISHED без QRESYNC — BAD.
    assert!(
        c.cmd("C4 UID FETCH 1:* FLAGS (CHANGEDSINCE 1 VANISHED)")
            .await
            .contains("C4 BAD")
    );
}

#[tokio::test]
async fn select_reports_vanished_and_changed() {
    let (env, uv, ms) = changed().await;
    let mut c = qresync_client(&env).await;
    let r = c
        .cmd(&format!("S1 SELECT INBOX (QRESYNC ({uv} {ms}))"))
        .await;
    assert!(r.contains("* 4 EXISTS\r\n"), "{r}");
    assert!(r.contains("[HIGHESTMODSEQ "), "{r}");
    assert!(r.contains("* VANISHED (EARLIER) 4\r\n"), "{r}");
    // Изменённое письмо: UID, флаги, MODSEQ; неизменённые не сообщаются.
    assert!(
        r.contains("* 2 FETCH (UID 2 FLAGS (\\Flagged) MODSEQ ("),
        "{r}"
    );
    assert_eq!(r.matches(" FETCH ").count(), 1, "{r}");
    // VANISHED и FETCH — до завершающего OK.
    let vanished = r.find("VANISHED").expect("vanished");
    let ok = r.find("S1 OK").expect("ok");
    assert!(vanished < ok);
    assert!(r.contains("S1 OK [READ-WRITE]"), "{r}");
}

#[tokio::test]
async fn select_known_uids_and_uidvalidity() {
    let (env, uv, ms) = changed().await;
    let mut c = qresync_client(&env).await;
    // Удалённое письмо вне known-uids не сообщается.
    let r = c
        .cmd(&format!("S1 EXAMINE INBOX (QRESYNC ({uv} {ms} 1:3))"))
        .await;
    assert!(!r.contains("VANISHED"), "{r}");
    assert!(r.contains("* 2 FETCH (UID 2"), "{r}");
    // Повторный выбор — сначала CLOSED.
    let r = c
        .cmd(&format!(
            "S2 EXAMINE INBOX (QRESYNC ({uv} {ms} 1:5 (1:5 1:5)))"
        ))
        .await;
    assert!(r.starts_with("* OK [CLOSED]"), "{r}");
    assert!(r.contains("* VANISHED (EARLIER) 4\r\n"), "{r}");
    // Другая UIDVALIDITY: кэш клиента недействителен — ни VANISHED, ни FETCH.
    let r = c
        .cmd(&format!("S3 SELECT INBOX (QRESYNC ({} {ms}))", uv + 1))
        .await;
    assert!(!r.contains("VANISHED") && !r.contains("FETCH"), "{r}");
    assert!(r.contains("S3 OK"), "{r}");
}

#[tokio::test]
async fn uid_fetch_vanished() {
    let (env, _uv, ms) = changed().await;
    let mut c = qresync_client(&env).await;
    assert!(c.cmd("S1 SELECT INBOX").await.contains("S1 OK"));
    let r = c
        .cmd(&format!(
            "F1 UID FETCH 1:* FLAGS (CHANGEDSINCE {ms} VANISHED)"
        ))
        .await;
    assert!(r.starts_with("* VANISHED (EARLIER) 4\r\n"), "{r}");
    assert!(
        r.contains("* 2 FETCH (UID 2 FLAGS (\\Flagged) MODSEQ (")
            || r.contains("* 2 FETCH (FLAGS (\\Flagged) UID 2 MODSEQ ("),
        "{r}"
    );
    assert!(r.contains("F1 OK"), "{r}");
    // UID вне набора не сообщается.
    let r = c
        .cmd(&format!(
            "F2 UID FETCH 1:3 FLAGS (CHANGEDSINCE {ms} VANISHED)"
        ))
        .await;
    assert!(!r.contains("VANISHED"), "{r}");
    // Без UID или без CHANGEDSINCE — BAD.
    assert!(
        c.cmd(&format!("F3 FETCH 1:* FLAGS (CHANGEDSINCE {ms} VANISHED)"))
            .await
            .contains("F3 BAD")
    );
    assert!(
        c.cmd("F4 UID FETCH 1:* FLAGS (VANISHED)")
            .await
            .contains("F4 BAD")
    );
}

#[tokio::test]
async fn expunge_sends_vanished() {
    let env = Env::new().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    for _ in 0..3 {
        env.deliver(inbox, PLAIN, Default::default()).await;
    }
    let mut c = qresync_client(&env).await;
    assert!(c.cmd("S1 SELECT INBOX").await.contains("S1 OK"));
    assert!(
        c.cmd("S2 STORE 2:3 +FLAGS.SILENT (\\Deleted)")
            .await
            .contains("S2 OK")
    );
    let r = c.cmd("S3 EXPUNGE").await;
    // RFC 7162, 3.2.10: VANISHED вместо EXPUNGE.
    assert!(r.starts_with("* VANISHED 2:3\r\n"), "{r}");
    assert!(!r.contains("EXPUNGE\r\n"), "{r}");
    // Изменения другой сессии — тоже VANISHED.
    let mut other = env.logged_in().await;
    assert!(other.cmd("O1 SELECT INBOX").await.contains("O1 OK"));
    other.cmd("O2 STORE 1 +FLAGS.SILENT (\\Deleted)").await;
    other.cmd("O3 EXPUNGE").await;
    let r = c.cmd("S4 NOOP").await;
    assert!(r.starts_with("* VANISHED 1\r\n"), "{r}");
}

#[tokio::test]
async fn old_modseq_reports_missing_uids() {
    let (env, uv, ms) = changed().await;
    // Журнал до текущего состояния стёрт: удалённые вычисляются по списку писем.
    env.meta
        .truncate_changelog(env.scope(), ModSeq(u64::MAX))
        .expect("truncate");
    let mut c = qresync_client(&env).await;
    let r = c
        .cmd(&format!("S1 SELECT INBOX (QRESYNC ({uv} {ms}))"))
        .await;
    assert!(r.contains("* VANISHED (EARLIER) 4\r\n"), "{r}");
    let r = c
        .cmd(&format!("S2 SELECT INBOX (QRESYNC ({uv} {ms} 1:9))"))
        .await;
    assert!(r.contains("* VANISHED (EARLIER) 4,6:9\r\n"), "{r}");
}
