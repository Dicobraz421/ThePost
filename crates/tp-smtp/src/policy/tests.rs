//! MTA-STS и DANE: разбор политики, шаблоны MX, кэш и смена `id` на мок-DNS и мок-HTTPS, сверка TLSA;
//! доставка `MxTransport` на тестовый SMTP-сервер с STARTTLS (`127.0.0.1`, сертификат `localhost`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio::net::TcpListener;
use tokio_rustls::rustls::pki_types::pem::PemObject;
use tp_contracts::role::{CancellationToken, ReadyHandle, RoleCtx, fixed_config};
use tp_platform::{EndpointConfig, Listener};

use super::*;
use crate::config::{QueueConfig, SmtpConfig};
use crate::queue::send::MxTransport;
use crate::queue::tests::{a, mx, req, smtp_server, transport};
use crate::queue::{RcptResult, Transport};
use crate::server::Mode;
use crate::server::tests::Recorder;
use crate::server::tls_tests::{certs, connector, testdata};
use crate::server::{ServerDeps, SmtpService};

const POLICY: &str =
    "version: STSv1\r\nmode: enforce\r\nmx: localhost\r\nmx: *.example.net\r\nmax_age: 86400\r\n";

#[test]
fn parse_policy_and_mx_patterns() {
    let p = StsPolicy::parse(POLICY).expect("policy");
    assert_eq!(p.mode, StsMode::Enforce);
    assert_eq!(p.max_age, Duration::from_secs(86400));
    assert!(p.allows("localhost"));
    assert!(p.allows("MX1.Example.Net."));
    // `*.` — ровно одна метка слева.
    assert!(!p.allows("a.b.example.net"));
    assert!(!p.allows("example.net"));
    assert!(!p.allows("mx.example.org"));
    // Неизвестные поля пропускаются, срок ограничен годом.
    let p = StsPolicy::parse(
        "version: STSv1\nmode: testing\nmx: mx.example.org\nmax_age: 99999999999\nx: y\n",
    )
    .expect("policy");
    assert_eq!(p.mode, StsMode::Testing);
    assert_eq!(p.max_age, Duration::from_secs(MAX_AGE_LIMIT));
    assert!(StsPolicy::parse("version: STSv1\nmode: none\nmax_age: 1\n").is_ok());
    for bad in [
        "version: STSv2\nmode: enforce\nmx: a\nmax_age: 1\n",
        "version: STSv1\nmode: strict\nmx: a\nmax_age: 1\n",
        "version: STSv1\nmode: enforce\nmax_age: 1\n",
        "version: STSv1\nmode: enforce\nmx: a\n",
        "version: STSv1\nmode: enforce\nmx: a\nmax_age: x\n",
    ] {
        assert!(StsPolicy::parse(bad).is_err(), "{bad}");
    }
}

type TlsaAnswer = Result<Option<Vec<Tlsa>>, String>;

/// DNS по сценарию.
#[derive(Default)]
struct MockDns {
    id: Mutex<Option<Result<Option<String>, String>>>,
    tlsa: Mutex<HashMap<String, TlsaAnswer>>,
}

impl MockDns {
    fn set_id(&self, id: Result<Option<&str>, &str>) {
        *self.id.lock().expect("lock") =
            Some(id.map(|o| o.map(String::from)).map_err(String::from));
    }

    fn set_tlsa(&self, host: &str, r: Result<Option<Vec<Tlsa>>, &str>) {
        self.tlsa
            .lock()
            .expect("lock")
            .insert(host.to_owned(), r.map_err(String::from));
    }
}

#[async_trait]
impl PolicyDns for MockDns {
    async fn sts_id(&self, _domain: &str) -> Result<Option<String>, String> {
        self.id.lock().expect("lock").clone().unwrap_or(Ok(None))
    }

    async fn tlsa(&self, host: &str, _port: u16) -> TlsaAnswer {
        self.tlsa
            .lock()
            .expect("lock")
            .get(host)
            .cloned()
            .unwrap_or(Ok(None))
    }
}

/// HTTPS по сценарию: текст политики или ошибка; счётчик запросов.
struct MockHttp {
    body: Mutex<Result<String, String>>,
    fetches: AtomicUsize,
}

impl MockHttp {
    fn set(&self, body: Result<&str, &str>) {
        *self.body.lock().expect("lock") = body.map(String::from).map_err(String::from);
    }

    fn fetches(&self) -> usize {
        self.fetches.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl PolicyHttp for MockHttp {
    async fn fetch(&self, domain: &str, _cfg: &PolicyConfig) -> Result<String, String> {
        assert_eq!(domain, "example.net");
        self.fetches.fetch_add(1, Ordering::SeqCst);
        self.body.lock().expect("lock").clone()
    }
}

fn policy() -> (Arc<MockDns>, Arc<MockHttp>, Arc<TlsPolicy>) {
    let dns = Arc::new(MockDns::default());
    let http = Arc::new(MockHttp {
        body: Mutex::new(Err("not found".into())),
        fetches: AtomicUsize::new(0),
    });
    let p = Arc::new(TlsPolicy::new(dns.clone(), http.clone()));
    (dns, http, p)
}

#[tokio::test]
async fn mta_sts_cache_and_refresh() {
    let (dns, http, p) = policy();
    let cfg = PolicyConfig::default();
    // Нет TXT — нет политики, HTTPS не запрашивается.
    assert!(p.mta_sts(&cfg, "example.net").await.is_none());
    assert_eq!(http.fetches(), 0);
    // Есть TXT — политика получена и кэширована по id.
    dns.set_id(Ok(Some("1")));
    http.set(Ok(POLICY));
    let got = p.mta_sts(&cfg, "Example.NET.").await.expect("policy");
    assert_eq!(got.mode, StsMode::Enforce);
    assert!(p.mta_sts(&cfg, "example.net").await.is_some());
    assert_eq!(http.fetches(), 1);
    // Сбой DNS или исчезновение TXT — действует кэш.
    dns.set_id(Err("SERVFAIL"));
    assert!(p.mta_sts(&cfg, "example.net").await.is_some());
    dns.set_id(Ok(None));
    assert!(p.mta_sts(&cfg, "example.net").await.is_some());
    assert_eq!(http.fetches(), 1);
    // Новый id — перечитывание; сбой HTTPS — действует прежняя политика.
    dns.set_id(Ok(Some("2")));
    http.set(Err("timeout"));
    assert!(p.mta_sts(&cfg, "example.net").await.is_some());
    assert_eq!(http.fetches(), 2);
    // Новая политика `mode: none` отменяет прежнюю.
    http.set(Ok("version: STSv1\nmode: none\nmax_age: 60\n"));
    assert!(p.mta_sts(&cfg, "example.net").await.is_none());
    assert_eq!(http.fetches(), 3);
    // Выключено настройкой — без запросов.
    let off = PolicyConfig {
        mta_sts: false,
        ..PolicyConfig::default()
    };
    assert!(p.mta_sts(&off, "example.net").await.is_none());
    assert_eq!(http.fetches(), 3);
}

#[tokio::test]
async fn host_tls_precedence() {
    let (dns, _http, p) = policy();
    let cfg = PolicyConfig::default();
    let sts = StsPolicy::parse(POLICY).expect("policy");
    let rec = vec![tlsa(3, 1, 1, vec![0; 32])];
    dns.set_tlsa("mx.example.net", Ok(Some(rec.clone())));
    dns.set_tlsa("bad.example.net", Err("SERVFAIL"));
    // DANE приоритетнее MTA-STS.
    assert_eq!(
        p.host_tls(&cfg, Some(&sts), "mx.example.net", 25).await,
        Ok(HostTls::Dane(rec))
    );
    assert_eq!(
        p.host_tls(&cfg, Some(&sts), "other.example.net", 25).await,
        Ok(HostTls::Verified)
    );
    assert_eq!(
        p.host_tls(&cfg, None, "other.example.net", 25).await,
        Ok(HostTls::Default)
    );
    assert!(p.host_tls(&cfg, None, "bad.example.net", 25).await.is_err());
    let no_dane = PolicyConfig {
        dane: false,
        ..PolicyConfig::default()
    };
    assert_eq!(
        p.host_tls(&no_dane, None, "bad.example.net", 25).await,
        Ok(HostTls::Default)
    );
}

fn tlsa(usage: u8, selector: u8, matching: u8, data: Vec<u8>) -> Tlsa {
    Tlsa {
        usage,
        selector,
        matching,
        data,
    }
}

fn test_cert() -> CertificateDer<'static> {
    let pem = std::fs::read(testdata("cert.pem")).expect("cert");
    CertificateDer::from_pem_slice(&pem).expect("pem")
}

fn spki(cert: &CertificateDer<'_>) -> Vec<u8> {
    webpki::EndEntityCert::try_from(cert)
        .expect("cert")
        .subject_public_key_info()
        .to_vec()
}

#[test]
fn dane_matching() {
    let cert = test_cert();
    let chain = [cert.clone()];
    let now = UnixTime::now();
    let full = cert.to_vec();
    let key = spki(&cert);
    // SPKI — DER-последовательность (тег SEQUENCE).
    assert_eq!(key.first(), Some(&0x30));
    for ok in [
        tlsa(3, 0, 0, full.clone()),
        tlsa(3, 0, 1, Sha256::digest(&full).to_vec()),
        tlsa(3, 1, 1, Sha256::digest(&key).to_vec()),
        tlsa(3, 1, 2, Sha512::digest(&key).to_vec()),
    ] {
        assert_eq!(
            dane_verify(std::slice::from_ref(&ok), &chain, "localhost", now),
            Ok(()),
            "{ok:?}"
        );
    }
    // Несовпадение — отказ; одна совпавшая запись из нескольких — достаточно.
    let wrong = tlsa(3, 1, 1, vec![0; 32]);
    assert!(dane_verify(std::slice::from_ref(&wrong), &chain, "localhost", now).is_err());
    let both = [wrong.clone(), tlsa(3, 1, 1, Sha256::digest(&key).to_vec())];
    assert_eq!(dane_verify(&both, &chain, "localhost", now), Ok(()));
    // DANE-TA: сертификата удостоверяющего центра в цепочке нет — отказ.
    assert!(dane_verify(&[tlsa(2, 0, 0, full)], &chain, "localhost", now).is_err());
    // Только PKIX-записи (непригодны для SMTP) — шифрование без проверки.
    assert_eq!(
        dane_verify(&[tlsa(1, 0, 0, vec![1])], &chain, "localhost", now),
        Ok(())
    );
    // Нет сертификата — отказ.
    assert!(dane_verify(&[wrong], &[], "localhost", now).is_err());
}

/// SMTP-сервер роли smtp-in с STARTTLS (сертификат `localhost`) на `127.0.0.1:0`.
async fn tls_smtp_server(rec: Arc<Recorder>) -> (u16, CancellationToken) {
    let tcp = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = tcp.local_addr().expect("addr");
    let tls = certs().await;
    let listener =
        Listener::from_tcp(tcp, &EndpointConfig::new(addr), Some(tls.clone())).expect("listener");
    let svc = SmtpService::new(
        Mode::Inbound,
        ServerDeps {
            handler: rec,
            sasl: None,
            limiter: None,
            tls: Some(tls),
            send_limits: None,
            metrics: None,
        },
    );
    let shutdown = CancellationToken::new();
    let ctx = RoleCtx {
        config: fixed_config(SmtpConfig {
            hostname: "localhost".into(),
            ..SmtpConfig::default()
        }),
        shutdown: shutdown.clone(),
        drain_deadline: Duration::from_secs(1),
        ready: ReadyHandle::default(),
        load: tp_contracts::load::fixed_load(tp_contracts::load::LoadLevel::Normal),
    };
    tokio::spawn(async move { svc.serve(vec![listener], ctx).await });
    (addr.port(), shutdown)
}

/// Отправка с политиками: MX `localhost` домена `example.net`, корни проверки — тестовый сертификат.
fn sender(p: Arc<TlsPolicy>) -> MxTransport {
    let t = transport()
        .with_policy(p)
        .with_verified_connector(connector());
    mx(&t, "example.net", &[(10, "localhost")]);
    a(&t, "localhost", [127, 0, 0, 1].into());
    t
}

#[tokio::test]
async fn delivery_under_dane() {
    let rec = Arc::new(Recorder::default());
    let (port, stop) = tls_smtp_server(rec.clone()).await;
    let (dns, _http, p) = policy();
    let t = sender(p);
    let cfg = QueueConfig {
        port,
        ..QueueConfig::default()
    };
    let rcpts = vec!["x@example.net".to_owned()];
    let key = spki(&test_cert());
    // Совпадение с записью TLSA — доставлено.
    let ok = tlsa(3, 1, 1, Sha256::digest(&key).to_vec());
    dns.set_tlsa("localhost", Ok(Some(vec![ok])));
    let res = t.send(req(&cfg, "example.net", &rcpts)).await;
    assert!(matches!(&res[0], RcptResult::Delivered(_)), "{res:?}");
    assert_eq!(rec.mails().len(), 1);
    // Несовпадение — узел пропущен, открытым текстом не повторяется.
    dns.set_tlsa("localhost", Ok(Some(vec![tlsa(3, 1, 1, vec![0; 32])])));
    let res = t.send(req(&cfg, "example.net", &rcpts)).await;
    assert!(
        matches!(&res[0], RcptResult::Temporary(r) if r.contains("DANE")),
        "{res:?}"
    );
    // Сбой поиска TLSA — узел пропущен.
    dns.set_tlsa("localhost", Err("SERVFAIL"));
    let res = t.send(req(&cfg, "example.net", &rcpts)).await;
    assert!(
        matches!(&res[0], RcptResult::Temporary(r) if r.contains("TLSA")),
        "{res:?}"
    );
    assert_eq!(rec.mails().len(), 1);
    stop.cancel();
}

#[tokio::test]
async fn delivery_under_mta_sts() {
    let rec = Arc::new(Recorder::default());
    let (port, stop) = tls_smtp_server(rec.clone()).await;
    let (dns, http, p) = policy();
    let t = sender(p);
    let cfg = QueueConfig {
        port,
        ..QueueConfig::default()
    };
    let rcpts = vec!["x@example.net".to_owned()];
    // MX в политике, сертификат проверен — доставлено.
    dns.set_id(Ok(Some("1")));
    http.set(Ok(POLICY));
    let res = t.send(req(&cfg, "example.net", &rcpts)).await;
    assert!(matches!(&res[0], RcptResult::Delivered(_)), "{res:?}");
    // MX вне политики — отказ от доставки до смены политики.
    dns.set_id(Ok(Some("2")));
    http.set(Ok(
        "version: STSv1\nmode: enforce\nmx: mx.example.net\nmax_age: 600\n",
    ));
    let res = t.send(req(&cfg, "example.net", &rcpts)).await;
    assert!(
        matches!(&res[0], RcptResult::Temporary(r) if r.contains("4.7.5")),
        "{res:?}"
    );
    // Режим testing — несоответствие только в журнал.
    dns.set_id(Ok(Some("3")));
    http.set(Ok(
        "version: STSv1\nmode: testing\nmx: mx.example.net\nmax_age: 600\n",
    ));
    let res = t.send(req(&cfg, "example.net", &rcpts)).await;
    assert!(matches!(&res[0], RcptResult::Delivered(_)), "{res:?}");
    assert_eq!(rec.mails().len(), 2);
    stop.cancel();
    // Сервер без STARTTLS при enforce — отказ.
    let plain = Arc::new(Recorder::default());
    let port = smtp_server(plain.clone()).await;
    dns.set_id(Ok(Some("4")));
    http.set(Ok(POLICY));
    let cfg = QueueConfig {
        port,
        ..QueueConfig::default()
    };
    let res = t.send(req(&cfg, "example.net", &rcpts)).await;
    assert!(
        matches!(&res[0], RcptResult::Temporary(r) if r.contains("STARTTLS not offered")),
        "{res:?}"
    );
    assert!(plain.mails().is_empty());
}
