//! Доверенные источники: XCLIENT и XFORWARD только от шлюза, исходный клиент в конверте, приложение без SASL.

use tokio::io::DuplexStream;
use tp_contracts::TenantId;
use tp_contracts::transport::{AddressPattern, IpNet, TransportStore};
use tp_testkit::MemTransport;

use super::*;
use crate::config::SmtpConfig;
use crate::server::tests::{Client, Recorder, conn_info};
use crate::server::{Mode, Session, SessionCtx};

fn net(s: &str) -> IpNet {
    s.parse().expect("net")
}

fn gateway(xclient: bool) -> TrustedSource {
    TrustedSource {
        name: "edge".into(),
        tenant: None,
        matcher: SourceMatch::Networks(vec![net("192.0.2.0/24")]),
        kind: SourceKind::Gateway {
            xclient,
            authserv_id: Some("gw.example.org".into()),
        },
    }
}

fn application() -> TrustedSource {
    TrustedSource {
        name: "mfp".into(),
        tenant: Some(TenantId::DEFAULT),
        matcher: SourceMatch::Networks(vec![net("192.0.2.10/32")]),
        kind: SourceKind::Application {
            senders: vec![AddressPattern::new("@scan.example.org").expect("pattern")],
            external: false,
        },
    }
}

async fn session(
    mode: Mode,
    source: Option<TrustedSource>,
) -> (Client<DuplexStream>, Arc<Recorder>) {
    let rec = Arc::new(Recorder::default());
    let ctx = SessionCtx {
        mode,
        config: Arc::new(SmtpConfig {
            hostname: "mx.example.org".into(),
            ..SmtpConfig::default()
        }),
        handler: rec.clone(),
        sasl: None,
        limiter: None,
        send_limits: None,
        metrics: None,
        screen: None,
    };
    let (client, mut server) = tokio::io::duplex(64 * 1024);
    tokio::spawn(async move {
        let mut s = Session::new(ctx, conn_info(false), false);
        s.set_source(source.map(Arc::new));
        s.serve(&mut server, true).await
    });
    let mut c = Client::new(client);
    assert_eq!(c.code().await, 220);
    (c, rec)
}

async fn send_mail(c: &mut Client<DuplexStream>) {
    assert_eq!(c.cmd("MAIL FROM:<sender@example.net>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<bob@example.org>").await, 250);
    assert_eq!(c.cmd("DATA").await, 354);
    c.send("Subject: hi\r\n\r\nhello\r\n.\r\n").await;
    assert_eq!(c.code().await, 250);
}

#[tokio::test]
async fn xforward_from_other_address_is_refused() {
    // Не шлюз (источника нет), шлюз без `xclient`, приложение — 550.
    for source in [None, Some(gateway(false)), Some(application())] {
        let (mut c, _) = session(Mode::Inbound, source).await;
        c.send("EHLO gw.example.org\r\n").await;
        let (code, lines) = c.reply().await;
        assert_eq!(code, 250);
        assert!(!lines.iter().any(|l| l.starts_with("XCLIENT")), "{lines:?}");
        assert_eq!(c.cmd("XFORWARD ADDR=203.0.113.5").await, 550);
        assert_eq!(c.cmd("XCLIENT ADDR=203.0.113.5").await, 550);
        assert_eq!(c.cmd("QUIT").await, 221);
    }
}

#[tokio::test]
async fn xforward_sets_original_client_of_transaction() {
    let (mut c, rec) = session(Mode::Inbound, Some(gateway(true))).await;
    c.send("EHLO gw.example.org\r\n").await;
    let (code, lines) = c.reply().await;
    assert_eq!(code, 250);
    assert!(lines.iter().any(|l| l.starts_with("XCLIENT ")), "{lines:?}");
    assert!(
        lines.iter().any(|l| l.starts_with("XFORWARD ")),
        "{lines:?}"
    );
    assert_eq!(
        c.cmd("XFORWARD NAME=client.example.net ADDR=203.0.113.5 PORT=4321")
            .await,
        250
    );
    assert_eq!(
        c.cmd("XFORWARD HELO=client.example.net SOURCE=REMOTE")
            .await,
        250
    );
    assert_eq!(c.cmd("XFORWARD BOGUS=1").await, 501);
    send_mail(&mut c).await;
    // Следующая транзакция — снова от шлюза.
    send_mail(&mut c).await;
    assert_eq!(c.cmd("QUIT").await, 221);
    let mails = rec.mails();
    let (env, _) = &mails[0];
    assert_eq!(env.conn.client, "203.0.113.5:4321".parse().expect("addr"));
    assert_eq!(env.gateway, Some(conn_info(false).client));
    assert_eq!(env.helo, "client.example.net");
    assert_eq!(env.source.as_ref().map(|s| s.name.as_str()), Some("edge"));
    let (env, _) = &mails[1];
    assert_eq!(env.conn.client, conn_info(false).client);
    assert_eq!(env.gateway, None);
}

#[tokio::test]
async fn xclient_resets_session_to_original_client() {
    let (mut c, rec) = session(Mode::Inbound, Some(gateway(true))).await;
    assert_eq!(c.cmd("EHLO gw.example.org").await, 250);
    assert_eq!(c.cmd("MAIL FROM:<x@example.net>").await, 250);
    // В транзакции — нельзя.
    assert_eq!(c.cmd("XCLIENT ADDR=203.0.113.7").await, 503);
    assert_eq!(c.cmd("RSET").await, 250);
    assert_eq!(
        c.cmd("XCLIENT ADDR=IPV6:2001:db8::7 HELO=orig.example")
            .await,
        220
    );
    // Сессия в начальном состоянии: нужен EHLO.
    assert_eq!(c.cmd("MAIL FROM:<x@example.net>").await, 503);
    assert_eq!(c.cmd("EHLO gw.example.org").await, 250);
    send_mail(&mut c).await;
    assert_eq!(c.cmd("QUIT").await, 221);
    let (env, _) = &rec.mails()[0];
    assert_eq!(
        env.conn.client.ip(),
        "2001:db8::7".parse::<IpAddr>().expect("ip")
    );
    assert_eq!(env.gateway, Some(conn_info(false).client));
    assert_eq!(env.helo, "orig.example");
}

#[tokio::test]
async fn xclient_split_across_reads() {
    let (mut c, rec) = session(Mode::Inbound, Some(gateway(true))).await;
    for part in ["XCLI", "ENT ADDR=203.0.", "113.9\r\n"] {
        c.send(part).await;
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(c.code().await, 220);
    assert_eq!(c.cmd("EHLO gw.example.org").await, 250);
    send_mail(&mut c).await;
    let (env, _) = &rec.mails()[0];
    assert_eq!(env.conn.client.ip().to_string(), "203.0.113.9");
}

#[tokio::test]
async fn application_without_sasl_only_allowed_senders() {
    // submission: приложение без AUTH; чужой отправитель — 550.
    let (mut c, rec) = session(Mode::Submission, Some(application())).await;
    assert_eq!(c.cmd("EHLO mfp.example.org").await, 250);
    assert_eq!(c.cmd("MAIL FROM:<boss@example.org>").await, 550);
    assert_eq!(c.cmd("MAIL FROM:<copier@scan.example.org>").await, 250);
    assert_eq!(c.cmd("RCPT TO:<bob@example.org>").await, 250);
    assert_eq!(c.cmd("DATA").await, 354);
    c.send("Subject: scan\r\n\r\nscan\r\n.\r\n").await;
    assert_eq!(c.code().await, 250);
    assert_eq!(rec.mails().len(), 1);
    // Обычный клиент submission без AUTH — 530.
    let (mut c, _) = session(Mode::Submission, None).await;
    assert_eq!(c.cmd("EHLO client.example.net").await, 250);
    assert_eq!(c.cmd("MAIL FROM:<copier@scan.example.org>").await, 530);
}

#[tokio::test]
async fn resolve_by_network() {
    let store = MemTransport::new();
    let v = store.put_source(0, &gateway(true)).await.expect("put");
    store.put_source(v, &application()).await.expect("put");
    let sources = Sources::new(Arc::new(Routes::new(Arc::new(store))));
    // Наиболее точная подсеть — приложение (/32).
    let s = sources
        .resolve("192.0.2.10".parse().expect("ip"), &[])
        .await
        .expect("source");
    assert_eq!(s.name, "mfp");
    let s = sources
        .resolve("::ffff:192.0.2.11".parse().expect("ip"), &[])
        .await
        .expect("source");
    assert_eq!(s.name, "edge");
    assert!(
        sources
            .resolve("198.51.100.1".parse().expect("ip"), &[])
            .await
            .is_none()
    );
    // Без сертификата клиента имена `ClientCert` не совпадают.
    assert!(cert_names(&[gateway(true)], &[]).is_empty());
}

#[test]
fn parse_attributes() {
    let o = parse(
        XCommand::Client,
        "XCLIENT NAME=x ADDR=192.0.2.1 PORT=25 HELO=a+2Eb LOGIN=[UNAVAILABLE]\r\n",
    )
    .expect("parse");
    assert_eq!(o.addr, Some("192.0.2.1".parse().expect("ip")));
    assert_eq!(o.port, Some(25));
    assert_eq!(o.helo.as_deref(), Some("a.b"));
    let o = parse(XCommand::Forward, "XFORWARD ADDR=[TEMPUNAVAIL]").expect("parse");
    assert_eq!(o.addr, None);
    assert!(parse(XCommand::Forward, "XFORWARD LOGIN=x").is_err());
    assert!(parse(XCommand::Client, "XCLIENT ADDR=not-an-ip").is_err());
    assert!(parse(XCommand::Client, "XCLIENT ADDR").is_err());
    assert!(parse(XCommand::Client, "XCLIENT HELO=a+2").is_err());
    assert_eq!(x_command(b"xclient A"), Some(XCommand::Client));
    assert_eq!(x_command(b"XFORWARD "), Some(XCommand::Forward));
    assert_eq!(x_command(b"XFORWARDS"), None);
    assert_eq!(x_command(b"XCLIENT"), None);
}
