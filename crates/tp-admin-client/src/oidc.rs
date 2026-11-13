//! Вход администратора через поставщика OpenID Connect с устройства без браузера (APP-42, решение 2026-11-11
//! п. 79): OAuth 2.0 Device Authorization Grant (RFC 8628).
//!
//! - Поставщик — `auth.oidc` (тот же, чьи токены проверяет узел): из его документа discovery —
//!   `device_authorization_endpoint` и `token_endpoint`. Клиент и области доступа — [`DeviceSettings`].
//! - Адрес и код для пользователя показывает вызывающий ([`DeviceLogin::login`], текст — [`prompt_text`]).
//! - Опрос токена — с паузой `interval` поставщика (нет — [`DeviceSettings::poll_interval`]); `slow_down`
//!   увеличивает паузу на [`DeviceSettings::slow_down_step`]; срок кода — `expires_in`, но не дольше
//!   [`DeviceSettings::max_wait`].
//!
//! Библиотеки OAuth нет: три запроса reqwest (решение 2026-10-31 — зависимость дороже своего кода такого объёма).

use std::path::Path;
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tp_auth::OidcConfig;
use tp_contracts::secrets::{Secret, SecretStore};
use tp_contracts::tls::{InternalTls, KeySource, Profile};

use crate::{Error, Result};

/// Тип разрешения RFC 8628.
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Клиент OAuth у поставщика и опрос токена (в config.yaml узла — `cli.oidc`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceSettings {
    /// Идентификатор открытого клиента (public client) с разрешённым Device Authorization Grant.
    pub client_id: String,
    /// Запрашиваемые области доступа через пробел.
    pub scope: String,
    /// Пауза между опросами токена, если поставщик не задал `interval`.
    pub poll_interval: Duration,
    /// Прибавка к паузе на ответ `slow_down`.
    pub slow_down_step: Duration,
    /// Наибольшее время ожидания подтверждения, даже если код поставщика действует дольше.
    pub max_wait: Duration,
}

/// Документ discovery поставщика: поля для входа с устройства.
#[derive(Debug, Deserialize)]
struct Discovery {
    issuer: String,
    device_authorization_endpoint: Option<String>,
    token_endpoint: String,
}

/// Ответ `device_authorization_endpoint` (RFC 8628, 3.2).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct DeviceCode {
    device_code: String,
    /// Код, который пользователь вводит на странице поставщика.
    pub user_code: String,
    /// Адрес страницы поставщика.
    pub verification_uri: String,
    /// Тот же адрес с подставленным кодом (для QR-кода и ссылки).
    pub verification_uri_complete: Option<String>,
    /// Срок кода, секунд.
    pub expires_in: u64,
    /// Наименьшая пауза между опросами токена, секунд.
    pub interval: Option<u64>,
}

/// Успешный ответ `token_endpoint`.
#[derive(Deserialize)]
struct Token {
    access_token: String,
}

/// Отказ `token_endpoint` и `device_authorization_endpoint` (RFC 6749, 5.2).
#[derive(Debug, Deserialize)]
struct OAuthError {
    error: String,
    error_description: Option<String>,
}

impl OAuthError {
    fn text(&self) -> String {
        match &self.error_description {
            Some(d) if !d.is_empty() => format!("{}: {d}", self.error),
            _ => self.error.clone(),
        }
    }
}

/// Клиент входа с устройства у поставщика `auth.oidc`.
#[derive(Debug)]
pub struct DeviceLogin {
    http: reqwest::Client,
    issuer: String,
    max_response: usize,
    settings: DeviceSettings,
}

impl DeviceLogin {
    /// Клиент к поставщику `oidc` с настройками `settings`; канал — как у узла (`auth.oidc.tls`, открытый — только
    /// dev; ключ сертификата клиента `tls.key` — из хранилища `secrets`).
    pub async fn new(
        oidc: &OidcConfig,
        settings: DeviceSettings,
        profile: Profile,
        secrets: &dyn SecretStore,
    ) -> Result<Self> {
        let plaintext = oidc.check(profile).map_err(|e| format!("auth.oidc: {e}"))?;
        let key = client_key(&oidc.tls, secrets).await?;
        let tls = oidc.tls.clone();
        let (roots, identity) = tokio::task::spawn_blocking(move || read_tls(&tls, key))
            .await
            .map_err(|e| format!("auth.oidc.tls: {e}"))??;
        let mut http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(oidc.http_timeout)
            .https_only(!plaintext);
        for cert in roots {
            http = http.add_root_certificate(cert);
        }
        if let Some(identity) = identity {
            http = http.identity(identity);
        }
        Ok(Self {
            http: http.build().map_err(|e| format!("клиент HTTP: {e}"))?,
            issuer: oidc.issuer.clone(),
            max_response: oidc.max_response,
            settings,
        })
    }

    /// Вход: код устройства → `prompt` (показать пользователю адрес и код) → опрос токена до подтверждения,
    /// отказа или истечения кода. Возвращает токен доступа.
    pub async fn login(&self, prompt: impl FnOnce(&DeviceCode)) -> Result<String> {
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            self.issuer.trim_end_matches('/')
        );
        let d: Discovery = match self.http.get(&discovery_url).send().await {
            Ok(resp) => self.json(&discovery_url, resp).await?,
            Err(e) => return Err(fail(format!("поставщик OIDC {discovery_url}: {e}"))),
        };
        if d.issuer != self.issuer {
            return Err(fail(format!(
                "поставщик OIDC: издатель в discovery {:?} не совпадает с auth.oidc.issuer {:?}",
                d.issuer, self.issuer
            )));
        }
        let device_endpoint = d.device_authorization_endpoint.ok_or_else(|| {
            fail(format!(
                "поставщик OIDC {} не объявляет device_authorization_endpoint: вход с устройства (RFC 8628) \
                 не поддерживается или выключен",
                self.issuer
            ))
        })?;

        let s = &self.settings;
        let code: DeviceCode = self
            .post(
                &device_endpoint,
                &[("client_id", &s.client_id), ("scope", &s.scope)],
            )
            .await?
            .map_err(|e| {
                fail(format!(
                    "поставщик OIDC отказал в коде устройства: {}",
                    e.text()
                ))
            })?;
        prompt(&code);

        let expired =
            || fail("код устройства истёк: вход не подтверждён вовремя — повторите команду");
        let lifetime = Duration::from_secs(code.expires_in).min(s.max_wait);
        let deadline = tokio::time::Instant::now() + lifetime;
        let mut interval = code.interval.map_or(s.poll_interval, Duration::from_secs);
        loop {
            if tokio::time::Instant::now() + interval >= deadline {
                return Err(expired());
            }
            tokio::time::sleep(interval).await;
            let answer = self
                .post::<Token>(
                    &d.token_endpoint,
                    &[
                        ("grant_type", DEVICE_GRANT),
                        ("device_code", &code.device_code),
                        ("client_id", &s.client_id),
                    ],
                )
                .await?;
            match answer {
                Ok(token) => return Ok(token.access_token),
                Err(e) => match e.error.as_str() {
                    "authorization_pending" => {}
                    "slow_down" => interval += s.slow_down_step,
                    "expired_token" => return Err(expired()),
                    "access_denied" => {
                        return Err(fail("вход отклонён на странице поставщика OIDC"));
                    }
                    _ => {
                        return Err(fail(format!(
                            "поставщик OIDC отказал в токене: {}",
                            e.text()
                        )));
                    }
                },
            }
        }
    }

    /// `POST` формы `application/x-www-form-urlencoded`: ответ 2xx — `T`, отказ OAuth — [`OAuthError`].
    async fn post<T: DeserializeOwned>(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<std::result::Result<T, OAuthError>> {
        let resp = self
            .http
            .post(url)
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(ACCEPT, "application/json")
            .body(urlencoded(form)?)
            .send()
            .await
            .map_err(|e| format!("поставщик OIDC {url}: {e}"))?;
        if resp.status().is_success() {
            return self.json(url, resp).await.map(Ok);
        }
        let status = resp.status();
        let body = self.body(url, resp).await?;
        match serde_json::from_slice::<OAuthError>(&body) {
            Ok(e) => Ok(Err(e)),
            Err(_) => Err(fail(format!(
                "поставщик OIDC {url}: HTTP {}",
                status.as_u16()
            ))),
        }
    }

    async fn json<T: DeserializeOwned>(&self, url: &str, resp: reqwest::Response) -> Result<T> {
        let status = resp.status();
        if status != StatusCode::OK {
            return Err(fail(format!(
                "поставщик OIDC {url}: HTTP {}",
                status.as_u16()
            )));
        }
        let body = self.body(url, resp).await?;
        serde_json::from_slice(&body)
            .map_err(|e| fail(format!("поставщик OIDC {url}: ответ не разобран: {e}")))
    }

    /// Тело ответа не больше `auth.oidc.max_response`.
    async fn body(&self, url: &str, mut resp: reqwest::Response) -> Result<Vec<u8>> {
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| format!("поставщик OIDC {url}: {e}"))?
        {
            if body.len() + chunk.len() > self.max_response {
                return Err(fail(format!(
                    "поставщик OIDC {url}: ответ больше auth.oidc.max_response"
                )));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

/// Ошибка клиента с текстом `msg`.
fn fail(msg: impl Into<String>) -> Error {
    Error::Client(msg.into())
}

/// Тело `application/x-www-form-urlencoded` (кодировщик формы `url` — тот же, что у строки запроса адреса).
fn urlencoded(form: &[(&str, &str)]) -> Result<String> {
    let mut url = Url::parse("http://form.invalid/").map_err(|e| e.to_string())?;
    url.query_pairs_mut().extend_pairs(form);
    Ok(url.query().unwrap_or_default().to_owned())
}

/// Ключ сертификата клиента канала к поставщику (`auth.oidc.tls.key`, PEM) из хранилища `secrets`, как у узла;
/// `None` — без mTLS. `tls.key_file` не поддерживается (решение 100).
async fn client_key(tls: &InternalTls, secrets: &dyn SecretStore) -> Result<Option<Secret>> {
    match tls
        .key_source()
        .map_err(|e| format!("auth.oidc.tls: {e}"))?
    {
        None => Ok(None),
        Some(KeySource::File(_)) => Err(fail(
            "auth.oidc.tls.key_file не поддерживается; задайте tls.key: { secret: \"<путь>#<поле>\" }",
        )),
        Some(KeySource::Secret(r)) => secrets.get(r).await.map(Some).map_err(|e| {
            fail(format!(
                "auth.oidc.tls.key: секрет {}#{}: {e}",
                r.path(),
                r.field()
            ))
        }),
    }
}

/// Корневые сертификаты и сертификат клиента канала к поставщику (`auth.oidc.tls`, ключ `key`), как у узла.
fn read_tls(
    tls: &InternalTls,
    key: Option<Secret>,
) -> Result<(Vec<reqwest::Certificate>, Option<reqwest::Identity>)> {
    let read =
        |p: &Path| std::fs::read(p).map_err(|e| format!("auth.oidc.tls: {}: {e}", p.display()));
    let roots = match &tls.ca_file {
        Some(p) => reqwest::Certificate::from_pem_bundle(&read(p)?)
            .map_err(|e| format!("auth.oidc.tls.ca_file: {e}"))?,
        None => Vec::new(),
    };
    let identity = match (&tls.cert_file, key) {
        (Some(cert), Some(key)) => {
            let mut pem = read(cert)?;
            pem.push(b'\n');
            pem.extend_from_slice(key.expose().as_bytes());
            let identity = reqwest::Identity::from_pem(&pem)
                .map_err(|e| format!("auth.oidc.tls.cert_file: {e}"));
            pem.fill(0);
            Some(identity?)
        }
        _ => None,
    };
    Ok((roots, identity))
}

/// Текст для пользователя: адрес страницы поставщика и код.
pub fn prompt_text(code: &DeviceCode) -> String {
    let mut text = format!(
        "Откройте {} и введите код: {}\n",
        code.verification_uri, code.user_code
    );
    if let Some(complete) = &code.verification_uri_complete {
        text.push_str(&format!("Или откройте ссылку с кодом: {complete}\n"));
    }
    text.push_str(&format!(
        "Код действует {} с. Ожидание подтверждения…\n",
        code.expires_in
    ));
    text
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::http::StatusCode as Status;
    use axum::routing::{get, post};
    use axum::{Form, Json, Router};
    use serde_json::{Value, json};
    use tp_testkit::MemSecretStore;

    use super::*;

    /// Заглушка поставщика: ответы `token_endpoint` по очереди и моменты опросов.
    #[derive(Clone)]
    struct Provider {
        issuer: Arc<Mutex<String>>,
        device: Value,
        answers: Arc<Mutex<Vec<(Status, Value)>>>,
        polls: Arc<Mutex<Vec<tokio::time::Instant>>>,
        forms: Arc<Mutex<Vec<HashMap<String, String>>>>,
    }

    async fn discovery(State(p): State<Provider>) -> Json<Value> {
        let issuer = p.issuer.lock().expect("lock").clone();
        Json(json!({
            "issuer": issuer,
            "device_authorization_endpoint": format!("{issuer}/device"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
        }))
    }

    async fn device_endpoint(
        State(p): State<Provider>,
        Form(form): Form<HashMap<String, String>>,
    ) -> Json<Value> {
        p.forms.lock().expect("lock").push(form);
        Json(p.device.clone())
    }

    async fn token(
        State(p): State<Provider>,
        Form(form): Form<HashMap<String, String>>,
    ) -> (Status, Json<Value>) {
        p.polls
            .lock()
            .expect("lock")
            .push(tokio::time::Instant::now());
        p.forms.lock().expect("lock").push(form);
        let mut answers = p.answers.lock().expect("lock");
        let (status, body) = if answers.len() > 1 {
            answers.remove(0)
        } else {
            answers[0].clone()
        };
        (status, Json(body))
    }

    fn pending(error: &str) -> (Status, Value) {
        (Status::BAD_REQUEST, json!({ "error": error }))
    }

    /// Настройки входа по умолчанию узла (`cli.oidc`) с клиентом `client_id`.
    fn settings(client_id: &str) -> DeviceSettings {
        DeviceSettings {
            client_id: client_id.to_owned(),
            scope: "openid".to_owned(),
            poll_interval: Duration::from_secs(5),
            slow_down_step: Duration::from_secs(5),
            max_wait: Duration::from_secs(15 * 60),
        }
    }

    /// Поставщик `issuer` в профиле dev, открытый канал.
    fn plaintext_oidc(issuer: String) -> OidcConfig {
        OidcConfig {
            issuer,
            audience: vec!["thepost".into()],
            tls: InternalTls {
                allow_plaintext: true,
                ..InternalTls::default()
            },
            ..OidcConfig::default()
        }
    }

    /// Поставщик на loopback и клиент к нему (профиль dev, открытый канал).
    async fn provider(device: Value, answers: Vec<(Status, Value)>) -> (Provider, DeviceLogin) {
        let p = Provider {
            issuer: Arc::default(),
            device,
            answers: Arc::new(Mutex::new(answers)),
            polls: Arc::default(),
            forms: Arc::default(),
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let issuer = format!("http://{}", listener.local_addr().expect("addr"));
        *p.issuer.lock().expect("lock") = issuer.clone();
        let router = Router::new()
            .route("/.well-known/openid-configuration", get(discovery))
            .route("/device", post(device_endpoint))
            .route("/token", post(token))
            .with_state(p.clone());
        tokio::spawn(async move { axum::serve(listener, router).await });
        let settings = DeviceSettings {
            slow_down_step: Duration::from_millis(400),
            ..settings("thepost cli")
        };
        let login = DeviceLogin::new(
            &plaintext_oidc(issuer),
            settings,
            Profile::Dev,
            &MemSecretStore::default(),
        )
        .await
        .expect("client");
        (p, login)
    }

    fn device_code(expires_in: u64) -> Value {
        json!({
            "device_code": "dev-1",
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://sso.example.test/device",
            "verification_uri_complete": "https://sso.example.test/device?user_code=WDJB-MJHT",
            "expires_in": expires_in,
            "interval": 0,
        })
    }

    /// RFC 8628: `authorization_pending` → `slow_down` (пауза растёт на шаг) → токен; пользователю показаны
    /// адрес и код; в запросах — клиент, области доступа и код устройства.
    #[tokio::test]
    async fn pending_then_slow_down_then_token() {
        let (p, login) = provider(
            device_code(600),
            vec![
                pending("authorization_pending"),
                pending("slow_down"),
                (
                    Status::OK,
                    json!({ "access_token": "at-1", "token_type": "Bearer" }),
                ),
            ],
        )
        .await;
        let mut shown = String::new();
        let token = login
            .login(|code| shown = prompt_text(code))
            .await
            .expect("login");
        assert_eq!(token, "at-1");
        assert!(
            shown.contains("https://sso.example.test/device") && shown.contains("WDJB-MJHT"),
            "{shown}"
        );
        let polls = p.polls.lock().expect("lock").clone();
        assert_eq!(polls.len(), 3);
        assert!(
            polls[2] - polls[1] >= Duration::from_millis(400),
            "после slow_down пауза увеличена на шаг"
        );
        assert!(polls[1] - polls[0] < Duration::from_millis(400));
        let forms = p.forms.lock().expect("lock").clone();
        assert_eq!(forms[0]["client_id"], "thepost cli");
        assert_eq!(forms[0]["scope"], "openid");
        assert_eq!(forms[1]["grant_type"], DEVICE_GRANT);
        assert_eq!(forms[1]["device_code"], "dev-1");
        assert_eq!(forms[1]["client_id"], "thepost cli");
    }

    /// Истёкший код — понятная ошибка: и по ответу поставщика `expired_token`, и по сроку `expires_in`, если
    /// поставщик продолжает отвечать `authorization_pending`.
    #[tokio::test]
    async fn expired_code_is_reported() {
        let (_p, login) = provider(device_code(600), vec![pending("expired_token")]).await;
        let e = login.login(|_| {}).await.expect_err("expired");
        assert!(e.to_string().contains("код устройства истёк"), "{e}");

        let mut code = device_code(1);
        code["interval"] = json!(null);
        let (p, mut login) = provider(code, vec![pending("authorization_pending")]).await;
        login.settings.poll_interval = Duration::from_millis(300);
        let e = login.login(|_| {}).await.expect_err("expired");
        assert!(e.to_string().contains("код устройства истёк"), "{e}");
        assert!(!p.polls.lock().expect("lock").is_empty());
    }

    /// Отказ пользователя и поставщик без входа с устройства — отдельные понятные ошибки.
    #[tokio::test]
    async fn denied_and_unsupported() {
        let (_p, login) = provider(device_code(600), vec![pending("access_denied")]).await;
        let e = login.login(|_| {}).await.expect_err("denied");
        assert!(e.to_string().contains("вход отклонён"), "{e}");

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let issuer = format!("http://{}", listener.local_addr().expect("addr"));
        let doc = json!({ "issuer": issuer, "token_endpoint": format!("{issuer}/token") });
        let router = Router::new().route(
            "/.well-known/openid-configuration",
            get(move || async move { Json(doc) }),
        );
        tokio::spawn(async move { axum::serve(listener, router).await });
        let e = DeviceLogin::new(
            &plaintext_oidc(issuer),
            settings("thepost-cli"),
            Profile::Dev,
            &MemSecretStore::default(),
        )
        .await
        .expect("client")
        .login(|_| {})
        .await
        .expect_err("unsupported");
        assert!(
            e.to_string().contains("device_authorization_endpoint"),
            "{e}"
        );
    }

    /// APP-56: ключ сертификата клиента `auth.oidc.tls.key` — из хранилища секретов; секрета нет — ошибка с путём;
    /// `tls.key_file` — ошибка.
    #[tokio::test]
    async fn client_key_from_secret_store() {
        use tp_contracts::secrets::SecretRef;

        let dir =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../tp-admin/src/testdata");
        let r = SecretRef::new("thepost/oidc", "tls_key").expect("ref");
        let mut oidc = OidcConfig {
            issuer: "https://sso.example.test".into(),
            audience: vec!["thepost".into()],
            tls: InternalTls {
                cert_file: Some(dir.join("client.pem")),
                key: Some(r.clone()),
                ..InternalTls::default()
            },
            ..OidcConfig::default()
        };
        let store = MemSecretStore::default();
        let login = |oidc: OidcConfig, store: MemSecretStore| async move {
            DeviceLogin::new(&oidc, settings("thepost-cli"), Profile::Dev, &store).await
        };
        let e = login(oidc.clone(), store.clone())
            .await
            .expect_err("нет секрета");
        assert!(e.to_string().contains("thepost/oidc#tls_key"), "{e}");

        store.set(
            &r,
            std::fs::read_to_string(dir.join("client-key.pem")).expect("key"),
        );
        login(oidc.clone(), store.clone())
            .await
            .expect("клиент с ключом из хранилища");

        oidc.tls.key = None;
        oidc.tls.key_file = Some(dir.join("client-key.pem"));
        let e = login(oidc, store).await.expect_err("key_file");
        assert!(e.to_string().contains("key_file"), "{e}");
    }
}
