//! Тесты клиента OpenBao/Vault: разбор секции, имитация хранилища по HTTPS (вход, KV v2, аренды, ошибки),
//! общий набор `tp_testkit::suite::secret_store`, секреты в конфигурации и перечитывании; интеграционный тест
//! с OpenBao — при `TEST_OPENBAO_URL` (dev-режим: `http://localhost:8200`, `TEST_OPENBAO_TOKEN` — root-токен;
//! с TLS — `TEST_OPENBAO_CACERT`), иначе пропуск.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use rustls::RootCertStore;
use rustls::server::WebPkiClientVerifier;
use serde_json::{Map, Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tp_contracts::role::CancellationToken;
use tp_contracts::secrets::{SecretRef, SecretStore};
use tp_contracts::tls::Profile;
use tp_contracts::{Error, TenantId};
use tp_testkit::MemSecretStore;
use tp_testkit::suite::{RealTime, SecretFixture, secret_store};

use super::*;
use crate::config::{CheckMode, SectionRegistry};
use crate::reload::Reloader;
use crate::tls::tests::{CERT_A, KEY_A, TempDir, der};

const CERT_CLIENT: &str = include_str!("../testdata/cert-client.pem");
const KEY_CLIENT: &str = include_str!("../testdata/key-client.pem");

fn r(s: &str) -> SecretRef {
    s.parse().expect("ref")
}

// ---------- секция ----------

#[test]
fn section_from_yaml_and_checks() {
    let cfg: SecretsConfig = serde_yaml_ng::from_str(
        "url: https://openbao-tls:8200\ntls:\n  ca_file: /etc/thepost/tls/ca.pem\nauth:\n  approle:\n    role_id_file: /etc/thepost/openbao/role_id\n    secret_id_file: /etc/thepost/openbao/secret_id\nrequest_timeout: 3s\n",
    )
    .expect("parse");
    assert_eq!(cfg.request_timeout, Duration::from_secs(3));
    assert_eq!(cfg.dynamic_mounts, ["database"]);
    let approle = cfg.auth.approle.as_ref().expect("approle");
    assert_eq!(approle.mount, "approle");
    assert!(cfg.check(Profile::Prod).is_ok());

    let k8s: SecretsConfig =
        serde_yaml_ng::from_str("url: https://openbao:8200\nauth: { kubernetes: {} }\n")
            .expect("k8s");
    let kube = k8s.auth.kubernetes.as_ref().expect("kubernetes");
    assert_eq!(
        (kube.mount.as_str(), kube.role.as_str()),
        ("kubernetes", "thepost")
    );
    assert!(k8s.check(Profile::Prod).is_ok());
    assert!(
        SecretsConfig::default().check(Profile::Prod).is_ok(),
        "без url"
    );

    let err = |yaml: &str, profile| {
        let cfg: SecretsConfig = serde_yaml_ng::from_str(yaml).expect("yaml");
        match cfg.check(profile) {
            Err(Error::Invalid(m)) => m,
            other => panic!("{yaml}: {other:?}"),
        }
    };
    let token = "auth: { token: { token_file: /t } }\n";
    assert!(err(&format!("url: http://bao:8200\n{token}"), Profile::Prod).contains("TLS only"));
    assert!(
        err(&format!("url: http://bao:8200\n{token}"), Profile::Dev).contains("allow_plaintext")
    );
    let dev: SecretsConfig = serde_yaml_ng::from_str(&format!(
        "url: http://bao:8200\ntls: {{ allow_plaintext: true }}\n{token}"
    ))
    .expect("dev");
    assert!(dev.check(Profile::Dev).is_ok());
    assert!(dev.check(Profile::Prod).is_err());
    assert!(err("url: https://bao\n", Profile::Prod).contains("exactly one method"));
    assert!(
        err(
            "url: https://bao\nauth: { kubernetes: {}, token: {} }\n",
            Profile::Prod
        )
        .contains("exactly one method")
    );
    assert!(
        err(
            "url: https://bao\nauth: { approle: { role_id: x, role_id_file: /r } }\n",
            Profile::Prod
        )
        .contains("role_id")
    );
    assert!(err("url: https://u:p@bao\n", Profile::Prod).contains("credentials"));
    assert!(err(&format!("url: https://bao/v1\n{token}"), Profile::Prod).contains("path"));
    assert!(
        err(
            &format!("url: https://bao\ndynamic_mounts: [a/b]\n{token}"),
            Profile::Prod
        )
        .contains("dynamic_mounts")
    );
    assert!(
        serde_yaml_ng::from_str::<SecretsConfig>("request_timeout: 10").is_err(),
        "длительность без единицы"
    );
    assert!(serde_yaml_ng::from_str::<SecretsConfig>("auth: { ldap: {} }").is_err());
}

#[test]
fn url_forms() {
    for (url, base, tls) in [
        (
            "https://openbao.example.ru:8200",
            "https://openbao.example.ru:8200/",
            true,
        ),
        ("https://vault/", "https://vault/", true),
        ("http://[::1]:8200", "http://[::1]:8200/", false),
    ] {
        let u = parse_url(url).expect(url);
        assert_eq!((u.as_str(), is_tls(&u)), (base, tls));
    }
    for bad in [
        "openbao:8200",
        "ftp://x",
        "https://",
        "https://h:port",
        "https://h:70000",
        "https://u:p@h:8200",
        "https://h/v1",
        "https://h?x=1",
        "https://h#f",
        "https://[::1",
    ] {
        assert!(parse_url(bad).is_err(), "{bad}");
    }
}

// ---------- имитация хранилища ----------

#[derive(Default)]
struct MockState {
    /// `<mount>/<путь>` → поля KV v2.
    kv: HashMap<String, Map<String, Value>>,
    /// Путь динамического секрета → поля, срок аренды (с), продлеваемость.
    dynamic: HashMap<String, (Map<String, Value>, u64, bool)>,
    leases: HashMap<String, (Instant, u64)>,
    tokens: HashMap<String, Instant>,
    forbidden: Vec<String>,
    token_ttl: u64,
    approle: (String, String),
    kube: (String, String),
    seal_type: String,
    sealed: bool,
    chunked: bool,
    issued: u64,
    logins: u32,
    renewals: u32,
    cert_logins: u32,
}

impl MockState {
    fn issue_token(&mut self) -> (u16, Value) {
        self.issued += 1;
        self.logins += 1;
        let token = format!("s.mock{}", self.issued);
        self.tokens.insert(
            token.clone(),
            Instant::now() + Duration::from_secs(self.token_ttl),
        );
        (
            200,
            json!({ "auth": { "client_token": token, "lease_duration": self.token_ttl, "renewable": true } }),
        )
    }

    fn route(
        &mut self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: &Value,
        client_cert: bool,
    ) -> (u16, Value) {
        let denied = (403, json!({ "errors": ["permission denied"] }));
        if self.sealed {
            return (503, json!({ "errors": ["Vault is sealed"] }));
        }
        let now = Instant::now();
        match (method, path) {
            ("GET", "/v1/sys/health?standbyok=true&perfstandbyok=true") => {
                return (
                    200,
                    json!({ "initialized": true, "sealed": false, "version": "2.7.2" }),
                );
            }
            ("GET", "/v1/sys/seal-status") => {
                return (200, json!({ "type": self.seal_type, "sealed": false }));
            }
            ("POST", "/v1/auth/approle/login") => {
                let ok = body["role_id"] == self.approle.0.as_str()
                    && body["secret_id"] == self.approle.1.as_str();
                return if ok {
                    self.issue_token()
                } else {
                    (400, json!({ "errors": ["invalid role or secret ID"] }))
                };
            }
            ("POST", "/v1/auth/kubernetes/login") => {
                let ok =
                    body["role"] == self.kube.0.as_str() && body["jwt"] == self.kube.1.as_str();
                return if ok { self.issue_token() } else { denied };
            }
            ("POST", "/v1/auth/cert/login") => {
                return if client_cert {
                    self.cert_logins += 1;
                    self.issue_token()
                } else {
                    (
                        400,
                        json!({ "errors": ["client certificate must be supplied"] }),
                    )
                };
            }
            _ => {}
        }
        let Some(token) = token.filter(|t| self.tokens.get(*t).is_some_and(|e| *e > now)) else {
            return denied;
        };
        match (method, path) {
            ("GET", "/v1/auth/token/lookup-self") => {
                let left = self.tokens[token].saturating_duration_since(now).as_secs();
                (200, json!({ "data": { "ttl": left, "renewable": true } }))
            }
            ("POST", "/v1/auth/token/renew-self") => {
                self.renewals += 1;
                self.tokens
                    .insert(token.to_owned(), now + Duration::from_secs(self.token_ttl));
                (
                    200,
                    json!({ "auth": { "client_token": token, "lease_duration": self.token_ttl, "renewable": true } }),
                )
            }
            ("PUT", "/v1/sys/leases/renew") => {
                let id = body["lease_id"].as_str().unwrap_or_default().to_owned();
                match self.leases.get(&id).copied() {
                    Some((expires, ttl)) if expires > now => {
                        self.leases
                            .insert(id.clone(), (now + Duration::from_secs(ttl), ttl));
                        (
                            200,
                            json!({ "lease_id": id, "lease_duration": ttl, "renewable": true }),
                        )
                    }
                    _ => (400, json!({ "errors": ["lease not found"] })),
                }
            }
            ("GET", p) if p.starts_with("/v1/") => {
                let rest = &p[4..];
                if self.forbidden.iter().any(|f| rest.starts_with(f.as_str())) {
                    return denied;
                }
                if let Some((mount, sub)) = rest.split_once("/data/")
                    && let Some(fields) = self.kv.get(&format!("{mount}/{sub}"))
                {
                    return (200, json!({ "data": { "data": fields, "metadata": {} } }));
                }
                if let Some((fields, ttl, renewable)) = self.dynamic.get(rest).cloned() {
                    self.issued += 1;
                    let id = format!("{rest}/{}", self.issued);
                    self.leases
                        .insert(id.clone(), (now + Duration::from_secs(ttl), ttl));
                    return (
                        200,
                        json!({ "lease_id": id, "lease_duration": ttl, "renewable": renewable, "data": fields }),
                    );
                }
                (404, json!({ "errors": [] }))
            }
            _ => (405, json!({ "errors": ["unsupported"] })),
        }
    }
}

/// Хранилище по HTTPS на 127.0.0.1 (сертификат `localhost`, клиентский сертификат — по желанию клиента).
struct Mock {
    addr: SocketAddr,
    state: Arc<StdMutex<MockState>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Mock {
    async fn start() -> Self {
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let mut roots = RootCertStore::empty();
        roots.add(der(CERT_CLIENT)).expect("client root");
        let verifier =
            WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
                .allow_unauthenticated()
                .build()
                .expect("verifier");
        let key = rustls_pemfile::private_key(&mut KEY_A.as_bytes())
            .expect("pem")
            .expect("key");
        let server = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_client_cert_verifier(verifier)
            .with_single_cert(vec![der(CERT_A)], key)
            .expect("server config");
        let acceptor = TlsAcceptor::from(Arc::new(server));
        // `localhost` может разрешиться и в ::1: слушаем оба адреса (IPv6 — если есть), иначе клиент ждёт
        // отказа соединения по ::1 (в Windows — секунды).
        let v4 = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = v4.local_addr().expect("addr");
        let v6 = TcpListener::bind(("::1", addr.port())).await.ok();
        let state = Arc::new(StdMutex::new(MockState {
            token_ttl: 3600,
            approle: ("role-1".into(), "secret-1".into()),
            kube: ("thepost".into(), "jwt-1".into()),
            seal_type: "static".into(),
            ..MockState::default()
        }));
        let shared = state.clone();
        let task = tokio::spawn(async move {
            loop {
                let accepted = match &v6 {
                    Some(v6) => tokio::select! {
                        a = v4.accept() => a,
                        a = v6.accept() => a,
                    },
                    None => v4.accept().await,
                };
                let Ok((tcp, _)) = accepted else {
                    return;
                };
                let (acceptor, state) = (acceptor.clone(), shared.clone());
                tokio::spawn(async move {
                    let Ok(mut tls) = acceptor.accept(tcp).await else {
                        return;
                    };
                    let client_cert = tls.get_ref().1.peer_certificates().is_some();
                    let _ = serve(&mut tls, &state, client_cert).await;
                });
            }
        });
        Self { addr, state, task }
    }

    fn with<T>(&self, f: impl FnOnce(&mut MockState) -> T) -> T {
        f(&mut self.state.lock().expect("mock state"))
    }

    fn config(&self, dir: &TempDir, auth: SecretsAuthConfig) -> SecretsConfig {
        let ca_file = dir.0.join("ca.pem");
        std::fs::write(&ca_file, CERT_A).expect("ca");
        SecretsConfig {
            url: Some(format!("https://localhost:{}", self.addr.port())),
            tls: SecretsTlsConfig {
                ca_file,
                allow_plaintext: false,
            },
            auth,
            ..SecretsConfig::default()
        }
    }
}

async fn serve<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    s: &mut S,
    state: &StdMutex<MockState>,
    client_cert: bool,
) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i;
        }
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or_default().split(' ');
    let (method, path) = (
        first.next().unwrap_or_default().to_owned(),
        first.next().unwrap_or_default().to_owned(),
    );
    let mut token = None;
    let mut len = 0usize;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            match k.trim().to_ascii_lowercase().as_str() {
                "x-vault-token" => token = Some(v.trim().to_owned()),
                "content-length" => len = v.trim().parse().unwrap_or(0),
                _ => {}
            }
        }
    }
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < len {
        let n = s.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let (status, chunked, reply) = {
        let mut st = state.lock().expect("mock state");
        let (status, reply) = st.route(&method, &path, token.as_deref(), &body, client_cert);
        (status, st.chunked, reply)
    };
    let bytes = serde_json::to_vec(&reply).unwrap_or_default();
    let mut out = format!(
        "HTTP/1.1 {status} Mock\r\nConnection: close\r\nContent-Type: application/json\r\n"
    )
    .into_bytes();
    if status == 503 {
        out.extend_from_slice(b"Retry-After: 7\r\n");
    }
    if chunked {
        out.extend_from_slice(b"Transfer-Encoding: chunked\r\n\r\n");
        for part in bytes.chunks(7) {
            out.extend_from_slice(format!("{:x}\r\n", part.len()).as_bytes());
            out.extend_from_slice(part);
            out.extend_from_slice(b"\r\n");
        }
        out.extend_from_slice(b"0\r\n\r\n");
    } else {
        out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes());
        out.extend_from_slice(&bytes);
    }
    s.write_all(&out).await?;
    s.shutdown().await
}

fn approle(dir: &TempDir, secret_id: &str) -> SecretsAuthConfig {
    let file = dir.0.join("secret_id");
    std::fs::write(&file, format!("{secret_id}\n")).expect("secret_id");
    SecretsAuthConfig {
        approle: Some(AppRoleAuth {
            role_id: Some("role-1".into()),
            secret_id_file: file,
            ..AppRoleAuth::default()
        }),
        ..SecretsAuthConfig::default()
    }
}

fn fields(pairs: &[(&str, &str)]) -> Map<String, Value> {
    pairs
        .iter()
        .map(|(k, v)| ((*k).to_owned(), Value::from(*v)))
        .collect()
}

// ---------- клиент ----------

#[tokio::test]
async fn approle_store_passes_contract_suite() {
    let mock = Mock::start().await;
    mock.with(|st| {
        st.kv.insert(
            "thepost/db".into(),
            fields(&[("password", "pw"), ("username", "thepost")]),
        );
        st.kv
            .insert("thepost/admin".into(), fields(&[("token", "root")]));
        st.forbidden.push("thepost/data/admin".into());
        st.dynamic.insert(
            "database/creds/thepost".into(),
            (fields(&[("username", "v-1"), ("password", "dyn")]), 1, true),
        );
        st.chunked = true;
    });
    let dir = TempDir::new("vault-suite");
    let store = VaultStore::new(&mock.config(&dir, approle(&dir, "secret-1")), Profile::Prod)
        .await
        .expect("store");
    let fixture = SecretFixture {
        present: (r("thepost/db#password"), "pw".into()),
        missing_field: r("thepost/db#nope"),
        missing_path: r("thepost/none#password"),
        forbidden: Some(r("thepost/admin#token")),
        dynamic: Some(r("database/creds/thepost#password")),
    };
    secret_store(&store, &fixture, &RealTime).await;
    assert_eq!(
        store.get(&r("thepost#password")).await.map(|_| ()),
        Err(Error::NotFound),
        "KV v2 без пути"
    );
    // Один вход на весь набор: отказ политики (403) — один повторный вход на запрос.
    let logins = mock.with(|st| st.logins);
    assert!((1..=3).contains(&logins), "logins {logins}");
    let shown = format!("{store:?}");
    assert!(
        shown.contains("approle") && !shown.contains("secret-1"),
        "{shown}"
    );
}

#[tokio::test]
async fn dynamic_fields_share_one_lease() {
    let mock = Mock::start().await;
    mock.with(|st| {
        st.dynamic.insert(
            "database/creds/ro".into(),
            (
                fields(&[("username", "v-ro"), ("password", "p-ro")]),
                60,
                true,
            ),
        );
    });
    let dir = TempDir::new("vault-dynamic");
    let store = VaultStore::new(&mock.config(&dir, approle(&dir, "secret-1")), Profile::Prod)
        .await
        .expect("store");
    let user = store
        .get(&r("database/creds/ro#username"))
        .await
        .expect("user");
    let pass = store
        .get(&r("database/creds/ro#password"))
        .await
        .expect("pass");
    assert_eq!((user.expose(), pass.expose()), ("v-ro", "p-ro"));
    let lease = user.lease().cloned().expect("lease");
    assert_eq!(pass.lease(), Some(&lease));
    assert_eq!(lease.ttl, Duration::from_secs(60));
    assert_eq!(mock.with(|st| st.issued), 2, "один вход и одна выдача");
    assert_eq!(
        store.get(&r("database/creds/ro#nope")).await.map(|_| ()),
        Err(Error::NotFound)
    );

    // Отозванная аренда: продление — NotFound, следующее чтение — новая выдача.
    mock.with(|st| st.leases.clear());
    assert_eq!(store.renew(&lease).await, Err(Error::NotFound));
    let again = store
        .get(&r("database/creds/ro#password"))
        .await
        .expect("again");
    assert_ne!(again.lease().map(|l| &l.id), Some(&lease.id));
    let fixed = SecretLease {
        renewable: false,
        ..lease
    };
    assert_eq!(store.renew(&fixed).await, Err(Error::NotFound));
}

#[tokio::test]
async fn node_token_is_renewed_and_relogged() {
    let mock = Mock::start().await;
    mock.with(|st| {
        st.token_ttl = 1;
        st.kv
            .insert("thepost/db".into(), fields(&[("password", "pw")]));
    });
    let dir = TempDir::new("vault-token");
    let store = VaultStore::new(&mock.config(&dir, approle(&dir, "secret-1")), Profile::Prod)
        .await
        .expect("store");
    let db = r("thepost/db#password");
    store.get(&db).await.expect("first");
    tokio::time::sleep(Duration::from_millis(750)).await;
    store.get(&db).await.expect("after 2/3 ttl");
    assert_eq!(mock.with(|st| (st.logins, st.renewals)), (1, 1));

    // Токен отозван: 403 → новый вход и повтор.
    mock.with(|st| st.tokens.clear());
    store.get(&db).await.expect("after revoke");
    assert_eq!(mock.with(|st| st.logins), 2);

    // Неверный secret_id: вход отклонён — Invalid (ошибка настройки), значение — не в сообщении.
    std::fs::write(dir.0.join("secret_id"), "wrong").expect("write");
    mock.with(|st| st.tokens.clear());
    let e = store.get(&db).await.expect_err("bad secret_id");
    assert!(
        matches!(&e, Error::Invalid(m) if m.contains("invalid role or secret ID")),
        "{e:?}"
    );
    assert!(!e.to_string().contains("wrong"));
    std::fs::remove_file(dir.0.join("secret_id")).expect("rm");
    assert!(
        matches!(store.get(&db).await, Err(Error::Invalid(_))),
        "нет файла"
    );
}

#[tokio::test]
async fn kubernetes_cert_and_token_login() {
    let mock = Mock::start().await;
    mock.with(|st| {
        st.kv.insert(
            "thepost/s3".into(),
            fields(&[("secret_access_key", "s3key")]),
        );
    });
    let dir = TempDir::new("vault-methods");
    let s3 = r("thepost/s3#secret_access_key");

    let jwt_file = dir.0.join("jwt");
    std::fs::write(&jwt_file, "jwt-1").expect("jwt");
    let kube = SecretsAuthConfig {
        kubernetes: Some(KubernetesAuth {
            jwt_file,
            ..KubernetesAuth::default()
        }),
        ..SecretsAuthConfig::default()
    };
    let store = VaultStore::new(&mock.config(&dir, kube), Profile::Prod)
        .await
        .expect("kube");
    assert_eq!(store.get(&s3).await.expect("kube").expose(), "s3key");

    let (cert_file, key_file) = (dir.0.join("node.pem"), dir.0.join("node.key"));
    std::fs::write(&cert_file, CERT_CLIENT).expect("cert");
    std::fs::write(&key_file, KEY_CLIENT).expect("key");
    let cert = SecretsAuthConfig {
        cert: Some(CertAuth {
            cert_file,
            key_file,
            ..CertAuth::default()
        }),
        ..SecretsAuthConfig::default()
    };
    let store = VaultStore::new(&mock.config(&dir, cert), Profile::Prod)
        .await
        .expect("cert");
    assert_eq!(store.get(&s3).await.expect("cert").expose(), "s3key");
    assert_eq!(
        mock.with(|st| st.cert_logins),
        1,
        "вход по сертификату узла"
    );

    let token_file = dir.0.join("token");
    std::fs::write(&token_file, "s.unknown").expect("token");
    let token = SecretsAuthConfig {
        token: Some(TokenAuth {
            token_file: token_file.clone(),
        }),
        ..SecretsAuthConfig::default()
    };
    let store = VaultStore::new(&mock.config(&dir, token), Profile::Prod)
        .await
        .expect("token");
    assert_eq!(store.get(&s3).await.map(|_| ()), Err(Error::Forbidden));
    let issued = mock.with(|st| {
        st.tokens
            .insert("s.agent".into(), Instant::now() + Duration::from_secs(600));
        st.tokens.len()
    });
    assert!(issued >= 3);
    std::fs::write(&token_file, "s.agent\n").expect("token");
    assert_eq!(store.get(&s3).await.expect("token").expose(), "s3key");
}

#[tokio::test]
async fn unavailable_tls_and_plaintext_rules() {
    let mock = Mock::start().await;
    mock.with(|st| {
        st.kv
            .insert("thepost/db".into(), fields(&[("password", "pw")]))
    });
    let dir = TempDir::new("vault-errors");
    let cfg = mock.config(&dir, approle(&dir, "secret-1"));
    let store = VaultStore::new(&cfg, Profile::Prod).await.expect("store");
    let db = r("thepost/db#password");
    mock.with(|st| st.sealed = true);
    assert_eq!(
        store.get(&db).await.map(|_| ()),
        Err(Error::Unavailable {
            retry_after: Some(Duration::from_secs(7))
        })
    );
    mock.with(|st| st.sealed = false);
    assert!(store.get(&db).await.is_ok());

    // Недоверенный сертификат хранилища и закрытый порт — Unavailable; канал без TLS вне dev — Invalid.
    let other_ca = dir.0.join("other-ca.pem");
    std::fs::write(&other_ca, crate::tls::tests::CERT_B).expect("ca");
    let untrusted = SecretsConfig {
        tls: SecretsTlsConfig {
            ca_file: other_ca,
            allow_plaintext: false,
        },
        ..cfg.clone()
    };
    let store = VaultStore::new(&untrusted, Profile::Prod)
        .await
        .expect("store");
    assert!(matches!(
        store.get(&db).await,
        Err(Error::Unavailable { .. })
    ));
    let closed = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = closed.local_addr().expect("addr").port();
    drop(closed);
    let down = SecretsConfig {
        url: Some(format!("https://localhost:{port}")),
        ..cfg.clone()
    };
    let store = VaultStore::new(&down, Profile::Prod).await.expect("store");
    assert!(matches!(
        store.get(&db).await,
        Err(Error::Unavailable { .. })
    ));
    let plain = SecretsConfig {
        url: Some("http://localhost:8200".into()),
        ..cfg.clone()
    };
    assert!(matches!(
        VaultStore::new(&plain, Profile::Prod).await,
        Err(Error::Invalid(_))
    ));
    let no_ca = SecretsConfig {
        tls: SecretsTlsConfig {
            ca_file: dir.0.join("absent.pem"),
            allow_plaintext: false,
        },
        ..cfg
    };
    let e = VaultStore::new(&no_ca, Profile::Prod)
        .await
        .expect_err("ca");
    assert!(e.to_string().contains("absent.pem"), "{e}");
}

#[tokio::test]
async fn doctor_reports_store_state() {
    let mock = Mock::start().await;
    let dir = TempDir::new("vault-doctor");
    let cfg = mock.config(&dir, approle(&dir, "secret-1"));
    let report = doctor(&cfg, Profile::Prod).await;
    let status = |id: &str| report.get(id).map(|c| c.status);
    assert_eq!(status("secrets_config"), Some(DoctorStatus::Ok));
    assert_eq!(status("secrets_store"), Some(DoctorStatus::Ok));
    assert_eq!(
        report.get("secrets_store").map(|c| c.value.as_str()),
        Some("2.7.2")
    );
    assert_eq!(
        status("secrets_seal"),
        Some(DoctorStatus::Warn),
        "ключ распечатывания в файле"
    );
    assert_eq!(status("secrets_login"), Some(DoctorStatus::Ok));
    assert_eq!(status("secrets_token"), Some(DoctorStatus::Ok));
    assert_eq!(report.status(), DoctorStatus::Warn);

    mock.with(|st| st.seal_type = "pkcs11".into());
    let bad = SecretsConfig {
        auth: approle(&dir, "wrong"),
        ..cfg.clone()
    };
    let report = doctor(&bad, Profile::Prod).await;
    assert_eq!(
        report.get("secrets_seal").map(|c| c.status),
        Some(DoctorStatus::Ok)
    );
    assert_eq!(
        report.get("secrets_login").map(|c| c.status),
        Some(DoctorStatus::Fail)
    );
    assert!(report.checks.iter().all(|c| !c.message.contains("wrong")));

    mock.with(|st| st.sealed = true);
    let report = doctor(&cfg, Profile::Prod).await;
    let store = report.get("secrets_store").expect("store");
    assert_eq!(
        (store.status, store.message.as_str()),
        (DoctorStatus::Fail, "store is sealed")
    );
    assert!(report.get("secrets_login").is_none());

    let off = doctor(&SecretsConfig::default(), Profile::Prod).await;
    assert_eq!(
        off.get("secrets_config").map(|c| c.status),
        Some(DoctorStatus::Skipped)
    );
    assert_eq!(off.status(), DoctorStatus::Ok);
    let plain = SecretsConfig {
        url: Some("http://localhost:1".into()),
        ..cfg
    };
    let report = doctor(&plain, Profile::Prod).await;
    assert_eq!(report.status(), DoctorStatus::Fail);
}

// ---------- секреты в конфигурации ----------

const BASE: &str = "blob: { url: file:///var/lib/thepost }\ndomain: example.com\n";

fn parse(src: &str) -> std::result::Result<crate::config::Config, String> {
    SectionRegistry::new()
        .parse_with(src, Vec::new(), CheckMode::Lenient)
        .map_err(|e| e.to_string())
}

#[test]
fn plaintext_secrets_are_rejected_in_both_modes() {
    let src = format!("{BASE}database: {{ url: postgres://db/thepost, password: hunter2 }}\n");
    let e = parse(&src).expect_err("plaintext");
    assert!(e.contains("database.password: secret in plaintext"), "{e}");
    assert!(!e.contains("hunter2"), "{e}");
    let strict = SectionRegistry::new()
        .parse_with(&src, Vec::new(), CheckMode::Strict)
        .expect_err("strict")
        .to_string();
    assert!(strict.contains("database.password"), "{strict}");

    let env = vec![("THEPOST_DATABASE__PASSWORD".to_owned(), "12345".to_owned())];
    let e = SectionRegistry::new()
        .parse_with(
            &format!("{BASE}database: {{ url: postgres://db }}\n"),
            env,
            CheckMode::Lenient,
        )
        .expect_err("env")
        .to_string();
    assert!(e.contains("database.password"), "{e}");

    let url = format!("{BASE}database: {{ url: \"postgres://u:pw@db/thepost\" }}\n");
    let e = parse(&url).expect_err("url password");
    assert!(e.contains("database.url: password in the address"), "{e}");
    assert!(!e.contains("pw@"), "{e}");
    assert!(parse(&format!("profile: dev\n{}", url)).is_ok(), "dev");
    let query = format!("{BASE}database: {{ url: \"postgres://db/x?password=pw\" }}\n");
    assert!(parse(&query).is_err());
    let user_only = format!("{BASE}database: {{ url: \"postgres://thepost@db/x\" }}\n");
    assert!(parse(&user_only).is_ok());
}

#[test]
fn secret_refs_are_collected() {
    let src = format!(
        "{BASE}database:\n  url: postgres://thepost@db/thepost\n  password: {{ secret: \"thepost/db#password\" }}\n  shards:\n    - {{ password: {{ secret: \"thepost/db#password\" }} }}\n    - {{ password: {{ secret: \"shards/s2#password\" }} }}\n"
    );
    let cfg = parse(&src).expect("refs");
    let refs: Vec<String> = cfg.secret_refs().map(ToString::to_string).collect();
    assert_eq!(refs, ["shards/s2#password", "thepost/db#password"]);
    assert_eq!(
        cfg.secret(&r("thepost/db#password")).map(|_| ()),
        Err(Error::NotFound)
    );

    let bad = format!("{BASE}database: {{ url: postgres://db, password: {{ secret: nohash }} }}\n");
    let e = parse(&bad).expect_err("bad ref");
    assert!(
        e.contains("database.password: invalid secret reference"),
        "{e}"
    );
}

#[tokio::test]
async fn resolve_secrets_reads_all_or_nothing() {
    let src = format!(
        "{BASE}database: {{ url: postgres://db, password: {{ secret: \"thepost/db#password\" }} }}\nmodules: {{}}\n"
    );
    let mut cfg = parse(&src).expect("parse");
    let mem = MemSecretStore::default();
    let e = cfg.resolve_secrets(&mem).await.expect_err("missing");
    assert!(
        matches!(&e, Error::Invalid(m) if m.contains("secret thepost/db#password: not found")),
        "{e:?}"
    );
    mem.set(&r("thepost/db#password"), "pw");
    mem.seal();
    assert_eq!(cfg.resolve_secrets(&mem).await, Err(Error::UNAVAILABLE));
    mem.unseal();
    cfg.resolve_secrets(&mem).await.expect("resolve");
    assert_eq!(
        cfg.secret(&r("thepost/db#password")).expect("pw").expose(),
        "pw"
    );
    assert!(!format!("{cfg:?}").contains("pw\""));
}

fn write_config(dir: &TempDir, text: &str) -> std::path::PathBuf {
    let path = dir.0.join("config.yaml");
    std::fs::write(&path, text).expect("write config");
    path
}

#[tokio::test]
async fn reloader_rereads_secrets_and_keeps_previous_on_failure() {
    let dir = TempDir::new("reload-secrets");
    let src = format!(
        "{BASE}database: {{ url: postgres://db, password: {{ secret: \"thepost/db#password\" }} }}\nreload: {{ poll_interval: 0s }}\n"
    );
    let path = write_config(&dir, &src);
    let mem = MemSecretStore::default();
    let db = r("thepost/db#password");
    mem.set(&db, "pw1");
    let registry = Arc::new(SectionRegistry::new());
    let started = Reloader::start_with(registry, path, Vec::new()).expect("start");
    assert_eq!(
        started.current().secret(&db).map(|_| ()),
        Err(Error::NotFound)
    );
    let mut reloader = started
        .with_secret_store(Arc::new(mem.clone()))
        .await
        .expect("with store");
    assert_eq!(reloader.current().secret(&db).expect("pw1").expose(), "pw1");

    // Файл не менялся: SIGHUP после ротации секрета рассылает новую версию, без ротации — ничего.
    assert!(!reloader.reload().await.expect("same"));
    mem.set(&db, "pw2");
    assert!(reloader.reload().await.expect("rotated"));
    assert_eq!(reloader.current().secret(&db).expect("pw2").expose(), "pw2");

    // Хранилище недоступно или секрет удалён — прежняя версия в силе.
    mem.seal();
    assert!(reloader.reload().await.is_err());
    mem.unseal();
    mem.remove(&db);
    assert!(matches!(reloader.reload().await, Err(Error::Invalid(_))));
    assert_eq!(
        reloader.current().secret(&db).expect("kept").expose(),
        "pw2"
    );
}

#[tokio::test]
async fn reloader_renews_leases_and_rereads_expired() {
    let dir = TempDir::new("reload-leases");
    let src = format!(
        "{BASE}database: {{ url: postgres://db, password: {{ secret: \"database/creds/thepost#password\" }} }}\nreload: {{ poll_interval: 0s }}\n"
    );
    let path = write_config(&dir, &src);
    let mem = MemSecretStore::default();
    let dynamic = r("database/creds/thepost#password");
    mem.set_dynamic(&dynamic, "dyn", Duration::from_millis(600), true);
    let reloader = Reloader::start_with(Arc::new(SectionRegistry::new()), path, Vec::new())
        .expect("start")
        .with_secret_store(Arc::new(mem.clone()))
        .await
        .expect("with store");
    let mut rx = reloader.subscribe();
    let first = reloader
        .current()
        .secret(&dynamic)
        .expect("dyn")
        .lease()
        .cloned()
        .expect("lease");
    let shutdown = CancellationToken::new();
    let task = tokio::spawn(reloader.run(shutdown.clone()));

    // Продление после 2/3 срока: аренда жива дольше исходного срока, версия не меняется.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert!(mem.renews() >= 1, "renews {}", mem.renews());
    assert!(mem.lease_active(&first.id));
    assert!(!rx.has_changed().expect("rx"));

    // Отозванная аренда: продление — NotFound, секреты перечитываются с новой арендой.
    mem.revoke(&first.id);
    tokio::time::timeout(Duration::from_secs(5), rx.changed())
        .await
        .expect("new version")
        .expect("changed");
    let next = rx
        .borrow_and_update()
        .secret(&dynamic)
        .expect("dyn")
        .lease()
        .cloned()
        .expect("lease");
    assert_ne!(next.id, first.id);
    shutdown.cancel();
    task.await.expect("run");
}

// ---------- OpenBao ----------

#[tokio::test]
async fn openbao_contract_suite() {
    let Ok(url) = std::env::var("TEST_OPENBAO_URL") else {
        eprintln!("TEST_OPENBAO_URL is not set: OpenBao integration test skipped");
        return;
    };
    let root_token = std::env::var("TEST_OPENBAO_TOKEN").expect("TEST_OPENBAO_TOKEN (root token)");
    let https = is_tls(&parse_url(&url).expect("TEST_OPENBAO_URL"));
    let profile = if https { Profile::Prod } else { Profile::Dev };
    let dir = TempDir::new("openbao");
    let tls = SecretsTlsConfig {
        ca_file: std::env::var("TEST_OPENBAO_CACERT")
            .map(Into::into)
            .unwrap_or_else(|_| SecretsTlsConfig::default().ca_file),
        allow_plaintext: !https,
    };
    let root_file = dir.0.join("root-token");
    std::fs::write(&root_file, &root_token).expect("root token");
    let base = SecretsConfig {
        url: Some(url),
        tls,
        ..SecretsConfig::default()
    };
    let root = VaultStore::new(
        &SecretsConfig {
            auth: SecretsAuthConfig {
                token: Some(TokenAuth {
                    token_file: root_file,
                }),
                ..SecretsAuthConfig::default()
            },
            ..base.clone()
        },
        profile,
    )
    .await
    .expect("root store");
    let admin = |method: &'static str, path: String, body: Option<Value>| {
        let root = root.clone();
        async move {
            match root.call(method, &path, body.as_ref()).await {
                Ok(v) => v,
                // Точка монтирования уже есть (повторный запуск).
                Err(e) if e.status() == Some(400) && path.starts_with("/v1/sys/") => Value::Null,
                Err(e) => panic!("{method} {path}: {e}"),
            }
        }
    };
    let run = TenantId::new().to_string().replace('-', "");
    let mount = "tp-kit";
    let approle_mount = "tp-kit-approle";
    admin(
        "POST",
        format!("/v1/sys/mounts/{mount}"),
        Some(json!({ "type": "kv", "options": { "version": "2" } })),
    )
    .await;
    admin(
        "POST",
        format!("/v1/sys/auth/{approle_mount}"),
        Some(json!({ "type": "approle" })),
    )
    .await;
    admin(
        "POST",
        format!("/v1/{mount}/data/{run}/db"),
        Some(json!({ "data": { "password": format!("pw-{run}"), "username": "thepost" } })),
    )
    .await;
    let policy = format!("tp-kit-{run}");
    let rules = format!(
        "path \"{mount}/data/{run}/*\" {{ capabilities = [\"read\"] }}\npath \"sys/leases/renew\" {{ capabilities = [\"update\"] }}\n"
    );
    admin(
        "PUT",
        format!("/v1/sys/policies/acl/{policy}"),
        Some(json!({ "policy": rules })),
    )
    .await;
    admin(
        "POST",
        format!("/v1/auth/{approle_mount}/role/{run}"),
        Some(json!({ "token_policies": [policy], "token_ttl": "10m", "token_max_ttl": "30m" })),
    )
    .await;
    let role_id = admin(
        "GET",
        format!("/v1/auth/{approle_mount}/role/{run}/role-id"),
        None,
    )
    .await["data"]["role_id"]
        .as_str()
        .expect("role_id")
        .to_owned();
    let secret_id = admin(
        "POST",
        format!("/v1/auth/{approle_mount}/role/{run}/secret-id"),
        Some(json!({})),
    )
    .await["data"]["secret_id"]
        .as_str()
        .expect("secret_id")
        .to_owned();
    let secret_id_file = dir.0.join("secret_id");
    std::fs::write(&secret_id_file, secret_id).expect("secret_id");
    let node_cfg = SecretsConfig {
        auth: SecretsAuthConfig {
            approle: Some(AppRoleAuth {
                mount: approle_mount.into(),
                role_id: Some(role_id),
                role_id_file: None,
                secret_id_file,
            }),
            ..SecretsAuthConfig::default()
        },
        ..base
    };
    let node = VaultStore::new(&node_cfg, profile)
        .await
        .expect("node store");
    let fixture = SecretFixture {
        present: (
            r(&format!("{mount}/{run}/db#password")),
            format!("pw-{run}"),
        ),
        missing_field: r(&format!("{mount}/{run}/db#nope")),
        missing_path: r(&format!("{mount}/{run}/none#password")),
        forbidden: Some(r(&format!("{mount}/{run}-other/db#password"))),
        dynamic: None,
    };
    secret_store(&node, &fixture, &RealTime).await;
    let report = doctor(&node_cfg, profile).await;
    assert_eq!(
        report.get("secrets_login").map(|c| c.status),
        Some(DoctorStatus::Ok),
        "{report:?}"
    );
    assert_ne!(report.status(), DoctorStatus::Fail, "{report:?}");

    admin("DELETE", format!("/v1/{mount}/metadata/{run}/db"), None).await;
    admin(
        "DELETE",
        format!("/v1/auth/{approle_mount}/role/{run}"),
        None,
    )
    .await;
    admin("DELETE", format!("/v1/sys/policies/acl/{policy}"), None).await;
}
