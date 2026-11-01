//! Сценарии сессии (IMAP-02): состояния, TLS, LOGIN, AUTHENTICATE, ENABLE, ID, LOGOUT, остановка с BYE.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tp_contracts::Error;
use tp_platform::{EndpointConfig, Listener, TlsCerts, TlsConfig};

use super::{Env, LOGIN, PASSWORD};
use crate::session::{Server, SessionCtx};

const CERT: &str = include_str!("../testdata/cert.pem");
const KEY: &str = include_str!("../testdata/key.pem");

fn b64(data: &[u8]) -> String {
    STANDARD.encode(data)
}

#[tokio::test]
async fn capabilities_depend_on_tls_and_state() {
    let env = Env::new().await;
    // Открытый канал: пароль не принимается, механизмы SASL не предлагаются.
    let mut c = env.client(false).await;
    let r = c.cmd("A1 CAPABILITY").await;
    assert!(
        r.contains("IMAP4rev2") && r.contains("LOGINDISABLED"),
        "{r}"
    );
    assert!(!r.contains("AUTH="), "{r}");
    let r = c.cmd("A2 LOGIN user secret").await;
    assert!(r.starts_with("A2 NO [PRIVACYREQUIRED]"), "{r}");
    let r = c.cmd("A3 AUTHENTICATE PLAIN").await;
    assert!(r.starts_with("A3 NO [PRIVACYREQUIRED]"), "{r}");

    // TLS: механизмы есть, LOGINDISABLED нет; после входа — без AUTH=.
    let mut c = env.client(true).await;
    let r = c.cmd("B1 CAPABILITY").await;
    assert!(
        r.contains("AUTH=PLAIN") && r.contains("AUTH=OAUTHBEARER"),
        "{r}"
    );
    assert!(
        !r.contains("LOGINDISABLED") && !r.contains("STARTTLS"),
        "{r}"
    );
    let r = c.cmd(&format!("B2 LOGIN {LOGIN} {PASSWORD}")).await;
    assert!(
        r.starts_with("B2 OK [CAPABILITY IMAP4REV1 IMAP4rev2"),
        "{r}"
    );
    assert!(!r.contains("AUTH="), "{r}");
    let r = c.cmd("B3 LOGIN again secret").await;
    assert!(r.starts_with("B3 BAD"), "{r}");
}

#[tokio::test]
async fn login_failures_and_literals() {
    let mut env = Env::new().await;
    env.config.limits.max_auth_failures = 2;
    let mut c = env.client(true).await;
    let r = c.cmd(&format!("A1 LOGIN {LOGIN} wrong")).await;
    assert_eq!(r, "A1 NO [AUTHENTICATIONFAILED] Authentication failed\r\n");
    // Пароль литералом (RFC 9051, 4.3): «+» на каждый синхронизирующий литерал.
    c.send("A2 LOGIN {16}\r\n").await;
    assert!(c.line().await.starts_with("+ "));
    c.send(&format!("{LOGIN} {{5}}\r\n")).await;
    assert!(c.line().await.starts_with("+ "));
    c.send("wrong\r\n").await;
    let r = c.until("A2").await;
    assert!(r.contains("A2 NO [AUTHENTICATIONFAILED]"), "{r}");
    assert_eq!(c.rest().await, "* BYE Too many authentication failures\r\n");

    // Временная недоступность каталога — не ошибка пароля.
    env.auth.set_failure(Some(Error::UNAVAILABLE));
    let mut c = env.client(true).await;
    let r = c.cmd(&format!("B1 LOGIN {LOGIN} {PASSWORD}")).await;
    assert!(r.starts_with("B1 NO [UNAVAILABLE]"), "{r}");
    env.auth.set_failure(None);
    // LITERAL+ без ожидания.
    let r = c
        .cmd(&format!("B2 LOGIN {{16+}}\r\n{LOGIN} {{6+}}\r\n{PASSWORD}"))
        .await;
    assert!(r.starts_with("B2 OK"), "{r}");
}

#[tokio::test]
async fn authenticate_plain() {
    let env = Env::new().await;
    let creds = b64(format!("\0{LOGIN}\0{PASSWORD}").as_bytes());
    // SASL-IR (RFC 4959).
    let mut c = env.client(true).await;
    let r = c.cmd(&format!("A1 AUTHENTICATE PLAIN {creds}")).await;
    assert!(r.starts_with("A1 OK [CAPABILITY "), "{r}");
    // Без начального ответа: пустой вызов «+ », затем данные.
    let mut c = env.client(true).await;
    c.send("B1 AUTHENTICATE PLAIN\r\n").await;
    assert_eq!(c.line().await, "+ \r\n");
    c.send(&format!("{creds}\r\n")).await;
    assert!(c.until("B1").await.starts_with("B1 OK"));
    // Отмена клиентом.
    let mut c = env.client(true).await;
    c.send("C1 AUTHENTICATE PLAIN\r\n").await;
    assert_eq!(c.line().await, "+ \r\n");
    c.send("*\r\n").await;
    assert!(c.until("C1").await.starts_with("C1 BAD"));
    // Неверный пароль, неизвестный механизм, битое сообщение.
    let bad = b64(format!("\0{LOGIN}\0nope").as_bytes());
    let r = c.cmd(&format!("C2 AUTHENTICATE PLAIN {bad}")).await;
    assert!(r.starts_with("C2 NO [AUTHENTICATIONFAILED]"), "{r}");
    let r = c.cmd("C3 AUTHENTICATE CRAM-MD5").await;
    assert!(r.starts_with("C3 NO [CANNOT]"), "{r}");
    let r = c
        .cmd(&format!("C4 AUTHENTICATE PLAIN {}", b64(b"garbage")))
        .await;
    assert!(r.starts_with("C4 BAD"), "{r}");
}

#[tokio::test]
async fn authenticate_oauthbearer() {
    let env = Env::new().await;
    let msg = |token: &str| b64(format!("n,a={LOGIN},\x01auth=Bearer {token}\x01\x01").as_bytes());
    let mut c = env.client(true).await;
    // RFC 7628, 3.2.3: при ошибке — вызов с JSON, клиент отвечает %x01, сервер — NO.
    c.send(&format!(
        "A1 AUTHENTICATE OAUTHBEARER {}\r\n",
        msg("bad-token")
    ))
    .await;
    let challenge = c.line().await;
    let json = challenge
        .strip_prefix("+ ")
        .and_then(|s| STANDARD.decode(s.trim_end()).ok())
        .expect("challenge");
    assert!(String::from_utf8_lossy(&json).contains("invalid_token"));
    c.send("AQ==\r\n").await;
    assert!(
        c.until("A1")
            .await
            .starts_with("A1 NO [AUTHENTICATIONFAILED]")
    );
    let r = c
        .cmd(&format!(
            "A2 AUTHENTICATE OAUTHBEARER {}",
            msg("good-token")
        ))
        .await;
    assert!(r.starts_with("A2 OK"), "{r}");
}

#[tokio::test]
async fn authenticated_commands() {
    let env = Env::new().await;
    let mut c = env.client(true).await;
    let r = c.cmd("A1 SELECT INBOX").await;
    assert!(r.starts_with("A1 BAD"), "{r}");
    let r = c.cmd("A2 ENABLE IMAP4rev2").await;
    assert!(r.starts_with("A2 BAD"), "{r}");
    let r = c.cmd("A3 ID (\"name\" \"test\")").await;
    assert!(r.contains("* ID (\"name\" \"The Post\" \"version\""), "{r}");
    let r = c.cmd("A4 NOOP").await;
    assert_eq!(r, "A4 OK NOOP completed\r\n");
    let r = c.cmd(&format!("A5 LOGIN {LOGIN} {PASSWORD}")).await;
    assert!(r.starts_with("A5 OK"), "{r}");
    let r = c.cmd("A6 ENABLE IMAP4rev2 CONDSTORE").await;
    assert!(
        r.starts_with("* ENABLED IMAP4rev2 CONDSTORE\r\nA6 OK"),
        "{r}"
    );
    let r = c.cmd("A7 LOGOUT").await;
    assert_eq!(r, "* BYE Logging out\r\nA7 OK LOGOUT completed\r\n");
    assert_eq!(c.rest().await, "");
    c.task.await.expect("join").expect("session");
}

#[tokio::test]
async fn bad_commands_and_timeouts() {
    let mut env = Env::new().await;
    env.config.limits.max_errors = 3;
    let mut c = env.client(true).await;
    assert!(c.cmd("A1 FOO").await.starts_with("A1 BAD"));
    // Конвейер: ответы в порядке команд.
    c.send("A2 BAR\r\nA3 NOOP\r\n").await;
    let r = c.until("A3").await;
    assert!(r.starts_with("A2 BAD") && r.contains("A3 OK"), "{r}");
    c.send("A4 BAZ\r\n").await;
    let r = c.rest().await;
    assert!(
        r.contains("A4 BAD") && r.contains("* BYE Too many invalid commands"),
        "{r}"
    );

    // Бездействие до входа.
    env.config.timeouts.login = Duration::from_millis(50);
    let mut c = env.client(true).await;
    assert!(c.rest().await.starts_with("* BYE Autologout"));

    // Синхронизирующий литерал больше лимита — отказ без данных, LITERAL+ — BYE.
    env.config.timeouts.login = Duration::from_secs(60);
    env.config.limits.max_message_size = 10;
    let mut c = env.client(true).await;
    let r = c.cmd("B1 LOGIN {100}").await;
    assert!(r.starts_with("B1 NO [TOOBIG]"), "{r}");
    assert!(c.cmd("B2 NOOP").await.starts_with("B2 OK"));
    c.send("B3 LOGIN {100+}\r\n").await;
    assert!(c.rest().await.starts_with("* BYE [TOOBIG]"));
}

#[tokio::test]
async fn shutdown_sends_bye() {
    let env = Env::new().await;
    let mut c = env.logged_in().await;
    env.shutdown.cancel();
    assert_eq!(
        c.rest().await,
        "* BYE [UNAVAILABLE] Server shutting down\r\n"
    );
    c.task.await.expect("join").expect("session");
}

fn temp_dir() -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let dir = std::env::temp_dir().join(format!("tp-imap-{}-{nanos}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    dir
}

async fn certs() -> Arc<TlsCerts> {
    let dir = temp_dir();
    let cfg = TlsConfig {
        cert_file: dir.join("cert.pem"),
        key_file: dir.join("key.pem"),
        ..TlsConfig::default()
    };
    std::fs::write(&cfg.cert_file, CERT).expect("cert");
    std::fs::write(&cfg.key_file, KEY).expect("key");
    let certs = TlsCerts::load(&cfg).await.expect("load");
    let _ = std::fs::remove_dir_all(dir);
    Arc::new(certs)
}

fn connector() -> TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut CERT.as_bytes()) {
        roots.add(cert.expect("pem")).expect("root");
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(cfg))
}

/// Сервер на 127.0.0.1 со STARTTLS; возвращает адрес и задачу `serve`.
async fn server(
    env: &Env,
) -> (
    SocketAddr,
    tokio::task::JoinHandle<tp_contracts::Result<()>>,
) {
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let at = tcp.local_addr().expect("addr");
    let listener =
        Listener::from_tcp(tcp, &EndpointConfig::new(at), Some(certs().await)).expect("listener");
    let ctx = SessionCtx::clone(&env.ctx());
    let server = Server::new(ctx);
    (
        at,
        tokio::spawn(async move { server.serve(listener).await }),
    )
}

async fn read_line<R: tokio::io::AsyncBufRead + Unpin>(r: &mut R) -> String {
    let mut s = String::new();
    r.read_line(&mut s).await.expect("read");
    s
}

#[tokio::test]
async fn starttls_discards_plaintext_and_upgrades() {
    let env = Env::new().await;
    let (at, serve) = server(&env).await;
    let mut tcp = BufReader::new(TcpStream::connect(at).await.expect("connect"));
    let greeting = read_line(&mut tcp).await;
    assert!(
        greeting.contains("STARTTLS") && greeting.contains("LOGINDISABLED"),
        "{greeting}"
    );
    // Команда, пришедшая открытым текстом вместе со STARTTLS, не исполняется после рукопожатия.
    tcp.get_mut()
        .write_all(format!("S1 STARTTLS\r\nX1 LOGIN {LOGIN} {PASSWORD}\r\n").as_bytes())
        .await
        .expect("write");
    assert_eq!(
        read_line(&mut tcp).await,
        "S1 OK Begin TLS negotiation now\r\n"
    );
    let name = ServerName::try_from("localhost").expect("name");
    let tls = connector()
        .connect(name, tcp.into_inner())
        .await
        .expect("handshake");
    let mut tls = BufReader::new(tls);
    tls.get_mut()
        .write_all(b"C1 CAPABILITY\r\n")
        .await
        .expect("write");
    let caps = read_line(&mut tls).await;
    assert!(caps.contains("AUTH=PLAIN"), "{caps}");
    assert!(
        !caps.contains("STARTTLS") && !caps.contains("LOGINDISABLED"),
        "{caps}"
    );
    assert_eq!(read_line(&mut tls).await, "C1 OK CAPABILITY completed\r\n");
    tls.get_mut()
        .write_all(format!("C2 LOGIN {LOGIN} {PASSWORD}\r\nC3 STARTTLS\r\n").as_bytes())
        .await
        .expect("write");
    assert!(read_line(&mut tls).await.starts_with("C2 OK"));
    assert!(read_line(&mut tls).await.starts_with("C3 BAD"));
    // Остановка узла: BYE и завершение serve.
    env.shutdown.cancel();
    assert_eq!(
        read_line(&mut tls).await,
        "* BYE [UNAVAILABLE] Server shutting down\r\n"
    );
    serve.await.expect("join").expect("serve");
}

#[tokio::test]
async fn connection_limit() {
    let mut env = Env::new().await;
    env.config.limits.max_connections = 1;
    let (at, serve) = server(&env).await;
    let mut first = BufReader::new(TcpStream::connect(at).await.expect("connect"));
    assert!(read_line(&mut first).await.starts_with("* OK"));
    let mut second = BufReader::new(TcpStream::connect(at).await.expect("connect"));
    assert_eq!(
        read_line(&mut second).await,
        "* BYE [UNAVAILABLE] Too many connections\r\n"
    );
    env.shutdown.cancel();
    assert!(read_line(&mut first).await.starts_with("* BYE"));
    serve.await.expect("join").expect("serve");
}
