//! POP3 (IMAP-12): сеанс RFC 1939 — USER/PASS, AUTH (RFC 5034), STAT, LIST, UIDL, RETR, TOP, DELE, RSET, QUIT.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tp_contracts::meta::{MessageQuery, MessageStore, SpecialUse};

use super::{Client, Env, LOGIN, PASSWORD, TestConn};
use crate::config::Pop3Config;
use crate::pop3::Pop3Session;

const ONE: &[u8] = b"From: a@example.org\r\nSubject: one\r\n\r\n.hidden\r\nline2\r\nline3\r\n";
const TWO: &[u8] = b"From: a@example.org\r\nSubject: two\r\n\r\nHello\r\n";

async fn pop(env: &Env, tls: bool) -> Client {
    let (server, client) = tokio::io::duplex(1 << 20);
    let session = Pop3Session::new(env.ctx(), TestConn { io: server, tls });
    let task = tokio::spawn(session.run());
    let mut c = Client {
        io: tokio::io::BufReader::new(client),
        task,
    };
    assert_eq!(c.line().await, "+OK The Post POP3 ready\r\n");
    c
}

impl Client {
    async fn pop(&mut self, line: &str) -> String {
        self.send(&format!("{line}\r\n")).await;
        self.line().await
    }

    /// Многострочный ответ до `.` (без первой строки).
    async fn multi(&mut self) -> String {
        let mut out = String::new();
        loop {
            let l = self.line().await;
            assert!(!l.is_empty(), "closed: {out}");
            if l == ".\r\n" {
                return out;
            }
            out.push_str(&l);
        }
    }
}

async fn logged_in(env: &Env) -> Client {
    let mut c = pop(env, true).await;
    assert!(c.pop(&format!("USER {LOGIN}")).await.starts_with("+OK"));
    assert_eq!(
        c.pop(&format!("PASS {PASSWORD}")).await,
        "+OK Logged in, 2 messages\r\n"
    );
    c
}

async fn two() -> Env {
    let env = Env::new().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    env.deliver(inbox, ONE, Default::default()).await;
    env.deliver(inbox, TWO, Default::default()).await;
    env
}

#[tokio::test]
async fn disabled_by_default() {
    assert!(!Pop3Config::default().enabled);
    assert!(!crate::ImapConfig::default().pop3.enabled);
}

#[tokio::test]
async fn capa_and_plaintext_requires_tls() {
    let env = two().await;
    let mut c = pop(&env, false).await;
    assert!(c.pop("CAPA").await.starts_with("+OK"));
    let caps = c.multi().await;
    assert!(
        caps.contains("UIDL\r\n") && caps.contains("TOP\r\n"),
        "{caps}"
    );
    // Без TLS — ни USER, ни механизмов с открытым паролем.
    assert!(!caps.contains("USER") && !caps.contains("PLAIN"), "{caps}");
    assert!(c.pop(&format!("USER {LOGIN}")).await.starts_with("-ERR"));
    assert!(c.pop("STAT").await.starts_with("-ERR"));
    assert!(c.pop("STLS").await.starts_with("-ERR"));
    assert_eq!(c.pop("QUIT").await, "+OK Bye\r\n");

    let mut c = pop(&env, true).await;
    c.pop("CAPA").await;
    let caps = c.multi().await;
    assert!(
        caps.contains("USER\r\n") && caps.contains("SASL PLAIN"),
        "{caps}"
    );
}

#[tokio::test]
async fn session_retr_top_dele_quit() {
    let env = two().await;
    let mut c = logged_in(&env).await;
    let stat = c.pop("STAT").await;
    assert!(stat.starts_with("+OK 2 "), "{stat}");
    assert!(c.pop("LIST").await.starts_with("+OK"));
    let list = c.multi().await;
    let sizes: Vec<u64> = list
        .lines()
        .map(|l| l.split(' ').nth(1).expect("size").parse().expect("number"))
        .collect();
    assert_eq!(sizes.len(), 2);
    assert_eq!(c.pop("LIST 2").await, format!("+OK 2 {}\r\n", sizes[1]));
    // RETR: точка в начале строки удвоена, размер — без удвоения.
    assert_eq!(
        c.pop("RETR 1").await,
        format!("+OK {} octets\r\n", sizes[0])
    );
    let body = c.multi().await;
    assert!(body.contains("\r\n\r\n..hidden\r\nline2\r\n"), "{body}");
    assert_eq!(body.len() as u64 - 1, sizes[0], "{body}");
    // TOP: заголовок и одна строка тела; TOP 0 — только заголовок.
    assert!(c.pop("TOP 1 1").await.starts_with("+OK"));
    let top = c.multi().await;
    assert!(top.ends_with("\r\n\r\n..hidden\r\n"), "{top}");
    assert!(c.pop("TOP 2 0").await.starts_with("+OK"));
    let top = c.multi().await;
    assert!(
        top.contains("Subject: two") && !top.contains("Hello"),
        "{top}"
    );
    // UIDL не меняется между сессиями.
    assert!(c.pop("UIDL").await.starts_with("+OK"));
    let uidl = c.multi().await;
    let uid2 = c.pop("UIDL 2").await;
    assert!(
        uidl.contains(uid2.trim_start_matches("+OK ")),
        "{uidl} {uid2}"
    );
    // DELE, RSET, снова DELE.
    assert!(c.pop("DELE 1").await.starts_with("+OK"));
    assert!(c.pop("DELE 1").await.starts_with("-ERR"));
    assert!(c.pop("RETR 1").await.starts_with("-ERR"));
    assert!(c.pop("STAT").await.starts_with("+OK 1 "));
    assert_eq!(c.pop("RSET").await, "+OK 2 messages\r\n");
    assert!(c.pop("STAT").await.starts_with("+OK 2 "));
    assert!(c.pop("DELE 2").await.starts_with("+OK"));
    assert!(c.pop("RETR 9").await.starts_with("-ERR"));
    assert!(c.pop("NOOP").await.starts_with("+OK"));
    assert_eq!(c.pop("QUIT").await, "+OK Bye\r\n");
    assert_eq!(c.rest().await, "");
    // Удалённое при QUIT — в корзине.
    let inbox = env.mailbox(SpecialUse::Inbox);
    let trash = env.mailbox(SpecialUse::Trash);
    let left = env
        .meta
        .list(env.scope(), inbox, MessageQuery::All, 10)
        .await
        .expect("list");
    assert_eq!(left.len(), 1);
    let trashed = env
        .meta
        .list(env.scope(), trash, MessageQuery::All, 10)
        .await
        .expect("list");
    assert_eq!(trashed.len(), 1);
    // Новая сессия: тот же UIDL у оставшегося письма.
    let mut c = pop(&env, true).await;
    c.pop(&format!("USER {LOGIN}")).await;
    c.pop(&format!("PASS {PASSWORD}")).await;
    // Осталось письмо 1 (письмо 2 удалено после RSET).
    let id1 = uidl
        .lines()
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .expect("uidl");
    assert_eq!(c.pop("UIDL 1").await, format!("+OK 1 {id1}\r\n"));
}

#[tokio::test]
async fn disconnect_without_quit_keeps_messages() {
    let env = two().await;
    let mut c = logged_in(&env).await;
    assert!(c.pop("DELE 1").await.starts_with("+OK"));
    drop(c);
    let inbox = env.mailbox(SpecialUse::Inbox);
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let left = env
        .meta
        .list(env.scope(), inbox, MessageQuery::All, 10)
        .await
        .expect("list");
    assert_eq!(left.len(), 2);
}

#[tokio::test]
async fn sasl_auth() {
    let env = two().await;
    let creds = STANDARD.encode(format!("\0{LOGIN}\0{PASSWORD}"));
    // Начальный ответ в команде.
    let mut c = pop(&env, true).await;
    assert_eq!(
        c.pop(&format!("AUTH PLAIN {creds}")).await,
        "+OK Logged in, 2 messages\r\n"
    );
    assert_eq!(c.pop("QUIT").await, "+OK Bye\r\n");
    // Ответ на вызов `+ `; отмена `*`.
    let mut c = pop(&env, true).await;
    assert_eq!(c.pop("AUTH PLAIN").await, "+ \r\n");
    assert!(c.pop("*").await.starts_with("-ERR"));
    assert_eq!(c.pop("AUTH PLAIN").await, "+ \r\n");
    assert_eq!(c.pop(&creds).await, "+OK Logged in, 2 messages\r\n");
    // Без TLS PLAIN не предлагается.
    let mut c = pop(&env, false).await;
    assert!(
        c.pop(&format!("AUTH PLAIN {creds}"))
            .await
            .starts_with("-ERR")
    );
    // Список механизмов.
    let mut c = pop(&env, true).await;
    assert!(c.pop("AUTH").await.starts_with("+OK"));
    assert!(c.multi().await.contains("PLAIN\r\n"));
}

#[tokio::test]
async fn auth_failures_close_session() {
    let env = two().await;
    let mut c = pop(&env, true).await;
    for _ in 0..2 {
        c.pop(&format!("USER {LOGIN}")).await;
        assert_eq!(
            c.pop("PASS wrong").await,
            "-ERR [AUTH] Authentication failed\r\n"
        );
    }
    c.pop(&format!("USER {LOGIN}")).await;
    assert!(c.pop("PASS wrong").await.starts_with("-ERR [AUTH]"));
    assert_eq!(c.rest().await, "");
}

#[tokio::test]
async fn long_line_closes_session() {
    let env = two().await;
    let mut c = pop(&env, true).await;
    let long = "X".repeat(env.config.pop3.max_line + 10);
    assert_eq!(c.pop(&long).await, "-ERR Line too long\r\n");
    assert_eq!(c.rest().await, "");
}

/// Вход в уже открытой сессии; ответ на `PASS`.
async fn login(c: &mut Client) -> String {
    assert!(c.pop(&format!("USER {LOGIN}")).await.starts_with("+OK"));
    c.pop(&format!("PASS {PASSWORD}")).await
}

#[tokio::test]
async fn maildrop_locked_per_session() {
    let env = two().await;
    let mut first = logged_in(&env).await;
    // Вторая сессия той же учётной записи — `[IN-USE]` (RFC 2449, 8.1.1), остаётся в AUTHORIZATION.
    let mut second = pop(&env, true).await;
    assert_eq!(
        login(&mut second).await,
        "-ERR [IN-USE] Mailbox is locked by another POP3 session\r\n"
    );
    assert!(second.pop("STAT").await.starts_with("-ERR"));
    assert_eq!(first.pop("QUIT").await, "+OK Bye\r\n");
    assert_eq!(first.rest().await, "");
    // После QUIT ящик свободен.
    assert_eq!(login(&mut second).await, "+OK Logged in, 2 messages\r\n");
    // Обрыв соединения тоже снимает блокировку.
    let Client { io, task } = second;
    drop(io);
    task.await.expect("join").expect("session");
    let mut third = pop(&env, true).await;
    assert_eq!(login(&mut third).await, "+OK Logged in, 2 messages\r\n");
}

#[tokio::test]
async fn quit_delete_audited_by_policy() {
    use tp_contracts::audit::{AuditAction, AuditPolicy, MailboxAudit};
    let env = two().await;
    let mut c = logged_in(&env).await;
    assert!(c.pop("RETR 1").await.starts_with("+OK"));
    c.multi().await;
    assert!(c.pop("DELE 1").await.starts_with("+OK"));
    assert_eq!(c.pop("QUIT").await, "+OK Bye\r\n");
    // По умолчанию удаление владельцем не записывается, чтение — никогда.
    assert!(env.audit.entries().is_empty());
    env.audit
        .set_audit_policy(env.principal.tenant, None, Some(AuditPolicy::all()))
        .await
        .expect("policy");
    let mut c = pop(&env, true).await;
    c.pop(&format!("USER {LOGIN}")).await;
    assert!(c.pop(&format!("PASS {PASSWORD}")).await.starts_with("+OK"));
    assert!(c.pop("DELE 1").await.starts_with("+OK"));
    assert_eq!(c.pop("QUIT").await, "+OK Bye\r\n");
    let e = env.audit.entries();
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0].action, AuditAction::Delete);
    assert_eq!(e[0].object.mailbox, Some(env.mailbox(SpecialUse::Inbox)));
    assert_eq!(e[0].client.as_deref(), Some("pop3 192.0.2.7"));
}
