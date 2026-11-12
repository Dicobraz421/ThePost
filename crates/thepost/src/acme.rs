//! Встроенный клиент ACME в `serve` (SRS 6.2, APP-14): при `acme.enabled` узел открывает слушатель проверок
//! HTTP-01 (`http.acme.listen`, по умолчанию порт 80; только `/.well-known/acme-challenge/{token}`, остальное —
//! 404) и запускает [`Acme::run`] — продление сертификата в файлах секции `tls` с перечитыванием без перезапуска.
//!
//! Сертификата ещё нет (первый запуск) — он выпускается до загрузки TLS: служебный HTTP и протоколы без него не
//! стартуют. Сертификат есть — узел стартует сразу, продление (в том числе после смены `acme.domains`) — в фоне.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::task::JoinHandle;
use tp_contracts::role::CancellationToken;
use tp_contracts::secrets::SecretStore;
use tp_contracts::{Error, Result};
use tp_platform::acme::challenge_router;
use tp_platform::{Acme, AcmeConfig, Config, Http01Challenges, TlsCerts, TlsConfig};

use crate::protocols::Sockets;
use crate::sections::{HTTP, HttpSection};

/// Имя слушателя проверок HTTP-01 в реестре адресов [`Bound`](crate::protocols::Bound).
pub const ACME_HTTP: &str = "acme-http01";

/// Клиент ACME узла: секция `acme`, ответы на проверки и слушатель HTTP-01.
pub struct NodeAcme {
    cfg: AcmeConfig,
    tls: TlsConfig,
    challenges: Http01Challenges,
    /// Хранилище секретов узла: ключ TLS (`tls.key`) пишется и читается в нём.
    keys: Arc<dyn SecretStore>,
    /// Слушатель проверок HTTP-01.
    pub listener: JoinHandle<()>,
}

/// При `acme.enabled` — слушатель проверок HTTP-01 до сигнала `shutdown` (сокет systemd из `sockets` или новый; адрес —
/// в реестре `sockets` под [`ACME_HTTP`]) и,
/// если файла сертификата `tls.cert_file` нет, первый выпуск сертификата (ключ — в секрет `tls.key` хранилища `keys`);
/// иначе `None`.
pub async fn prepare(
    cfg: &Config,
    keys: Arc<dyn SecretStore>,
    sockets: &Sockets,
    shutdown: CancellationToken,
) -> Result<Option<NodeAcme>> {
    let acme = cfg.section::<AcmeConfig>("acme")?;
    if !acme.enabled {
        return Ok(None);
    }
    let http = cfg.section::<HttpSection>(HTTP)?;
    let tls = (*cfg.section::<TlsConfig>("tls")?).clone();
    let challenges = Http01Challenges::default();
    let (addr, listener) =
        serve_challenges(sockets, http.acme.listen, challenges.clone(), shutdown)?;
    sockets.bound().set(ACME_HTTP, vec![addr]);
    let node = NodeAcme {
        cfg: (*acme).clone(),
        tls,
        challenges,
        keys,
        listener,
    };
    let exists = tokio::fs::metadata(&node.tls.cert_file)
        .await
        .is_ok_and(|m| m.is_file());
    if !exists {
        tracing::info!(domains = ?node.cfg.domains, "acme: сертификата нет — выпуск до запуска TLS");
        if let Err(e) = node.client(None).issue().await {
            node.listener.abort();
            return Err(match e {
                Error::Unavailable { .. } => Error::Invalid(format!(
                    "acme: сертификат для {:?} не выпущен (сервер ACME {} недоступен или проверка HTTP-01 на {addr} не пройдена; подробности — в журнале)",
                    node.cfg.domains, node.cfg.directory_url
                )),
                other => crate::infra::context("acme", other),
            });
        }
    }
    Ok(Some(node))
}

impl std::fmt::Debug for NodeAcme {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NodeAcme")
            .field("cfg", &self.cfg)
            .field("tls", &self.tls)
            .finish_non_exhaustive()
    }
}

impl NodeAcme {
    fn client(&self, certs: Option<Arc<TlsCerts>>) -> Acme {
        Acme::new(
            self.cfg.clone(),
            self.tls.clone(),
            self.challenges.clone(),
            certs,
            self.keys.clone(),
        )
    }

    /// Продление сертификата (`acme.check_interval`) с перечитыванием `certs` до сигнала `shutdown`.
    pub fn spawn(
        &self,
        certs: Option<Arc<TlsCerts>>,
        shutdown: CancellationToken,
    ) -> JoinHandle<()> {
        tokio::spawn(self.client(certs).run(shutdown))
    }
}

/// Открытый HTTP на `listen` только с маршрутом проверок HTTP-01; фактический адрес и задача сервера.
fn serve_challenges(
    sockets: &Sockets,
    listen: SocketAddr,
    challenges: Http01Challenges,
    shutdown: CancellationToken,
) -> Result<(SocketAddr, JoinHandle<()>)> {
    let listener = sockets
        .take_or(listen, || {
            tp_sys::bind_tcp(listen, &tp_sys::ListenerConfig::default())
                .map_err(|e| Error::Invalid(format!("http.acme.listen {listen}: {e}")))
        })
        .map_err(|e| crate::infra::context("http.acme.listen", e))?;
    let addr = listener
        .local_addr()
        .map_err(|e| Error::Internal(format!("http.acme: {e}")))?;
    let router = challenge_router(challenges);
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, router)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
        {
            tracing::error!(error = %e, "acme: слушатель HTTP-01 остановлен");
        }
    });
    Ok((addr, task))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sections::registry;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tp_testkit::MemSecretStore;

    fn keys() -> Arc<dyn SecretStore> {
        Arc::new(MemSecretStore::default())
    }

    fn config(yaml: &str) -> Config {
        // В сборке оба провайдера rustls: клиенту ACME (instant-acme) нужен процессный.
        let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        registry()
            .expect("registry")
            .parse(yaml, Vec::new())
            .expect("config")
    }

    async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
        let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .expect("write");
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.expect("read");
        let text = String::from_utf8_lossy(&buf).into_owned();
        let code = text
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let body = text
            .split_once("\r\n\r\n")
            .map(|(_, b)| b.to_owned())
            .unwrap_or_default();
        (code, body)
    }

    const BASE: &str = "domain: example.org\nprofile: dev\ndatabase:\n  url: postgres://x@y/z?sslmode=disable\n  \
                        tls: { allow_plaintext: true }\nblob:\n  url: file:///tmp/b\n";

    #[tokio::test]
    async fn disabled_without_section() {
        let sockets = Sockets::default();
        let acme = prepare(&config(BASE), keys(), &sockets, CancellationToken::new())
            .await
            .expect("prepare");
        assert!(acme.is_none());
        assert!(sockets.bound().get(ACME_HTTP).is_empty());
    }

    /// Секция `acme` с готовым сертификатом: слушатель HTTP-01 открыт, отвечает только на
    /// `/.well-known/acme-challenge/*` (неизвестный токен и другие пути — 404), продление — в фоне.
    #[tokio::test]
    async fn challenge_listener_with_existing_cert() {
        let dir = tempfile::tempdir().expect("tempdir");
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/testdata");
        let cert = dir.path().join("cert.pem");
        let key = dir.path().join("key.pem");
        std::fs::copy(src.join("cert.pem"), &cert).expect("cert");
        std::fs::copy(src.join("key.pem"), &key).expect("key");
        let path = |p: &std::path::Path| p.display().to_string().replace('\\', "/");
        let cfg = config(&format!(
            "{BASE}tls:\n  cert_file: {}\n  key_file: {}\n\
             acme:\n  enabled: true\n  domains: [mail.example.org]\n  state_dir: {}\n  \
             directory_url: https://127.0.0.1:1/directory\n  retry_interval: 1h\n\
             http:\n  acme: {{ listen: 127.0.0.1:0 }}\n",
            path(&cert),
            path(&key),
            path(&dir.path().join("acme")),
        ));
        let sockets = Sockets::default();
        let shutdown = CancellationToken::new();
        let acme = prepare(&cfg, keys(), &sockets, shutdown.clone())
            .await
            .expect("prepare")
            .expect("enabled");
        let addr = sockets.bound().get(ACME_HTTP)[0];
        // Каталог ACME недоступен: продление не удаётся, слушатель и узел продолжают работу.
        let renew = acme.spawn(None, shutdown.clone());
        assert_eq!(
            get(addr, "/.well-known/acme-challenge/unknown").await.0,
            404
        );
        assert_eq!(get(addr, "/metrics").await.0, 404);
        assert_eq!(get(addr, "/").await.0, 404);
        assert!(!renew.is_finished());
        shutdown.cancel();
        renew.await.expect("renew");
        acme.listener.await.expect("listener");
    }

    /// Первый запуск без сертификата: выпуск не удался (каталог ACME недоступен) — ошибка запуска с адресом
    /// каталога, слушатель HTTP-01 закрыт.
    #[tokio::test]
    async fn first_issue_failure_stops_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = |p: &std::path::Path| p.display().to_string().replace('\\', "/");
        let cfg = config(&format!(
            "{BASE}tls:\n  cert_file: {}\n  key_file: {}\n  key: {{ secret: \"thepost/tls#key\" }}\n\
             acme:\n  enabled: true\n  domains: [mail.example.org]\n  state_dir: {}\n  \
             directory_url: https://127.0.0.1:1/directory\n\
             http:\n  acme: {{ listen: 127.0.0.1:0 }}\n",
            path(&dir.path().join("cert.pem")),
            path(&dir.path().join("key.pem")),
            path(&dir.path().join("acme")),
        ));
        let sockets = Sockets::default();
        let e = prepare(&cfg, keys(), &sockets, CancellationToken::new())
            .await
            .expect_err("no acme server")
            .to_string();
        assert!(e.contains("acme") && e.contains("127.0.0.1:1"), "{e}");
    }
}
