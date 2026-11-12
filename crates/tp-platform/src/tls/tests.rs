//! Тесты TLS сервера: версии, перечитывание цепочки и ключа из хранилища секретов, слушатель, разбор секции.

use rustls::pki_types::ServerName;
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsConnector;
use tp_testkit::MemSecretStore;

use super::*;
use crate::fswatch::NoFsEvents;
use crate::fswatch::tests::MemFsEvents;

pub(crate) const CERT_A: &str = include_str!("../testdata/cert-a.pem");
pub(crate) const KEY_A: &str = include_str!("../testdata/key-a.pem");
pub(crate) const CERT_B: &str = include_str!("../testdata/cert-b.pem");
pub(crate) const KEY_B: &str = include_str!("../testdata/key-b.pem");
/// CA сертификатов клиента и выписанный им сертификат шлюза `gateway.example.test` (mTLS).
pub(crate) const CA_CLIENT: &str = include_str!("../testdata/ca-client.pem");
pub(crate) const CERT_GW: &str = include_str!("../testdata/cert-gw.pem");
pub(crate) const KEY_GW: &str = include_str!("../testdata/key-gw.pem");
/// CA со списками отзыва (PLT-28): сертификаты клиентов `ok.example.test` и `revoked.example.test`, список
/// без отозванных (`CRL_EMPTY`) и с отозванным `revoked.example.test` (`CRL_REVOKED`, он же в DER).
pub(crate) const CA_CRL: &str = include_str!("../testdata/ca-crl.pem");
pub(crate) const CERT_CRL_OK: &str = include_str!("../testdata/cert-crl-ok.pem");
pub(crate) const KEY_CRL_OK: &str = include_str!("../testdata/key-crl-ok.pem");
pub(crate) const CERT_CRL_REVOKED: &str = include_str!("../testdata/cert-crl-revoked.pem");
pub(crate) const KEY_CRL_REVOKED: &str = include_str!("../testdata/key-crl-revoked.pem");
pub(crate) const CRL_EMPTY: &str = include_str!("../testdata/crl-empty.pem");
pub(crate) const CRL_REVOKED: &str = include_str!("../testdata/crl-revoked.pem");
pub(crate) const CRL_REVOKED_DER: &[u8] = include_bytes!("../testdata/crl-revoked.der");

/// Временный каталог теста; удаляется при drop.
pub(crate) struct TempDir(pub(crate) PathBuf);

impl TempDir {
    pub(crate) fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "tp-platform-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) fn der(pem: &str) -> CertificateDer<'static> {
    rustls_pemfile::certs(&mut pem.as_bytes())
        .next()
        .expect("cert")
        .expect("pem")
}

/// Клиент, доверяющий самоподписанным `certs`, с версиями `versions`.
pub(crate) fn connector(
    certs: &[&str],
    versions: &[&'static rustls::SupportedProtocolVersion],
) -> TlsConnector {
    let mut roots = RootCertStore::empty();
    for pem in certs {
        roots.add(der(pem)).expect("root");
    }
    let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)
        .expect("versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(cfg))
}

/// Пара теста: цепочка в файле `cert.pem` каталога, ключ — в хранилище секретов в памяти по ссылке `tls.key`.
pub(crate) struct Pair {
    pub(crate) cfg: TlsConfig,
    pub(crate) store: Arc<MemSecretStore>,
}

impl Pair {
    /// Секция по умолчанию с цепочкой `cert` в `dir/cert.pem` и ключом `key` в секрете `thepost/tls#key`.
    pub(crate) fn new(dir: &Path, cert: &str, key: &str) -> Self {
        let pair = Self {
            cfg: TlsConfig {
                cert_file: dir.join("cert.pem"),
                ..TlsConfig::default()
            },
            store: Arc::new(MemSecretStore::default()),
        };
        pair.write(cert, key);
        pair
    }

    /// Новая пара: цепочка — в файл, ключ — новой версией секрета.
    pub(crate) fn write(&self, cert: &str, key: &str) {
        std::fs::write(&self.cfg.cert_file, cert).expect("write cert");
        self.store.set(&self.cfg.key, key);
    }

    /// Сертификаты по секции пары с её хранилищем.
    pub(crate) async fn load(&self) -> TlsCerts {
        TlsCerts::load(&self.cfg, self.store.clone())
            .await
            .expect("load")
    }
}

/// Рукопожатие с сервером на `certs`: версия протокола и сертификат сервера.
async fn handshake(
    certs: &TlsCerts,
    client: TlsConnector,
) -> std::io::Result<(rustls::ProtocolVersion, CertificateDer<'static>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let acceptor = certs.acceptor();
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.expect("accept");
        let mut tls = acceptor.accept(tcp).await?;
        let mut buf = [0u8; 4];
        tls.read_exact(&mut buf).await?;
        tls.write_all(b"pong").await?;
        tls.shutdown().await
    });
    let tcp = TcpStream::connect(addr).await.expect("connect");
    let name = ServerName::try_from("localhost").expect("name");
    let mut tls = client.connect(name, tcp).await?;
    tls.write_all(b"ping").await?;
    let mut buf = [0u8; 4];
    tls.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"pong");
    let (_, conn) = tls.get_ref();
    let version = conn.protocol_version().expect("version");
    let cert = conn.peer_certificates().expect("peer certs")[0].clone();
    let _ = server.await;
    Ok((version, cert))
}

#[tokio::test]
async fn tls13_by_default_tls12_as_option() {
    let dir = TempDir::new("tls-versions");
    let pair = Pair::new(&dir.0, CERT_A, KEY_A);
    let certs = pair.load().await;
    let any = &[&rustls::version::TLS13, &rustls::version::TLS12];
    let (version, cert) = handshake(&certs, connector(&[CERT_A], any))
        .await
        .expect("tls13");
    assert_eq!(version, rustls::ProtocolVersion::TLSv1_3);
    assert_eq!(cert, der(CERT_A));
    let tls12_only = || connector(&[CERT_A], &[&rustls::version::TLS12]);
    assert!(handshake(&certs, tls12_only()).await.is_err());

    certs
        .apply(&TlsConfig {
            min_version: TlsVersion::Tls12,
            ..pair.cfg.clone()
        })
        .await
        .expect("apply");
    let (version, _) = handshake(&certs, tls12_only()).await.expect("tls12");
    assert_eq!(version, rustls::ProtocolVersion::TLSv1_2);
}

#[tokio::test]
async fn files_are_reread_without_restart() {
    let dir = TempDir::new("tls-reload");
    let pair = Pair::new(&dir.0, CERT_A, KEY_A);
    let certs = pair.load().await;
    assert!(!certs.reload_if_changed().await.expect("same"));

    // Повреждённый ключ: перечитывание отклонено, действуют прежние сертификаты.
    pair.store.set(&pair.cfg.key, "garbage");
    assert!(matches!(
        certs.reload_if_changed().await,
        Err(Error::Invalid(_))
    ));
    let both = || connector(&[CERT_A, CERT_B], &[&rustls::version::TLS13]);
    assert_eq!(handshake(&certs, both()).await.expect("old").1, der(CERT_A));

    pair.write(CERT_B, KEY_B);
    assert!(certs.reload_if_changed().await.expect("changed"));
    assert_eq!(handshake(&certs, both()).await.expect("new").1, der(CERT_B));
}

/// Подпись `msg` ключом `ck` проверяется открытым ключом его сертификата узла (ECDSA P-256 тестовых пар).
fn signed_by_chain(ck: &CertifiedKey, msg: &[u8]) -> bool {
    use rustls::SignatureScheme;
    let scheme = SignatureScheme::ECDSA_NISTP256_SHA256;
    let Some(signer) = ck.key.choose_scheme(&[scheme]) else {
        return false;
    };
    let sig = signer.sign(msg).expect("sign");
    // Открытый ключ P-256 — последние 65 байт SubjectPublicKeyInfo сертификата (несжатая точка).
    let cert = rustls::server::ParsedCertificate::try_from(&ck.cert[0]).expect("parse cert");
    let spki = cert.subject_public_key_info();
    let point = &spki.as_ref()[spki.as_ref().len() - 65..];
    let algs = rustls::crypto::aws_lc_rs::default_provider().signature_verification_algorithms;
    let (_, verifiers) = algs
        .mapping
        .iter()
        .find(|(s, _)| *s == scheme)
        .expect("p-256 verifier");
    verifiers[0].verify_signature(point, msg, &sig).is_ok()
}

#[tokio::test]
async fn certified_key_follows_reload() {
    let dir = TempDir::new("tls-certified");
    let pair = Pair::new(&dir.0, CERT_A, KEY_A);
    let certs = pair.load().await;
    let a = certs.certified_key();
    assert_eq!(a.cert, vec![der(CERT_A)]);
    a.keys_match().expect("key matches cert a");
    assert!(signed_by_chain(&a, b"profile"));

    // Ошибка перечитывания — прежняя пара.
    pair.store.set(&pair.cfg.key, "garbage");
    assert!(certs.reload_if_changed().await.is_err());
    assert_eq!(certs.certified_key().cert, vec![der(CERT_A)]);

    pair.write(CERT_B, KEY_B);
    certs.reload().await.expect("reload");
    let b = certs.certified_key();
    assert_eq!(b.cert, vec![der(CERT_B)]);
    b.keys_match().expect("key matches cert b");
    assert!(signed_by_chain(&b, b"profile"));
    // Старая пара не подходит к новой цепочке.
    let mixed = CertifiedKey::new(b.cert.clone(), a.key.clone());
    assert!(mixed.keys_match().is_err());
    assert!(!signed_by_chain(&mixed, b"profile"));
}

/// Ждёт замены конфигурации rustls после `before`.
async fn replaced(certs: &TlsCerts, before: &Arc<ServerConfig>) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while Arc::ptr_eq(&certs.server_config(), before) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("certificates replaced in time");
}

#[tokio::test]
async fn file_event_reloads_without_waiting_for_poll() {
    let dir = TempDir::new("tls-event");
    let mut pair = Pair::new(&dir.0, CERT_A, KEY_A);
    pair.cfg.reload_interval = Duration::from_secs(3600);
    pair.cfg.key_poll_interval = Duration::from_secs(3600);
    let certs = Arc::new(pair.load().await);
    let events = Arc::new(MemFsEvents::default());
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(certs.clone().run_with(events.clone(), shutdown.clone()));
    tokio::time::timeout(Duration::from_secs(5), async {
        while events.watches() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("watch installed");

    let before = certs.server_config();
    pair.write(CERT_B, KEY_B);
    assert_eq!(events.fire(&dir.0), 1);
    replaced(&certs, &before).await;
    let both = connector(&[CERT_A, CERT_B], &[&rustls::version::TLS13]);
    assert_eq!(handshake(&certs, both).await.expect("new").1, der(CERT_B));

    shutdown.cancel();
    task.await.expect("join");
}

#[tokio::test]
async fn poll_fallback_without_fs_events() {
    let dir = TempDir::new("tls-poll");
    let mut pair = Pair::new(&dir.0, CERT_A, KEY_A);
    pair.cfg.reload_interval = Duration::from_millis(20);
    pair.cfg.key_poll_interval = Duration::from_secs(3600);
    let certs = Arc::new(pair.load().await);
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(
        certs
            .clone()
            .run_with(Arc::new(NoFsEvents), shutdown.clone()),
    );

    let before = certs.server_config();
    pair.write(CERT_B, KEY_B);
    replaced(&certs, &before).await;

    shutdown.cancel();
    task.await.expect("join");
}

fn key_ref() -> SecretRef {
    "thepost/tls#key".parse().expect("ref")
}

/// Сертификат сервера, который слушатель с неявным TLS на `certs` отдаёт клиенту.
async fn listener_cert(certs: &Arc<TlsCerts>) -> CertificateDer<'static> {
    use crate::listen::{EndpointConfig, Listener};
    let mut endpoint = EndpointConfig::new("127.0.0.1:0".parse().expect("addr"));
    endpoint.implicit_tls = true;
    let tcp = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let at = tcp.local_addr().expect("addr");
    let l = Listener::from_tcp(tcp, &endpoint, Some(certs.clone())).expect("listener");
    let server = tokio::spawn(async move {
        let conn = l.accept().await.expect("accept").establish().await?;
        assert!(conn.info.tls);
        let mut stream = conn.stream;
        stream.write_all(b"pong").await.expect("pong");
        stream.shutdown().await.expect("shutdown");
        tp_contracts::Result::Ok(())
    });
    let tcp = TcpStream::connect(at).await.expect("connect");
    let name = ServerName::try_from("localhost").expect("name");
    let mut tls = connector(&[CERT_A, CERT_B], &[&rustls::version::TLS13])
        .connect(name, tcp)
        .await
        .expect("handshake");
    let mut buf = [0u8; 4];
    tls.read_exact(&mut buf).await.expect("pong");
    let cert = tls.get_ref().1.peer_certificates().expect("peer certs")[0].clone();
    server.await.expect("join").expect("establish");
    cert
}

#[tokio::test]
async fn listener_starts_with_key_from_secret_store_and_follows_new_version() {
    let dir = TempDir::new("tls-secret-key");
    let cfg = TlsConfig {
        cert_file: dir.0.join("cert.pem"),
        ..TlsConfig::default()
    };
    std::fs::write(&cfg.cert_file, CERT_A).expect("write cert");
    let store = Arc::new(MemSecretStore::default());
    // Секрета ещё нет — пара не загружается; файла ключа нет и не ищется.
    let err = TlsCerts::load(&cfg, store.clone())
        .await
        .expect_err("no secret");
    assert!(
        matches!(&err, Error::Invalid(m) if m.contains("thepost/tls#key")),
        "{err}"
    );
    store.set(&key_ref(), KEY_A);
    let certs = Arc::new(TlsCerts::load(&cfg, store.clone()).await.expect("load"));
    assert_eq!(listener_cert(&certs).await, der(CERT_A));
    assert!(!certs.reload_if_changed().await.expect("same version"));

    // Новая версия секрета раньше новой цепочки: ключ не подходит — действует прежняя пара.
    store.set(&key_ref(), KEY_B);
    assert!(matches!(
        certs.reload_if_changed().await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(listener_cert(&certs).await, der(CERT_A));
    std::fs::write(&cfg.cert_file, CERT_B).expect("write cert");
    assert!(certs.reload_if_changed().await.expect("new version"));
    assert_eq!(listener_cert(&certs).await, der(CERT_B));
    certs.certified_key().keys_match().expect("pair b");

    // Не PEM в секрете и недоступное хранилище — прежняя пара остаётся.
    store.set(&key_ref(), "garbage");
    let err = certs.reload_if_changed().await.expect_err("garbage");
    assert!(
        matches!(&err, Error::Invalid(m) if !m.contains("garbage")),
        "{err}"
    );
    store.set(&key_ref(), KEY_B);
    store.seal();
    assert!(certs.reload().await.expect_err("sealed").is_retryable());
    store.unseal();
    assert_eq!(listener_cert(&certs).await, der(CERT_B));
}

#[tokio::test]
async fn secret_version_is_polled_even_with_fs_events() {
    let dir = TempDir::new("tls-secret-poll");
    let mut pair = Pair::new(&dir.0, CERT_A, KEY_A);
    pair.cfg.reload_interval = Duration::from_secs(3600);
    pair.cfg.key_poll_interval = Duration::from_millis(20);
    let certs = Arc::new(pair.load().await);
    let events = Arc::new(MemFsEvents::default());
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(certs.clone().run_with(events.clone(), shutdown.clone()));

    // Событий ОС о файлах нет: новую пару замечает только опрос секрета.
    let before = certs.server_config();
    pair.write(CERT_B, KEY_B);
    replaced(&certs, &before).await;
    assert_eq!(certs.certified_key().cert, vec![der(CERT_B)]);

    shutdown.cancel();
    task.await.expect("join");
}

#[tokio::test]
async fn missing_or_mismatched_files_are_invalid() {
    let dir = TempDir::new("tls-invalid");
    let Pair { mut cfg, store } = Pair::new(&dir.0, CERT_A, KEY_B);
    let load = |cfg: TlsConfig| {
        let store = store.clone();
        async move { TlsCerts::load(&cfg, store).await }
    };
    assert!(matches!(load(cfg.clone()).await, Err(Error::Invalid(_))));
    cfg.cert_file = dir.0.join("absent.pem");
    let err = load(cfg.clone()).await.expect_err("absent");
    assert!(err.to_string().contains("absent.pem"), "{err}");
    let empty = dir.0.join("empty.pem");
    std::fs::write(&empty, "").expect("write");
    cfg.cert_file = empty;
    assert!(matches!(load(cfg).await, Err(Error::Invalid(_))));
}

#[test]
fn config_from_yaml() {
    let cfg: TlsConfig =
        serde_yaml_ng::from_str("min_version: \"1.2\"\ncert_file: /x/c.pem\n").expect("parse");
    assert_eq!(cfg.min_version, TlsVersion::Tls12);
    assert_eq!(cfg.cert_file, PathBuf::from("/x/c.pem"));
    assert!(serde_yaml_ng::from_str::<TlsConfig>("min_version: \"1.1\"").is_err());
    assert_eq!(cfg.reload_interval, Duration::from_secs(60));
    let off: TlsConfig = serde_yaml_ng::from_str("reload_interval: 0s").expect("0s");
    assert!(off.reload_interval.is_zero());
    assert!(serde_yaml_ng::from_str::<TlsConfig>("reload_interval: 60000").is_err());
    assert!(serde_yaml_ng::from_str::<TlsConfig>("reload_interval_ms: 60000").is_err());
    assert_eq!(
        cfg.key,
        key_ref(),
        "ключ по умолчанию — секрет thepost/tls#key"
    );
    assert_eq!(cfg.key_poll_interval, Duration::from_secs(60));
    let secret: TlsConfig =
        serde_yaml_ng::from_str("key: { secret: \"thepost/tls/mx#pem\" }\nkey_poll_interval: 5m\n")
            .expect("secret key");
    assert_eq!(secret.key, "thepost/tls/mx#pem".parse().expect("ref"));
    assert_eq!(secret.key_poll_interval, Duration::from_secs(300));
    // Ключ открытым текстом в конфигурации — ошибка, значение в неё не попадает.
    let err = serde_yaml_ng::from_str::<TlsConfig>("key: PRIVATE-KEY-TEXT").expect_err("plain");
    assert!(!err.to_string().contains("PRIVATE-KEY-TEXT"), "{err}");
}

/// PLT-31: ключ — только ссылка на секрет; `key_file` в секции — ошибка разбора.
#[test]
fn key_file_is_rejected() {
    let err = serde_yaml_ng::from_str::<TlsConfig>("cert_file: /x/c.pem\nkey_file: /x/k.pem\n")
        .expect_err("key_file");
    assert!(err.to_string().contains("key_file"), "{err}");
    let both = "key_file: /x/k.pem\nkey: { secret: \"thepost/tls#key\" }\n";
    assert!(serde_yaml_ng::from_str::<TlsConfig>(both).is_err());
}

/// Проверка пары для `thepost doctor`: ключ из PEM подходит к цепочке файла; чужой ключ и не PEM — `Invalid`
/// без значения ключа в сообщении.
#[tokio::test]
async fn check_pair_matches_key_to_chain() {
    let dir = TempDir::new("tls-check-pair");
    let cert = dir.0.join("cert.pem");
    std::fs::write(&cert, CERT_A).expect("write cert");
    check_pair(&cert, &Secret::new(KEY_A.to_owned()))
        .await
        .expect("pair a");
    assert!(matches!(
        check_pair(&cert, &Secret::new(KEY_B.to_owned())).await,
        Err(Error::Invalid(_))
    ));
    let err = check_pair(&cert, &Secret::new("not-a-key".to_owned()))
        .await
        .expect_err("garbage");
    assert!(
        matches!(&err, Error::Invalid(m) if !m.contains("not-a-key")),
        "{err}"
    );
}
