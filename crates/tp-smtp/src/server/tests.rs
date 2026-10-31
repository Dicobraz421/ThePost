//! Сценарии сессии по RFC 5321 (и расширениям PIPELINING, SIZE, CHUNKING, SMTPUTF8) над `tokio::io::duplex`.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::task::JoinHandle;
use tp_platform::ConnInfo;

use super::*;
use crate::config::SmtpConfig;

/// Обработчик, запоминающий письма; получатели из `reject` отклоняются.
#[derive(Default)]
pub(crate) struct Recorder {
    pub(crate) mails: Mutex<Vec<(Envelope, Bytes)>>,
    pub(crate) reject: Mutex<Vec<String>>,
}

impl Recorder {
    pub(crate) fn mails(&self) -> Vec<(Envelope, Bytes)> {
        self.mails.lock().expect("lock").clone()
    }
}

#[async_trait]
impl Handler for Recorder {
    async fn rcpt(&self, _env: &Envelope, rcpt: &str) -> Reply {
        if self.reject.lock().expect("lock").iter().any(|r| r == rcpt) {
            reply(550, [5, 1, 1], "No such user")
        } else {
            reply(250, [2, 1, 5], "OK")
        }
    }

    async fn data(&self, env: &Envelope, raw: Bytes) -> Reply {
        self.mails.lock().expect("lock").push((env.clone(), raw));
        reply(250, [2, 0, 0], "Queued")
    }
}

pub(crate) fn conn_info(tls: bool) -> ConnInfo {
    ConnInfo {
        client: "192.0.2.10:40000".parse().expect("addr"),
        local: "198.51.100.1:25".parse().expect("addr"),
        proxy: None,
        tls,
    }
}

/// Клиент сессии: команды и чтение ответов (многострочных — до строки `NNN `).
pub(crate) struct Client<S> {
    pub(crate) io: S,
    buf: Vec<u8>,
    last: String,
}

impl<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin> Client<S> {
    pub(crate) fn new(io: S) -> Self {
        Self {
            io,
            buf: Vec::new(),
            last: String::new(),
        }
    }

    pub(crate) async fn send(&mut self, data: &str) {
        self.last = data.chars().take(80).collect();
        self.io.write_all(data.as_bytes()).await.expect("write");
    }

    /// Следующий ответ: код и строки текста.
    pub(crate) async fn reply(&mut self) -> (u16, Vec<String>) {
        let mut lines = Vec::new();
        loop {
            let line = self.line().await;
            let code: u16 = line.get(..3).and_then(|c| c.parse().ok()).expect("code");
            let last = line.as_bytes().get(3) != Some(&b'-');
            lines.push(line.get(4..).unwrap_or_default().to_owned());
            if last {
                return (code, lines);
            }
        }
    }

    /// Код следующего ответа.
    pub(crate) async fn code(&mut self) -> u16 {
        self.reply().await.0
    }

    /// Команда и код ответа на неё.
    pub(crate) async fn cmd(&mut self, line: &str) -> u16 {
        self.send(&format!("{line}\r\n")).await;
        self.code().await
    }

    async fn line(&mut self) -> String {
        loop {
            if let Some(pos) = self.buf.windows(2).position(|w| w == b"\r\n") {
                let line: Vec<u8> = self.buf.drain(..pos + 2).collect();
                return String::from_utf8_lossy(&line[..pos]).into_owned();
            }
            let mut chunk = [0u8; 4096];
            let n = tokio::time::timeout(Duration::from_secs(10), self.io.read(&mut chunk))
                .await
                .unwrap_or_else(|_| panic!("no reply after {:?}", self.last))
                .expect("read");
            assert!(n > 0, "server closed connection");
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// Сервер закрыл соединение, не прислав больше данных.
    pub(crate) async fn closed(&mut self) -> bool {
        let mut chunk = [0u8; 64];
        self.buf.is_empty() && matches!(self.io.read(&mut chunk).await, Ok(0) | Err(_))
    }
}

fn config(edit: impl FnOnce(&mut SmtpConfig)) -> Arc<SmtpConfig> {
    let mut cfg = SmtpConfig {
        hostname: "mx.example.org".into(),
        ..SmtpConfig::default()
    };
    edit(&mut cfg);
    Arc::new(cfg)
}

struct Env {
    client: Client<DuplexStream>,
    rec: Arc<Recorder>,
    task: JoinHandle<std::io::Result<Exit>>,
}

async fn start(mode: Mode, cfg: Arc<SmtpConfig>) -> Env {
    let rec = Arc::new(Recorder::default());
    let ctx = SessionCtx {
        mode,
        config: cfg,
        handler: rec.clone(),
        sasl: None,
        limiter: None,
    };
    let (client, mut server) = tokio::io::duplex(64 * 1024);
    let task = tokio::spawn(async move {
        let mut s = Session::new(ctx, conn_info(false), false);
        s.serve(&mut server, true).await
    });
    let mut client = Client::new(client);
    let (code, text) = client.reply().await;
    assert_eq!(code, 220);
    assert!(text[0].starts_with("mx.example.org "), "{text:?}");
    Env { client, rec, task }
}

async fn inbound() -> Env {
    start(Mode::Inbound, config(|_| {})).await
}

#[tokio::test]
async fn ehlo_advertises_extensions() {
    let mut env = inbound().await;
    env.client.send("EHLO client.example.net\r\n").await;
    let (code, lines) = env.client.reply().await;
    assert_eq!(code, 250);
    assert!(lines[0].starts_with("mx.example.org"));
    for ext in [
        "PIPELINING",
        "SIZE 52428800",
        "8BITMIME",
        "SMTPUTF8",
        "ENHANCEDSTATUSCODES",
        "CHUNKING",
    ] {
        assert!(lines.iter().any(|l| l == ext), "{ext} in {lines:?}");
    }
    // Без сертификатов STARTTLS не предлагается, без TLS — AUTH.
    assert!(
        !lines
            .iter()
            .any(|l| l == "STARTTLS" || l.starts_with("AUTH"))
    );
    assert_eq!(env.client.cmd("HELO client.example.net").await, 250);
    assert_eq!(env.client.cmd("QUIT").await, 221);
    assert_eq!(env.task.await.expect("join").expect("io"), Exit::Quit);
}

#[tokio::test]
async fn transaction_with_dot_stuffing() {
    let mut env = inbound().await;
    let c = &mut env.client;
    assert_eq!(c.cmd("EHLO client.example.net").await, 250);
    assert_eq!(
        c.cmd("MAIL FROM:<alice@example.net> BODY=8BITMIME").await,
        250
    );
    assert_eq!(c.cmd("RCPT TO:<bob@example.org>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<carol@example.org>").await, 250);
    // Повтор получателя принимается, но не дублируется.
    assert_eq!(c.cmd("RCPT TO:<BOB@example.org>").await, 250);
    assert_eq!(c.cmd("DATA").await, 354);
    c.send("Subject: hi\r\n\r\n..leading dot\r\nline\r\n.\r\n")
        .await;
    assert_eq!(c.code().await, 250);
    let mails = env.rec.mails();
    assert_eq!(mails.len(), 1);
    let (e, raw) = &mails[0];
    assert_eq!(e.mail_from, "alice@example.net");
    assert_eq!(e.rcpts, ["bob@example.org", "carol@example.org"]);
    assert_eq!(e.helo, "client.example.net");
    assert!(e.esmtp && !e.smtputf8 && e.principal.is_none());
    assert_eq!(&raw[..], b"Subject: hi\r\n\r\n.leading dot\r\nline\r\n");
    // Транзакция закрыта: следующая начинается с MAIL, пустой отправитель допустим.
    assert_eq!(c.cmd("RCPT TO:<bob@example.org>").await, 503);
    assert_eq!(c.cmd("MAIL FROM:<>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<bob@example.org>").await, 250);
    assert_eq!(c.cmd("DATA").await, 354);
    c.send("\r\n.\r\n").await;
    assert_eq!(c.code().await, 250);
    assert_eq!(env.rec.mails()[1].0.mail_from, "");
}

#[tokio::test]
async fn command_sequence_errors() {
    let mut env = start(Mode::Inbound, config(|c| c.limits.max_errors = 100)).await;
    let c = &mut env.client;
    assert_eq!(c.cmd("MAIL FROM:<a@example.net>").await, 503);
    assert_eq!(c.cmd("EHLO client.example.net").await, 250);
    assert_eq!(c.cmd("RCPT TO:<b@example.org>").await, 503);
    assert_eq!(c.cmd("DATA").await, 503);
    assert_eq!(c.cmd("MAIL FROM:<a@example.net>").await, 250);
    assert_eq!(c.cmd("MAIL FROM:<a@example.net>").await, 503);
    assert_eq!(c.cmd("DATA").await, 554);
    assert_eq!(c.cmd("RCPT TO:<b@example.org>").await, 250);
    assert_eq!(c.cmd("RSET").await, 250);
    assert_eq!(c.cmd("DATA").await, 503);
    assert_eq!(c.cmd("FOO bar").await, 500);
    assert_eq!(c.cmd("MAIL FROM:bad").await, 501);
    assert_eq!(c.cmd("NOOP").await, 250);
    assert_eq!(c.cmd("VRFY bob").await, 252);
    assert_eq!(c.cmd("EXPN list").await, 502);
    assert_eq!(c.cmd("STARTTLS").await, 454);
    assert_eq!(c.cmd("AUTH PLAIN AGJvYgBzZWNyZXQ=").await, 502);
    assert!(env.rec.mails().is_empty());
}

#[tokio::test]
async fn pipelined_commands_get_replies_in_order() {
    let mut env = inbound().await;
    env.rec
        .reject
        .lock()
        .expect("lock")
        .push("nobody@example.org".into());
    let c = &mut env.client;
    c.send(concat!(
        "EHLO client.example.net\r\n",
        "MAIL FROM:<a@example.net>\r\n",
        "RCPT TO:<b@example.org>\r\n",
        "RCPT TO:<nobody@example.org>\r\n",
        "RCPT TO:<c@example.org>\r\n",
        "DATA\r\n",
    ))
    .await;
    let mut codes = Vec::new();
    for _ in 0..6 {
        codes.push(c.code().await);
    }
    assert_eq!(codes, [250, 250, 250, 550, 250, 354]);
    // Данные и следующая команда в одном пакете.
    c.send("Subject: x\r\n\r\nbody\r\n.\r\nQUIT\r\n").await;
    assert_eq!(c.code().await, 250);
    assert_eq!(c.code().await, 221);
    let mails = env.rec.mails();
    assert_eq!(mails[0].0.rcpts, ["b@example.org", "c@example.org"]);
}

#[tokio::test]
async fn bdat_chunks() {
    let mut env = inbound().await;
    let c = &mut env.client;
    assert_eq!(c.cmd("EHLO client.example.net").await, 250);
    // BDAT без транзакции: блок читается и отбрасывается.
    c.send("BDAT 5\r\nhello").await;
    assert_eq!(c.code().await, 503);
    assert_eq!(c.cmd("MAIL FROM:<a@example.net>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<b@example.org>").await, 250);
    // Блоки конвейером: точки в BDAT не экранируются.
    c.send("BDAT 15\r\nSubject: x\r\n\r\n.BDAT 6 LAST\r\nbody\r\n")
        .await;
    let (code, text) = c.reply().await;
    assert_eq!((code, text[0].as_str()), (250, "2.0.0 15 octets received"));
    assert_eq!(c.code().await, 250);
    assert_eq!(&env.rec.mails()[0].1[..], b"Subject: x\r\n\r\n.body\r\n");
    // BDAT 0 LAST завершает письмо; DATA после BDAT запрещён.
    assert_eq!(c.cmd("MAIL FROM:<a@example.net>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<b@example.org>").await, 250);
    c.send("BDAT 3\r\nabc").await;
    assert_eq!(c.code().await, 250);
    assert_eq!(c.cmd("DATA").await, 503);
    assert_eq!(c.cmd("BDAT 0 LAST").await, 250);
    assert_eq!(&env.rec.mails()[1].1[..], b"abc");
}

#[tokio::test]
async fn size_limits() {
    let cfg = config(|c| c.limits.max_message_size = 100);
    let mut env = start(Mode::Inbound, cfg).await;
    let c = &mut env.client;
    c.send("EHLO client.example.net\r\n").await;
    assert!(c.reply().await.1.iter().any(|l| l == "SIZE 100"));
    assert_eq!(c.cmd("MAIL FROM:<a@example.net> SIZE=101").await, 552);
    assert_eq!(c.cmd("MAIL FROM:<a@example.net> SIZE=100").await, 250);
    assert_eq!(c.cmd("RCPT TO:<b@example.org>").await, 250);
    assert_eq!(c.cmd("DATA").await, 354);
    // Письмо больше лимита дочитывается до конца, затем — отказ; сессия продолжается.
    c.send(&format!("{}\r\n.\r\n", "x".repeat(300))).await;
    assert_eq!(c.code().await, 552);
    assert_eq!(c.cmd("RCPT TO:<b@example.org>").await, 503);
    assert_eq!(c.cmd("MAIL FROM:<a@example.net>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<b@example.org>").await, 250);
    c.send(&format!("BDAT 101 LAST\r\n{}", "y".repeat(101)))
        .await;
    assert_eq!(c.code().await, 552);
    assert_eq!(c.cmd("NOOP").await, 250);
    assert!(env.rec.mails().is_empty());
}

#[tokio::test]
async fn too_many_recipients() {
    let cfg = config(|c| c.limits.max_recipients = 2);
    let mut env = start(Mode::Inbound, cfg).await;
    let c = &mut env.client;
    assert_eq!(c.cmd("EHLO client.example.net").await, 250);
    assert_eq!(c.cmd("MAIL FROM:<a@example.net>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<b1@example.org>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<b2@example.org>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<b3@example.org>").await, 452);
}

#[tokio::test]
async fn only_rejected_recipients_fail_data() {
    let mut env = inbound().await;
    env.rec
        .reject
        .lock()
        .expect("lock")
        .push("nobody@example.org".into());
    let c = &mut env.client;
    assert_eq!(c.cmd("EHLO client.example.net").await, 250);
    assert_eq!(c.cmd("MAIL FROM:<a@example.net>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<nobody@example.org>").await, 550);
    assert_eq!(c.cmd("DATA").await, 554);
}

#[tokio::test]
async fn smtputf8() {
    let mut env = inbound().await;
    let c = &mut env.client;
    assert_eq!(c.cmd("EHLO client.example.net").await, 250);
    assert_eq!(c.cmd("MAIL FROM:<отправитель@пример.рф>").await, 553);
    assert_eq!(c.cmd("MAIL FROM:<a@example.net>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<получатель@example.org>").await, 553);
    assert_eq!(c.cmd("RSET").await, 250);
    assert_eq!(
        c.cmd("MAIL FROM:<отправитель@пример.рф> SMTPUTF8").await,
        250
    );
    assert_eq!(c.cmd("RCPT TO:<получатель@example.org>").await, 250);
    assert_eq!(c.cmd("DATA").await, 354);
    c.send("Subject: привет\r\n\r\n.\r\n").await;
    assert_eq!(c.code().await, 250);
    let (e, _) = &env.rec.mails()[0];
    assert!(e.smtputf8);
    assert_eq!(e.mail_from, "отправитель@пример.рф");
}

#[tokio::test]
async fn too_many_errors_close_connection() {
    let cfg = config(|c| c.limits.max_errors = 3);
    let mut env = start(Mode::Inbound, cfg).await;
    let c = &mut env.client;
    assert_eq!(c.cmd("FOO").await, 500);
    assert_eq!(c.cmd("BAR").await, 500);
    assert_eq!(c.cmd("BAZ").await, 500);
    assert_eq!(c.code().await, 421);
    assert!(c.closed().await);
    assert_eq!(env.task.await.expect("join").expect("io"), Exit::Closed);
}

#[tokio::test]
async fn idle_timeout() {
    let cfg = config(|c| c.timeouts.command = Duration::from_millis(50));
    let mut env = start(Mode::Inbound, cfg).await;
    assert_eq!(env.client.code().await, 421);
    assert_eq!(env.task.await.expect("join").expect("io"), Exit::Closed);
}

#[tokio::test]
async fn submission_requires_authentication() {
    let mut env = start(Mode::Submission, config(|_| {})).await;
    let c = &mut env.client;
    assert_eq!(c.cmd("EHLO client.example.net").await, 250);
    assert_eq!(c.cmd("MAIL FROM:<a@example.org>").await, 530);
}
