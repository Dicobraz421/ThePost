//! Токены OpenID Connect (SRS 5.1): Bearer для JMAP, DAV, Admin API и SASL OAUTHBEARER / XOAUTH2.
//!
//! Ключи поставщика — JWKS по `jwks_uri` из документа discovery (`<issuer>/.well-known/openid-configuration`)
//! или из конфигурации; набор кэшируется (`moka`) на `jwks_ttl`, неизвестный `kid` перечитывает набор не
//! чаще `jwks_refresh_min` (смена ключей у поставщика). Подпись, `iss`, `aud`, `exp`, `nbf` проверяет
//! `jsonwebtoken`; симметричные алгоритмы (`HS*`) не принимаются. Имя входа — claim `login_claim`
//! (по умолчанию `email`; при `email_verified: false` вход запрещён), области доступа — `scope` / `scp`.
//!
//! ```yaml
//! issuer: https://id.example.com/realms/corp
//! audience: [thepost]
//! login_claim: email
//! jwks_ttl: 10m
//! ```

use std::collections::BTreeSet;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use jsonwebtoken::jwk::JwkSet;
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tp_contracts::auth::{Authenticator, Credentials, Principal};
use tp_contracts::config::duration;
use tp_contracts::tls::{InternalTls, Profile};
use tp_contracts::{Error, Result};

use crate::{Accounts, check_channel, plaintext_warning};

/// Секция поставщика OpenID Connect (`auth.oidc`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OidcConfig {
    /// Издатель (`iss`), `https://`; от него — адрес discovery.
    pub issuer: String,
    /// Допустимые получатели токена (`aud`), не пусто.
    pub audience: Vec<String>,
    /// Адрес JWKS; не задан — из документа discovery.
    pub jwks_uri: Option<String>,
    /// Claim с именем входа учётной записи.
    pub login_claim: String,
    /// Допустимые алгоритмы подписи (`RS256`, `PS256`, `ES256`, `EdDSA`, ...); `HS*` запрещены.
    pub algorithms: Vec<String>,
    /// Допуск расхождения часов для `exp`/`nbf`.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub leeway: Duration,
    /// Срок кэша набора ключей.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub jwks_ttl: Duration,
    /// Наименьший интервал перечитывания набора ключей при неизвестном `kid`.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub jwks_refresh_min: Duration,
    /// Таймаут запроса к поставщику.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub http_timeout: Duration,
    /// Наибольший размер ответа поставщика, байт.
    pub max_response: usize,
    /// TLS: CA поставщика (`ca_file`; не задан — системные корни), сертификат клиента; `allow_plaintext` — dev.
    pub tls: InternalTls,
}

impl Default for OidcConfig {
    fn default() -> Self {
        OidcConfig {
            issuer: String::new(),
            audience: Vec::new(),
            jwks_uri: None,
            login_claim: "email".into(),
            algorithms: ["RS256", "PS256", "ES256", "EdDSA"]
                .map(String::from)
                .into(),
            leeway: Duration::from_secs(60),
            jwks_ttl: Duration::from_secs(600),
            jwks_refresh_min: Duration::from_secs(30),
            http_timeout: Duration::from_secs(5),
            max_response: 1 << 20,
            tls: InternalTls::default(),
        }
    }
}

impl OidcConfig {
    /// Проверяет секцию (`thepost config check`, запуск). `Ok(true)` — канал открытый (только dev).
    pub fn check(&self, profile: Profile) -> Result<bool> {
        let plaintext = self.channel(&self.issuer, profile)?;
        if let Some(uri) = &self.jwks_uri {
            self.channel(uri, profile)?;
        }
        if self.audience.is_empty() {
            return Err(Error::invalid("OIDC: audience is empty"));
        }
        if self.login_claim.is_empty() {
            return Err(Error::invalid("OIDC: login_claim is empty"));
        }
        self.algs()?;
        Ok(plaintext)
    }

    fn channel(&self, url: &str, profile: Profile) -> Result<bool> {
        check_channel("OIDC", url, &["https"], &["http"], &self.tls, profile)
    }

    fn algs(&self) -> Result<Vec<Algorithm>> {
        if self.algorithms.is_empty() {
            return Err(Error::invalid("OIDC: algorithms is empty"));
        }
        self.algorithms
            .iter()
            .map(|a| match a.parse::<Algorithm>() {
                Ok(Algorithm::HS256 | Algorithm::HS384 | Algorithm::HS512) | Err(_) => Err(
                    Error::Invalid(format!("OIDC: algorithm {a} is not allowed")),
                ),
                Ok(alg) => Ok(alg),
            })
            .collect()
    }
}

/// Набор ключей поставщика и момент чтения.
struct Keys {
    set: JwkSet,
    fetched: Instant,
}

/// Документ discovery (OpenID Connect Discovery 1.0): нужные поля.
#[derive(Deserialize)]
struct Discovery {
    issuer: String,
    jwks_uri: String,
}

/// [`Authenticator`] для [`Credentials::Bearer`]: токен поставщика OIDC → включённая учётная запись по claim
/// имени входа, области доступа — из токена. Неверный, просроченный токен — `Forbidden`; поставщик
/// недоступен — `Unavailable`.
#[derive(Clone)]
pub struct OidcAuthenticator {
    cfg: Arc<OidcConfig>,
    algs: Vec<Algorithm>,
    plaintext: bool,
    http: reqwest::Client,
    keys: moka::future::Cache<(), Arc<Keys>>,
    accounts: Accounts,
}

impl fmt::Debug for OidcAuthenticator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OidcAuthenticator")
            .field("issuer", &self.cfg.issuer)
            .field("audience", &self.cfg.audience)
            .field("accounts", &self.accounts)
            .finish_non_exhaustive()
    }
}

impl OidcAuthenticator {
    /// Проверка токенов поставщика `cfg` для учётных записей `accounts`. Ключи читаются при первой проверке.
    pub async fn new(cfg: OidcConfig, profile: Profile, accounts: Accounts) -> Result<Self> {
        let plaintext = cfg.check(profile)?;
        if plaintext {
            tracing::warn!("{}", plaintext_warning("OIDC"));
        }
        let tls = cfg.tls.clone();
        let (roots, identity) = tokio::task::spawn_blocking(move || read_tls(&tls))
            .await
            .map_err(|e| Error::Internal(format!("OIDC: tls: {e}")))??;
        let mut http = reqwest::Client::builder()
            .timeout(cfg.http_timeout)
            .https_only(!plaintext);
        for cert in roots {
            http = http.add_root_certificate(cert);
        }
        if let Some(id) = identity {
            http = http.identity(id);
        }
        let http = http
            .build()
            .map_err(|e| Error::Internal(format!("OIDC: http client: {e}")))?;
        Ok(OidcAuthenticator {
            algs: cfg.algs()?,
            keys: moka::future::Cache::builder()
                .max_capacity(1)
                .time_to_live(cfg.jwks_ttl)
                .build(),
            cfg: Arc::new(cfg),
            plaintext,
            http,
            accounts,
        })
    }

    /// Адрес документа discovery поставщика (`openid-configuration` в ответе-ошибке OAUTHBEARER).
    pub fn discovery_url(&self) -> String {
        format!(
            "{}/.well-known/openid-configuration",
            self.cfg.issuer.trim_end_matches('/')
        )
    }

    async fn get<T: DeserializeOwned>(&self, url: &str) -> Result<T> {
        let fail = |what: &str| {
            tracing::warn!(url, "OIDC provider: {what}");
            Error::Unavailable { retry_after: None }
        };
        let mut resp = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| fail(&e.to_string()))?;
        if !resp.status().is_success() {
            return Err(fail(&format!("HTTP {}", resp.status())));
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| fail(&e.to_string()))? {
            if body.len() + chunk.len() > self.cfg.max_response {
                return Err(fail("response too large"));
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|e| fail(&e.to_string()))
    }

    async fn fetch(&self) -> Result<Arc<Keys>> {
        let uri = match &self.cfg.jwks_uri {
            Some(u) => u.clone(),
            None => {
                let d: Discovery = self.get(&self.discovery_url()).await?;
                if d.issuer != self.cfg.issuer {
                    return Err(Error::Internal(format!(
                        "OIDC: discovery issuer {} does not match {}",
                        d.issuer, self.cfg.issuer
                    )));
                }
                if self.channel_of(&d.jwks_uri)? && !self.plaintext {
                    return Err(Error::internal("OIDC: jwks_uri without TLS"));
                }
                d.jwks_uri
            }
        };
        let set: JwkSet = self.get(&uri).await?;
        Ok(Arc::new(Keys {
            set,
            fetched: Instant::now(),
        }))
    }

    fn channel_of(&self, url: &str) -> Result<bool> {
        let tls = InternalTls {
            allow_plaintext: true,
            ..InternalTls::default()
        };
        check_channel("OIDC", url, &["https"], &["http"], &tls, Profile::Dev)
    }

    async fn keys(&self) -> Result<Arc<Keys>> {
        self.keys
            .try_get_with((), self.fetch())
            .await
            .map_err(|e| (*e).clone())
    }

    /// Ключ проверки подписи по `kid`; без `kid` — единственный ключ набора.
    async fn key(&self, kid: Option<&str>) -> Result<DecodingKey> {
        let find = |keys: &Keys| -> Option<DecodingKey> {
            let jwk = match (kid, keys.set.keys.as_slice()) {
                (Some(kid), _) => keys.set.find(kid)?,
                (None, [only]) => only,
                (None, _) => return None,
            };
            DecodingKey::from_jwk(jwk).ok()
        };
        let keys = self.keys().await?;
        if let Some(k) = find(&keys) {
            return Ok(k);
        }
        if keys.fetched.elapsed() < self.cfg.jwks_refresh_min {
            return Err(Error::Forbidden);
        }
        self.keys.invalidate(&()).await;
        find(&*self.keys().await?).ok_or(Error::Forbidden)
    }

    /// Проверяет токен и возвращает его claims; неверный — `Forbidden`.
    pub async fn claims(&self, token: &str) -> Result<serde_json::Map<String, Value>> {
        let header = jsonwebtoken::decode_header(token).map_err(|_| Error::Forbidden)?;
        if !self.algs.contains(&header.alg) {
            return Err(Error::Forbidden);
        }
        let key = self.key(header.kid.as_deref()).await?;
        let mut v = Validation::new(header.alg);
        v.set_audience(&self.cfg.audience);
        v.set_issuer(&[&self.cfg.issuer]);
        v.set_required_spec_claims(&["exp", "iss", "aud"]);
        v.leeway = self.cfg.leeway.as_secs();
        v.validate_nbf = true;
        match jsonwebtoken::decode::<serde_json::Map<String, Value>>(token, &key, &v) {
            Ok(data) => Ok(data.claims),
            Err(e) => {
                tracing::debug!(kind = ?e.kind(), "OIDC token rejected");
                Err(Error::Forbidden)
            }
        }
    }
}

/// Корни (`ca_file`) и сертификат клиента для `reqwest`. Блокирующий I/O.
fn read_tls(tls: &InternalTls) -> Result<(Vec<reqwest::Certificate>, Option<reqwest::Identity>)> {
    let read = |p: &std::path::Path| {
        std::fs::read(p).map_err(|e| Error::Invalid(format!("OIDC: {}: {e}", p.display())))
    };
    let roots = match &tls.ca_file {
        Some(p) => reqwest::Certificate::from_pem_bundle(&read(p)?)
            .map_err(|e| Error::Invalid(format!("OIDC: tls.ca_file: {e}")))?,
        None => Vec::new(),
    };
    let identity = match (&tls.cert_file, &tls.key_file) {
        (Some(c), Some(k)) => {
            let mut pem = read(c)?;
            pem.push(b'\n');
            pem.extend(read(k)?);
            let id = reqwest::Identity::from_pem(&pem)
                .map_err(|e| Error::Invalid(format!("OIDC: tls.cert_file: {e}")));
            pem.fill(0);
            Some(id?)
        }
        _ => None,
    };
    Ok((roots, identity))
}

/// Области доступа: `scope` — строка через пробел (RFC 8693), `scp` — массив или строка.
fn scopes(claims: &serde_json::Map<String, Value>) -> BTreeSet<String> {
    let words = |s: &str| s.split_whitespace().map(str::to_owned).collect::<Vec<_>>();
    ["scope", "scp"]
        .iter()
        .filter_map(|k| claims.get(*k))
        .flat_map(|v| match v {
            Value::String(s) => words(s),
            Value::Array(a) => a
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        })
        .collect()
}

#[async_trait]
impl Authenticator for OidcAuthenticator {
    async fn verify(&self, credentials: Credentials) -> Result<Principal> {
        let Credentials::Bearer(token) = credentials else {
            return Err(Error::Forbidden);
        };
        let claims = self.claims(&token).await?;
        let login = claims
            .get(&self.cfg.login_claim)
            .and_then(Value::as_str)
            .ok_or(Error::Forbidden)?;
        if self.cfg.login_claim == "email"
            && claims.get("email_verified").and_then(Value::as_bool) == Some(false)
        {
            return Err(Error::Forbidden);
        }
        let account = self.accounts.enabled(login).await?;
        self.accounts
            .principal(account, Some(scopes(&claims)))
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use jsonwebtoken::jwk::Jwk;
    use jsonwebtoken::{EncodingKey, Header};
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::testutil::accounts;

    /// Ключи EC P-256 только для тестов.
    const K1: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgb7o3m589PUtHbPj7
UO9WdnJnyxmNux5C9f6eckNypWShRANCAASr/U2lzWo78B5woOdwQCRVpqkebDg+
1gcGS2Ti+KKk+MbcsMD4HOOurqZ2FQinAwb+s5NIvJtdc5fnWzGAY1wz
-----END PRIVATE KEY-----";
    const K2: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgvT84dWo7+RQOIYvZ
cEvRmCbWQ4ebUToVxqf4qLogPNqhRANCAAQcAPtk0UPqgezcUaaDR+mQ4YttNGrk
MUMElV6U7JBen+Mmd88wiTjFQnwPupCAPsH6marAmHdaA0ybQ0Ldn0G2
-----END PRIVATE KEY-----";

    fn key(pem: &str) -> EncodingKey {
        EncodingKey::from_ec_pem(pem.as_bytes()).expect("pem")
    }

    fn jwks(keys: &[(&str, &str)]) -> String {
        let keys: Vec<Jwk> = keys
            .iter()
            .map(|(kid, pem)| {
                let mut jwk = Jwk::from_encoding_key(&key(pem), Algorithm::ES256).expect("jwk");
                jwk.common.key_id = Some((*kid).to_owned());
                jwk
            })
            .collect();
        serde_json::to_string(&JwkSet { keys }).expect("jwks")
    }

    /// Поставщик OIDC на 127.0.0.1 (HTTP, профиль dev): discovery и JWKS, счётчик запросов по пути.
    struct Provider {
        issuer: String,
        jwks: Arc<Mutex<String>>,
        hits: Arc<Mutex<HashMap<String, usize>>>,
    }

    impl Provider {
        async fn start(keys: &[(&str, &str)]) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let issuer = format!("http://{}", listener.local_addr().expect("addr"));
            let jwks = Arc::new(Mutex::new(jwks(keys)));
            let hits = Arc::new(Mutex::new(HashMap::new()));
            let (iss, set, count) = (issuer.clone(), jwks.clone(), hits.clone());
            tokio::spawn(async move {
                while let Ok((mut sock, _)) = listener.accept().await {
                    let mut buf = vec![0; 4096];
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req.split_whitespace().nth(1).unwrap_or("").to_owned();
                    *count.lock().expect("lock").entry(path.clone()).or_insert(0) += 1;
                    let body = match path.as_str() {
                        "/.well-known/openid-configuration" => {
                            json!({ "issuer": iss, "jwks_uri": format!("{iss}/jwks") }).to_string()
                        }
                        "/jwks" => set.lock().expect("lock").clone(),
                        _ => String::new(),
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                }
            });
            Provider { issuer, jwks, hits }
        }

        fn hits(&self, path: &str) -> usize {
            self.hits
                .lock()
                .expect("lock")
                .get(path)
                .copied()
                .unwrap_or(0)
        }

        fn cfg(&self) -> OidcConfig {
            OidcConfig {
                issuer: self.issuer.clone(),
                audience: vec!["thepost".into()],
                tls: InternalTls {
                    allow_plaintext: true,
                    ..InternalTls::default()
                },
                ..OidcConfig::default()
            }
        }
    }

    fn now() -> u64 {
        jsonwebtoken::get_current_timestamp()
    }

    fn token(kid: Option<&str>, pem: &str, claims: Value) -> String {
        let mut h = Header::new(Algorithm::ES256);
        h.kid = kid.map(str::to_owned);
        jsonwebtoken::encode(&h, &claims, &key(pem)).expect("encode")
    }

    fn claims(p: &Provider, extra: Value) -> Value {
        let mut c = json!({
            "iss": p.issuer,
            "aud": "thepost",
            "exp": now() + 300,
            "iat": now(),
            "email": "alice@example.org",
            "scope": "openid mail",
        });
        if let (Some(c), Value::Object(e)) = (c.as_object_mut(), extra) {
            c.extend(e);
        }
        c
    }

    async fn setup(cfg: OidcConfig) -> (OidcAuthenticator, tp_contracts::AccountId) {
        let (accounts, ids) =
            accounts(&[("alice@example.org", true), ("bob@example.org", false)]).await;
        let auth = OidcAuthenticator::new(cfg, Profile::Dev, accounts)
            .await
            .expect("new");
        (auth, ids[0])
    }

    #[tokio::test]
    async fn valid_token_and_cache() {
        let p = Provider::start(&[("k1", K1)]).await;
        let (auth, alice) = setup(p.cfg()).await;
        let t = token(Some("k1"), K1, claims(&p, json!({})));
        let pr = auth
            .verify(Credentials::Bearer(t.clone()))
            .await
            .expect("ok");
        assert_eq!(pr.account, alice);
        assert_eq!(pr.login, "alice@example.org");
        assert_eq!(pr.groups, BTreeSet::from(["staff".to_owned()]));
        assert_eq!(
            pr.scopes,
            Some(BTreeSet::from(["mail".to_owned(), "openid".to_owned()]))
        );
        assert!(pr.has_scope("mail") && !pr.has_scope("admin"));
        // Набор ключей — из кэша.
        auth.verify(Credentials::Bearer(t)).await.expect("again");
        assert_eq!(p.hits("/.well-known/openid-configuration"), 1);
        assert_eq!(p.hits("/jwks"), 1);
        // Без kid — единственный ключ набора; scp массивом.
        let t = token(None, K1, claims(&p, json!({ "scp": ["imap"] })));
        let pr = auth.verify(Credentials::Bearer(t)).await.expect("no kid");
        assert!(pr.has_scope("imap"));
        assert_eq!(
            auth.discovery_url(),
            format!("{}/.well-known/openid-configuration", p.issuer)
        );
    }

    #[tokio::test]
    async fn rejected_tokens() {
        let p = Provider::start(&[("k1", K1)]).await;
        let (auth, _) = setup(p.cfg()).await;
        let hs = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims(&p, json!({})),
            &EncodingKey::from_secret(b"k"),
        )
        .expect("hs");
        let bad = [
            token(Some("k1"), K1, claims(&p, json!({ "aud": "other" }))),
            token(
                Some("k1"),
                K1,
                claims(&p, json!({ "iss": "https://evil.example" })),
            ),
            token(Some("k1"), K1, claims(&p, json!({ "exp": now() - 3600 }))),
            token(Some("k1"), K1, claims(&p, json!({ "nbf": now() + 3600 }))),
            token(Some("k1"), K2, claims(&p, json!({}))),
            token(
                Some("k1"),
                K1,
                claims(&p, json!({ "email": "nobody@example.org" })),
            ),
            token(
                Some("k1"),
                K1,
                claims(&p, json!({ "email": "bob@example.org" })),
            ),
            token(
                Some("k1"),
                K1,
                claims(&p, json!({ "email_verified": false })),
            ),
            hs,
            "not.a.token".to_owned(),
        ];
        for t in bad {
            assert_eq!(
                auth.verify(Credentials::Bearer(t)).await,
                Err(Error::Forbidden)
            );
        }
        let pw = Credentials::Password {
            login: "alice@example.org".into(),
            secret: "x".into(),
        };
        assert_eq!(auth.verify(pw).await, Err(Error::Forbidden));
    }

    #[tokio::test]
    async fn key_rotation() {
        let p = Provider::start(&[("k1", K1)]).await;
        let (auth, _) = setup(OidcConfig {
            jwks_refresh_min: Duration::ZERO,
            ..p.cfg()
        })
        .await;
        auth.verify(Credentials::Bearer(token(
            Some("k1"),
            K1,
            claims(&p, json!({})),
        )))
        .await
        .expect("k1");
        *p.jwks.lock().expect("lock") = jwks(&[("k2", K2)]);
        auth.verify(Credentials::Bearer(token(
            Some("k2"),
            K2,
            claims(&p, json!({})),
        )))
        .await
        .expect("k2 after refresh");
        assert_eq!(p.hits("/jwks"), 2);

        // Частое перечитывание ограничено: неизвестный kid в пределах jwks_refresh_min — отказ без запроса.
        let (slow, _) = setup(p.cfg()).await;
        slow.verify(Credentials::Bearer(token(
            Some("k2"),
            K2,
            claims(&p, json!({})),
        )))
        .await
        .expect("k2");
        let unknown = token(Some("k3"), K1, claims(&p, json!({})));
        assert_eq!(
            slow.verify(Credentials::Bearer(unknown)).await,
            Err(Error::Forbidden)
        );
        assert_eq!(p.hits("/jwks"), 3);
    }

    #[tokio::test]
    async fn provider_problems() {
        let p = Provider::start(&[("k1", K1)]).await;
        // Издатель в discovery не совпадает с настроенным.
        let (auth, _) = setup(OidcConfig {
            issuer: format!("{}/", p.issuer),
            ..p.cfg()
        })
        .await;
        let t = token(Some("k1"), K1, claims(&p, json!({})));
        assert!(matches!(
            auth.verify(Credentials::Bearer(t)).await,
            Err(Error::Internal(_))
        ));
        // Поставщик недоступен.
        let (auth, _) = setup(OidcConfig {
            jwks_uri: Some("http://127.0.0.1:9/jwks".into()),
            ..p.cfg()
        })
        .await;
        let t = token(Some("k1"), K1, claims(&p, json!({})));
        assert!(matches!(
            auth.verify(Credentials::Bearer(t)).await,
            Err(Error::Unavailable { .. })
        ));
    }

    #[test]
    fn config_check() {
        let cfg = OidcConfig {
            issuer: "https://id.example.com".into(),
            audience: vec!["thepost".into()],
            ..OidcConfig::default()
        };
        assert_eq!(cfg.check(Profile::Prod), Ok(false));
        let http = OidcConfig {
            issuer: "http://id.example.com".into(),
            ..cfg.clone()
        };
        assert!(http.check(Profile::Prod).is_err());
        let hs = OidcConfig {
            algorithms: vec!["HS256".into()],
            ..cfg.clone()
        };
        assert!(hs.check(Profile::Prod).is_err());
        let no_aud = OidcConfig {
            audience: Vec::new(),
            ..cfg.clone()
        };
        assert!(no_aud.check(Profile::Prod).is_err());
        let parsed: OidcConfig = serde_json::from_str(
            r#"{"issuer": "https://id.example.com", "audience": ["a"], "jwks_ttl": "1h"}"#,
        )
        .expect("parse");
        assert_eq!(parsed.jwks_ttl, Duration::from_secs(3600));
        assert_eq!(parsed.login_claim, "email");
    }

    #[test]
    fn scope_claims() {
        let c = json!({ "scope": "a  b", "scp": ["c"] });
        assert_eq!(
            scopes(c.as_object().expect("obj")),
            BTreeSet::from(["a".into(), "b".into(), "c".into()])
        );
    }
}
