//! TLS сервера (SRS 6.2): конфигурация rustls из PEM-файлов (цепочка и ключ) с перечитыванием без перезапуска —
//! по изменению файлов (опрос mtime), по новой версии секции и по запросу (например, после выпуска ACME).
//!
//! Клиентский доступ — TLS 1.3; TLS 1.2 включается настройкой `min_version: "1.2"` для устаревших клиентов.
//! Ошибка перечитывания оставляет в силе прежние сертификаты.

use std::fmt;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tp_contracts::Error;
use tp_contracts::config::duration;
use tp_contracts::role::CancellationToken;

/// Секция `tls`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TlsConfig {
    /// PEM-файл с цепочкой сертификатов (сертификат узла первым).
    pub cert_file: PathBuf,
    /// PEM-файл с закрытым ключом (PKCS#8, PKCS#1 или SEC1).
    pub key_file: PathBuf,
    /// Минимальная версия TLS для клиентского доступа.
    pub min_version: TlsVersion,
    /// Период проверки изменения файлов (`1m`); `0s` — только по новой версии секции и по запросу.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub reload_interval: Duration,
}

impl Default for TlsConfig {
    fn default() -> Self {
        Self {
            cert_file: PathBuf::from("/etc/thepost/tls/fullchain.pem"),
            key_file: PathBuf::from("/etc/thepost/tls/privkey.pem"),
            min_version: TlsVersion::Tls13,
            reload_interval: Duration::from_secs(60),
        }
    }
}

/// Версия TLS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
pub enum TlsVersion {
    /// TLS 1.2 (устаревшие клиенты).
    #[serde(rename = "1.2")]
    Tls12,
    /// TLS 1.3.
    #[default]
    #[serde(rename = "1.3")]
    Tls13,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum TlsError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0}: no certificates found")]
    NoCert(PathBuf),
    #[error("{0}: no private key found")]
    NoKey(PathBuf),
    #[error("tls: {0}")]
    Rustls(#[from] rustls::Error),
    #[error("tls: loader task failed")]
    Join,
}

impl From<TlsError> for Error {
    fn from(e: TlsError) -> Self {
        match e {
            TlsError::Join => Error::Internal(e.to_string()),
            _ => Error::Invalid(e.to_string()),
        }
    }
}

/// Отметка версии файлов: mtime и размер цепочки и ключа.
type Stamp = [(Option<SystemTime>, u64); 2];

/// Действующие сертификаты сервера. Разделяется слушателями через `Arc`; новые соединения
/// получают текущую конфигурацию, установленные соединения не затрагиваются.
pub struct TlsCerts {
    current: RwLock<Arc<ServerConfig>>,
    files: Mutex<(TlsConfig, Stamp)>,
}

impl fmt::Debug for TlsCerts {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let files = self.files.lock().unwrap_or_else(PoisonError::into_inner);
        f.debug_struct("TlsCerts")
            .field("cert_file", &files.0.cert_file)
            .field("min_version", &files.0.min_version)
            .finish_non_exhaustive()
    }
}

impl TlsCerts {
    /// Загружает сертификаты по `cfg` (чтение файлов — в `spawn_blocking`).
    pub async fn load(cfg: &TlsConfig) -> tp_contracts::Result<Self> {
        let cfg = cfg.clone();
        let (server, stamp, cfg) = tokio::task::spawn_blocking(move || {
            read_config(&cfg).map(|(server, stamp)| (server, stamp, cfg))
        })
        .await
        .map_err(|_| TlsError::Join)??;
        Ok(Self {
            current: RwLock::new(Arc::new(server)),
            files: Mutex::new((cfg, stamp)),
        })
    }

    /// Текущая конфигурация rustls.
    pub fn server_config(&self) -> Arc<ServerConfig> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Акцептор на текущей конфигурации.
    pub fn acceptor(&self) -> TlsAcceptor {
        TlsAcceptor::from(self.server_config())
    }

    /// TLS-рукопожатие поверх принятого соединения (неявный TLS или STARTTLS).
    pub async fn accept(&self, tcp: TcpStream) -> std::io::Result<TlsStream<TcpStream>> {
        self.acceptor().accept(tcp).await
    }

    /// Перечитывает файлы текущей секции безусловно (например, после выпуска сертификата ACME).
    pub async fn reload(&self) -> tp_contracts::Result<()> {
        let cfg = self.config();
        self.reload_with(cfg).await
    }

    /// Применяет новую версию секции: пути и версия TLS. Ошибка — прежние сертификаты остаются.
    pub async fn apply(&self, cfg: &TlsConfig) -> tp_contracts::Result<()> {
        if *cfg == self.config() {
            return Ok(());
        }
        self.reload_with(cfg.clone()).await
    }

    /// Перечитывает файлы, если изменились их mtime или размер. `true` — сертификаты заменены.
    pub async fn reload_if_changed(&self) -> tp_contracts::Result<bool> {
        let (cfg, old) = self
            .files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let probe = cfg.clone();
        let stamp = tokio::task::spawn_blocking(move || stamp(&probe))
            .await
            .map_err(|_| TlsError::Join)?;
        if stamp == old {
            return Ok(false);
        }
        self.reload_with(cfg).await?;
        Ok(true)
    }

    /// Проверяет файлы каждые `reload_interval` до сигнала остановки; ошибки пишет в лог.
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        loop {
            let interval = self.config().reload_interval;
            // Опрос выключен: период могут включить новой версией секции.
            let wait = if interval.is_zero() {
                Duration::from_secs(60)
            } else {
                interval
            };
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(wait) => {}
            }
            if interval.is_zero() {
                continue;
            }
            match self.reload_if_changed().await {
                Ok(true) => tracing::info!("tls certificates reloaded"),
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "tls reload failed, keeping previous certificates")
                }
            }
        }
    }

    fn config(&self) -> TlsConfig {
        self.files
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .0
            .clone()
    }

    async fn reload_with(&self, cfg: TlsConfig) -> tp_contracts::Result<()> {
        let (server, stamp, cfg) = tokio::task::spawn_blocking(move || {
            read_config(&cfg).map(|(server, stamp)| (server, stamp, cfg))
        })
        .await
        .map_err(|_| TlsError::Join)??;
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(server);
        *self.files.lock().unwrap_or_else(PoisonError::into_inner) = (cfg, stamp);
        Ok(())
    }
}

/// Конфигурация rustls сервера из цепочки и ключа. Провайдер криптографии — установленный в процессе,
/// иначе aws-lc-rs.
pub fn server_config(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    min_version: TlsVersion,
) -> tp_contracts::Result<ServerConfig> {
    Ok(build(chain, key, min_version)?)
}

fn build(
    chain: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
    min_version: TlsVersion,
) -> Result<ServerConfig, TlsError> {
    let provider = CryptoProvider::get_default()
        .cloned()
        .unwrap_or_else(|| Arc::new(rustls::crypto::aws_lc_rs::default_provider()));
    let versions: &[&rustls::SupportedProtocolVersion] = match min_version {
        TlsVersion::Tls13 => &[&rustls::version::TLS13],
        TlsVersion::Tls12 => &[&rustls::version::TLS13, &rustls::version::TLS12],
    };
    Ok(ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(versions)?
        .with_no_client_auth()
        .with_single_cert(chain, key)?)
}

/// Читает и проверяет файлы секции. Блокирующий I/O.
fn read_config(cfg: &TlsConfig) -> Result<(ServerConfig, Stamp), TlsError> {
    // Отметка снимается до чтения: изменение во время чтения будет замечено следующим опросом.
    let stamp = stamp(cfg);
    let chain = read_chain(&cfg.cert_file)?;
    let key = read_key(&cfg.key_file)?;
    Ok((build(chain, key, cfg.min_version)?, stamp))
}

/// Цепочка сертификатов из PEM-файла. Блокирующий I/O.
pub(crate) fn read_chain(path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let io = |source| TlsError::Io {
        path: path.to_owned(),
        source,
    };
    let mut reader = BufReader::new(File::open(path).map_err(io)?);
    let chain = rustls_pemfile::certs(&mut reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(io)?;
    if chain.is_empty() {
        return Err(TlsError::NoCert(path.to_owned()));
    }
    Ok(chain)
}

fn read_key(path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let io = |source| TlsError::Io {
        path: path.to_owned(),
        source,
    };
    let mut reader = BufReader::new(File::open(path).map_err(io)?);
    rustls_pemfile::private_key(&mut reader)
        .map_err(io)?
        .ok_or_else(|| TlsError::NoKey(path.to_owned()))
}

fn stamp(cfg: &TlsConfig) -> Stamp {
    let one = |p: &Path| match std::fs::metadata(p) {
        Ok(m) => (m.modified().ok(), m.len()),
        Err(_) => (None, 0),
    };
    [one(&cfg.cert_file), one(&cfg.key_file)]
}

#[cfg(test)]
pub(crate) mod tests {
    use rustls::pki_types::ServerName;
    use rustls::{ClientConfig, RootCertStore};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsConnector;

    use super::*;

    pub(crate) const CERT_A: &str = include_str!("testdata/cert-a.pem");
    pub(crate) const KEY_A: &str = include_str!("testdata/key-a.pem");
    pub(crate) const CERT_B: &str = include_str!("testdata/cert-b.pem");
    pub(crate) const KEY_B: &str = include_str!("testdata/key-b.pem");

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

    pub(crate) fn write_pair(dir: &Path, cert: &str, key: &str) -> TlsConfig {
        let cfg = TlsConfig {
            cert_file: dir.join("cert.pem"),
            key_file: dir.join("key.pem"),
            ..TlsConfig::default()
        };
        std::fs::write(&cfg.cert_file, cert).expect("write cert");
        std::fs::write(&cfg.key_file, key).expect("write key");
        cfg
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
        let cfg = write_pair(&dir.0, CERT_A, KEY_A);
        let certs = TlsCerts::load(&cfg).await.expect("load");
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
                ..cfg
            })
            .await
            .expect("apply");
        let (version, _) = handshake(&certs, tls12_only()).await.expect("tls12");
        assert_eq!(version, rustls::ProtocolVersion::TLSv1_2);
    }

    #[tokio::test]
    async fn files_are_reread_without_restart() {
        let dir = TempDir::new("tls-reload");
        let cfg = write_pair(&dir.0, CERT_A, KEY_A);
        let certs = TlsCerts::load(&cfg).await.expect("load");
        assert!(!certs.reload_if_changed().await.expect("same"));

        // Повреждённый ключ: перечитывание отклонено, действуют прежние сертификаты.
        std::fs::write(&cfg.key_file, "garbage").expect("write");
        assert!(matches!(
            certs.reload_if_changed().await,
            Err(Error::Invalid(_))
        ));
        let both = || connector(&[CERT_A, CERT_B], &[&rustls::version::TLS13]);
        assert_eq!(handshake(&certs, both()).await.expect("old").1, der(CERT_A));

        write_pair(&dir.0, CERT_B, KEY_B);
        assert!(certs.reload_if_changed().await.expect("changed"));
        assert_eq!(handshake(&certs, both()).await.expect("new").1, der(CERT_B));
    }

    #[tokio::test]
    async fn missing_or_mismatched_files_are_invalid() {
        let dir = TempDir::new("tls-invalid");
        let mut cfg = write_pair(&dir.0, CERT_A, KEY_B);
        assert!(matches!(TlsCerts::load(&cfg).await, Err(Error::Invalid(_))));
        cfg.cert_file = dir.0.join("absent.pem");
        let err = TlsCerts::load(&cfg).await.expect_err("absent");
        assert!(err.to_string().contains("absent.pem"), "{err}");
        let empty = dir.0.join("empty.pem");
        std::fs::write(&empty, "").expect("write");
        cfg.cert_file = empty;
        assert!(matches!(TlsCerts::load(&cfg).await, Err(Error::Invalid(_))));
    }

    #[test]
    fn config_from_yaml() {
        let cfg: TlsConfig =
            serde_yaml_ng::from_str("min_version: \"1.2\"\ncert_file: /x/c.pem\n").expect("parse");
        assert_eq!(cfg.min_version, TlsVersion::Tls12);
        assert_eq!(cfg.cert_file, PathBuf::from("/x/c.pem"));
        assert_eq!(cfg.key_file, TlsConfig::default().key_file);
        assert!(serde_yaml_ng::from_str::<TlsConfig>("min_version: \"1.1\"").is_err());
        assert_eq!(cfg.reload_interval, Duration::from_secs(60));
        let off: TlsConfig = serde_yaml_ng::from_str("reload_interval: 0s").expect("0s");
        assert!(off.reload_interval.is_zero());
        assert!(serde_yaml_ng::from_str::<TlsConfig>("reload_interval: 60000").is_err());
        assert!(serde_yaml_ng::from_str::<TlsConfig>("reload_interval_ms: 60000").is_err());
    }
}
