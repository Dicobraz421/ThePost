//! Роль submission и TLS на настоящих сокетах `127.0.0.1`: STARTTLS (587), неявный TLS (465), AUTH только
//! после TLS, отправка после AUTH, ограничение попыток и частоты отправки.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
use tokio_rustls::rustls::{self, ClientConfig, RootCertStore};
use tp_contracts::TenantId;
use tp_contracts::role::{CancellationToken, ReadyHandle, RoleCtx, fixed_config};
use tp_platform::ratelimit::BucketConfig;
use tp_platform::{EndpointConfig, Listener, RateLimitConfig, RateLimiter, TlsCerts, TlsConfig};
use tp_testkit::{FakeAuthenticator, FakeSasl, principal};

use super::listen::{ServerDeps, SmtpService};
use super::tests::{Client, Recorder};
use super::*;
use crate::config::SmtpConfig;

fn testdata(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src/server/testdata")
        .join(name)
}

async fn certs() -> Arc<TlsCerts> {
    let cfg = TlsConfig {
        cert_file: testdata("cert.pem"),
        key_file: testdata("key.pem"),
        ..TlsConfig::default()
    };
    Arc::new(TlsCerts::load(&cfg).await.expect("certs"))
}

fn connector() -> TlsConnector {
    let mut roots = RootCertStore::empty();
    let pem = std::fs::read(testdata("cert.pem")).expect("cert");
    roots
        .add(CertificateDer::from_pem_slice(&pem).expect("pem"))
        .expect("root");
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(cfg))
}

async fn handshake(tcp: TcpStream) -> TlsStream<TcpStream> {
    let name = ServerName::try_from("localhost").expect("name");
    connector().connect(name, tcp).await.expect("tls")
}

/// Сервер роли с одним слушателем на `127.0.0.1:0`.
struct Server {
    addr: std::net::SocketAddr,
    rec: Arc<Recorder>,
    shutdown: CancellationToken,
    task: tokio::task::JoinHandle<tp_contracts::Result<()>>,
}

impl Server {
    async fn start(mode: Mode, implicit_tls: bool, edit: impl FnOnce(&mut SmtpConfig)) -> Self {
        Self::with_limiter(mode, implicit_tls, edit, None).await
    }

    async fn with_limiter(
        mode: Mode,
        implicit_tls: bool,
        edit: impl FnOnce(&mut SmtpConfig),
        limiter: Option<Arc<RateLimiter>>,
    ) -> Self {
        let tcp = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = tcp.local_addr().expect("addr");
        let mut ep = EndpointConfig::new(addr);
        ep.implicit_tls = implicit_tls;
        let tls = certs().await;
        let listener = Listener::from_tcp(tcp, &ep, Some(tls.clone())).expect("listener");
        let auth = FakeAuthenticator::default();
        auth.add_password(
            "alice@example.org",
            "secret",
            principal(TenantId::DEFAULT, "alice@example.org"),
        );
        let rec = Arc::new(Recorder::default());
        let svc = SmtpService::new(
            mode,
            ServerDeps {
                handler: rec.clone(),
                sasl: Some(Arc::new(FakeSasl::new(Arc::new(auth)))),
                limiter,
                tls: Some(tls),
            },
        );
        let mut cfg = SmtpConfig {
            hostname: "mx.example.org".into(),
            ..SmtpConfig::default()
        };
        edit(&mut cfg);
        let shutdown = CancellationToken::new();
        let ctx = RoleCtx {
            config: fixed_config(cfg),
            shutdown: shutdown.clone(),
            drain_deadline: Duration::from_secs(1),
            ready: ReadyHandle::default(),
        };
        let task = tokio::spawn(async move { svc.serve(vec![listener], ctx).await });
        Self {
            addr,
            rec,
            shutdown,
            task,
        }
    }

    async fn connect(&self) -> TcpStream {
        TcpStream::connect(self.addr).await.expect("connect")
    }

    async fn stop(self) {
        self.shutdown.cancel();
        self.task.await.expect("join").expect("serve");
    }
}

/// `AUTH PLAIN` с паролем `pass` для alice.
fn plain(pass: &str) -> String {
    use base64::Engine;
    let raw = format!("\0alice@example.org\0{pass}");
    format!(
        "AUTH PLAIN {}",
        base64::engine::general_purpose::STANDARD.encode(raw)
    )
}

async fn ehlo<S>(c: &mut Client<S>) -> Vec<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    c.send("EHLO client.example.net\r\n").await;
    let (code, lines) = c.reply().await;
    assert_eq!(code, 250);
    lines
}

/// Отправка письма alice → bob после AUTH.
async fn send_mail<S>(c: &mut Client<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    assert_eq!(c.cmd("MAIL FROM:<alice@example.org>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<bob@example.org>").await, 250);
    assert_eq!(c.cmd("DATA").await, 354);
    c.send("Subject: hi\r\n\r\nhello\r\n.\r\n").await;
    assert_eq!(c.code().await, 250);
}

#[tokio::test]
async fn submission_starttls_then_auth_then_send() {
    let srv = Server::start(Mode::Submission, false, |_| {}).await;
    let mut c = Client::new(srv.connect().await);
    assert_eq!(c.code().await, 220);
    let lines = ehlo(&mut c).await;
    // Без TLS: STARTTLS предлагается, AUTH — нет, AUTH отклоняется, отправка без AUTH — тоже.
    assert!(lines.iter().any(|l| l == "STARTTLS"), "{lines:?}");
    assert!(!lines.iter().any(|l| l.starts_with("AUTH")), "{lines:?}");
    c.send(&format!("{}\r\n", plain("secret"))).await;
    let (code, text) = c.reply().await;
    assert_eq!(
        (code, text[0].as_str()),
        (
            538,
            "5.7.11 Encryption required for requested authentication mechanism"
        )
    );
    assert_eq!(c.cmd("MAIL FROM:<alice@example.org>").await, 530);
    // Команды, присланные вместе со STARTTLS открытым текстом, после рукопожатия не исполняются.
    c.send("STARTTLS\r\nNOOP\r\n").await;
    assert_eq!(c.code().await, 220);
    let mut c = Client::new(handshake(c.io).await);
    // После TLS — новый EHLO: AUTH предлагается, STARTTLS — нет.
    assert_eq!(c.cmd("MAIL FROM:<alice@example.org>").await, 503);
    let lines = ehlo(&mut c).await;
    assert!(
        lines.iter().any(|l| l == "AUTH PLAIN OAUTHBEARER"),
        "{lines:?}"
    );
    assert!(!lines.iter().any(|l| l == "STARTTLS"));
    assert_eq!(c.cmd("MAIL FROM:<alice@example.org>").await, 530);
    assert_eq!(c.cmd(&plain("wrong")).await, 535);
    assert_eq!(c.cmd(&plain("secret")).await, 235);
    assert_eq!(c.cmd(&plain("secret")).await, 503);
    send_mail(&mut c).await;
    assert_eq!(c.cmd("QUIT").await, 221);
    let mails = srv.rec.mails();
    assert_eq!(mails.len(), 1);
    let (env, _) = &mails[0];
    assert!(env.conn.tls);
    assert_eq!(
        env.principal.as_ref().map(|p| p.login.as_str()),
        Some("alice@example.org")
    );
    srv.stop().await;
}

#[tokio::test]
async fn submissions_implicit_tls() {
    let srv = Server::start(Mode::Submission, true, |_| {}).await;
    let mut c = Client::new(handshake(srv.connect().await).await);
    assert_eq!(c.code().await, 220);
    let lines = ehlo(&mut c).await;
    assert!(lines.iter().any(|l| l.starts_with("AUTH ")));
    assert!(!lines.iter().any(|l| l == "STARTTLS"));
    // AUTH без начального ответа: вызов 334, ответ клиента; отмена `*`.
    assert_eq!(c.cmd("AUTH PLAIN").await, 334);
    assert_eq!(c.cmd("*").await, 501);
    assert_eq!(c.cmd("AUTH PLAIN").await, 334);
    use base64::Engine;
    let resp = base64::engine::general_purpose::STANDARD.encode("\0alice@example.org\0secret");
    assert_eq!(c.cmd(&resp).await, 235);
    send_mail(&mut c).await;
    assert_eq!(srv.rec.mails().len(), 1);
    srv.stop().await;
}

#[tokio::test]
async fn inbound_starttls_without_auth() {
    let srv = Server::start(Mode::Inbound, false, |_| {}).await;
    let mut c = Client::new(srv.connect().await);
    assert_eq!(c.code().await, 220);
    assert!(ehlo(&mut c).await.iter().any(|l| l == "STARTTLS"));
    assert_eq!(c.cmd("STARTTLS").await, 220);
    let mut c = Client::new(handshake(c.io).await);
    let lines = ehlo(&mut c).await;
    assert!(!lines.iter().any(|l| l.starts_with("AUTH")));
    assert_eq!(c.cmd(&plain("secret")).await, 502);
    assert_eq!(c.cmd("STARTTLS").await, 503);
    assert_eq!(c.cmd("MAIL FROM:<a@example.net>").await, 250);
    srv.stop().await;
}

#[tokio::test]
async fn auth_failures_block_address() {
    let limiter = Arc::new(RateLimiter::new(RateLimitConfig::default()));
    let srv = Server::with_limiter(
        Mode::Submission,
        true,
        |c| c.limits.max_auth_failures = 2,
        Some(limiter.clone()),
    )
    .await;
    let mut c = Client::new(handshake(srv.connect().await).await);
    assert_eq!(c.code().await, 220);
    ehlo(&mut c).await;
    assert_eq!(c.cmd(&plain("wrong")).await, 535);
    assert_eq!(c.cmd(&plain("wrong")).await, 535);
    assert_eq!(c.code().await, 421);
    assert!(c.closed().await);
    // Адрес заблокирован: новое соединение получает отказ сразу.
    let mut c = Client::new(handshake(srv.connect().await).await);
    assert_eq!(c.code().await, 421);
    srv.stop().await;
}

#[tokio::test]
async fn sending_rate_limit() {
    let limiter = Arc::new(RateLimiter::new(RateLimitConfig {
        account: BucketConfig {
            burst: 1,
            per_minute: 1,
        },
        ..RateLimitConfig::default()
    }));
    let srv = Server::with_limiter(Mode::Submission, true, |_| {}, Some(limiter)).await;
    let mut c = Client::new(handshake(srv.connect().await).await);
    assert_eq!(c.code().await, 220);
    ehlo(&mut c).await;
    assert_eq!(c.cmd(&plain("secret")).await, 235);
    send_mail(&mut c).await;
    let (code, text) = {
        c.send("MAIL FROM:<alice@example.org>\r\n").await;
        c.reply().await
    };
    assert_eq!(
        (code, text[0].as_str()),
        (451, "4.7.1 Sending rate exceeded, try again later")
    );
    srv.stop().await;
}

#[tokio::test]
async fn connection_limit() {
    let srv = Server::start(Mode::Inbound, false, |c| c.limits.max_connections = 1).await;
    let mut first = Client::new(srv.connect().await);
    assert_eq!(first.code().await, 220);
    let mut second = Client::new(srv.connect().await);
    assert_eq!(second.code().await, 421);
    assert_eq!(first.cmd("QUIT").await, 221);
    srv.stop().await;
}

#[tokio::test]
async fn submission_role_requires_certificates() {
    let svc = SmtpService::new(
        Mode::Submission,
        ServerDeps {
            handler: Arc::new(Recorder::default()),
            sasl: None,
            limiter: None,
            tls: None,
        },
    );
    let cfg = SmtpConfig {
        submission: vec![EndpointConfig::new("127.0.0.1:0".parse().expect("addr"))],
        ..SmtpConfig::default()
    };
    let ctx = RoleCtx {
        config: fixed_config(cfg),
        shutdown: CancellationToken::new(),
        drain_deadline: Duration::from_secs(1),
        ready: ReadyHandle::default(),
    };
    use tp_contracts::role::RoleService;
    assert!(matches!(
        svc.run(ctx).await,
        Err(tp_contracts::Error::Invalid(_))
    ));
}
