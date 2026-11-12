//! Вход администратора через поставщика OpenID Connect с устройства без браузера (APP-42, решение 2026-11-11
//! п. 79): OAuth 2.0 Device Authorization Grant (RFC 8628).
//!
//! - Поставщик — `auth.oidc` из config.yaml (тот же, чьи токены проверяет узел): из его документа discovery —
//!   `device_authorization_endpoint` и `token_endpoint`. Клиент и области доступа — секция `cli.oidc`.
//! - Адрес и код для пользователя — в stderr; токен доступа — единственной строкой в stdout, чтобы он не попал
//!   в историю оболочки: `export THEPOST_ADMIN_TOKEN="$(…)"`.
//! - Опрос токена — с паузой `interval` поставщика (нет — `cli.oidc.poll_interval`); `slow_down` увеличивает
//!   паузу на `cli.oidc.slow_down_step`; срок кода — `expires_in`, но не дольше `cli.oidc.max_wait`.
//!
//! Библиотеки OAuth нет: три запроса reqwest (решение 2026-10-31 — зависимость дороже своего кода такого объёма).

use std::path::Path;
use std::time::Duration;

use reqwest::header::{ACCEPT, CONTENT_TYPE};
use reqwest::{StatusCode, Url};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use tp_auth::OidcConfig;
use tp_contracts::tls::{InternalTls, Profile};

use crate::sections::{self, AUTH, AuthSection, CLI, CliOidc, CliSection};

/// Тип разрешения RFC 8628.
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

type CliResult<T> = std::result::Result<T, String>;

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
    cli: CliOidc,
    client_id: String,
}

impl DeviceLogin {
    /// Клиент к поставщику `oidc` с настройками `cli`; канал — как у узла (`auth.oidc.tls`, открытый — только dev).
    pub async fn new(oidc: &OidcConfig, cli: &CliOidc, profile: Profile) -> CliResult<Self> {
        let plaintext = oidc.check(profile).map_err(|e| format!("auth.oidc: {e}"))?;
        cli.check().map_err(|e| e.to_string())?;
        let client_id = cli.client_id.clone().ok_or_else(|| {
            "не задан cli.oidc.client_id — идентификатор открытого клиента (public client) с разрешённым \
             Device Authorization Grant у поставщика OIDC"
                .to_owned()
        })?;
        let tls = oidc.tls.clone();
        let (roots, identity) = tokio::task::spawn_blocking(move || read_tls(&tls))
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
            cli: cli.clone(),
            client_id,
        })
    }

    /// Вход: код устройства → `prompt` (показать пользователю адрес и код) → опрос токена до подтверждения,
    /// отказа или истечения кода. Возвращает токен доступа.
    pub async fn login(&self, prompt: impl FnOnce(&DeviceCode)) -> CliResult<String> {
        let discovery_url = format!(
            "{}/.well-known/openid-configuration",
            self.issuer.trim_end_matches('/')
        );
        let d: Discovery = match self.http.get(&discovery_url).send().await {
            Ok(resp) => self.json(&discovery_url, resp).await?,
            Err(e) => return Err(format!("поставщик OIDC {discovery_url}: {e}")),
        };
        if d.issuer != self.issuer {
            return Err(format!(
                "поставщик OIDC: издатель в discovery {:?} не совпадает с auth.oidc.issuer {:?}",
                d.issuer, self.issuer
            ));
        }
        let device_endpoint = d.device_authorization_endpoint.ok_or_else(|| {
            format!(
                "поставщик OIDC {} не объявляет device_authorization_endpoint: вход с устройства (RFC 8628) \
                 не поддерживается или выключен",
                self.issuer
            )
        })?;

        let code: DeviceCode = self
            .post(
                &device_endpoint,
                &[("client_id", &self.client_id), ("scope", &self.cli.scope)],
            )
            .await?
            .map_err(|e| format!("поставщик OIDC отказал в коде устройства: {}", e.text()))?;
        prompt(&code);

        let expired =
            || "код устройства истёк: вход не подтверждён вовремя — повторите команду".to_owned();
        let lifetime = Duration::from_secs(code.expires_in).min(self.cli.max_wait);
        let deadline = tokio::time::Instant::now() + lifetime;
        let mut interval = code
            .interval
            .map_or(self.cli.poll_interval, Duration::from_secs);
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
                        ("client_id", &self.client_id),
                    ],
                )
                .await?;
            match answer {
                Ok(token) => return Ok(token.access_token),
                Err(e) => match e.error.as_str() {
                    "authorization_pending" => {}
                    "slow_down" => interval += self.cli.slow_down_step,
                    "expired_token" => return Err(expired()),
                    "access_denied" => {
                        return Err("вход отклонён на странице поставщика OIDC".to_owned());
                    }
                    _ => return Err(format!("поставщик OIDC отказал в токене: {}", e.text())),
                },
            }
        }
    }

    /// `POST` формы `application/x-www-form-urlencoded`: ответ 2xx — `T`, отказ OAuth — [`OAuthError`].
    async fn post<T: DeserializeOwned>(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> CliResult<std::result::Result<T, OAuthError>> {
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
            Err(_) => Err(format!("поставщик OIDC {url}: HTTP {}", status.as_u16())),
        }
    }

    async fn json<T: DeserializeOwned>(&self, url: &str, resp: reqwest::Response) -> CliResult<T> {
        let status = resp.status();
        if status != StatusCode::OK {
            return Err(format!("поставщик OIDC {url}: HTTP {}", status.as_u16()));
        }
        let body = self.body(url, resp).await?;
        serde_json::from_slice(&body)
            .map_err(|e| format!("поставщик OIDC {url}: ответ не разобран: {e}"))
    }

    /// Тело ответа не больше `auth.oidc.max_response`.
    async fn body(&self, url: &str, mut resp: reqwest::Response) -> CliResult<Vec<u8>> {
        let mut body = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| format!("поставщик OIDC {url}: {e}"))?
        {
            if body.len() + chunk.len() > self.max_response {
                return Err(format!(
                    "поставщик OIDC {url}: ответ больше auth.oidc.max_response"
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

/// Тело `application/x-www-form-urlencoded` (кодировщик формы `url` — тот же, что у строки запроса адреса).
fn urlencoded(form: &[(&str, &str)]) -> CliResult<String> {
    let mut url = Url::parse("http://form.invalid/").map_err(|e| e.to_string())?;
    url.query_pairs_mut().extend_pairs(form);
    Ok(url.query().unwrap_or_default().to_owned())
}

/// Корневые сертификаты и сертификат клиента канала к поставщику (`auth.oidc.tls`), как у узла.
fn read_tls(
    tls: &InternalTls,
) -> CliResult<(Vec<reqwest::Certificate>, Option<reqwest::Identity>)> {
    let read =
        |p: &Path| std::fs::read(p).map_err(|e| format!("auth.oidc.tls: {}: {e}", p.display()));
    let roots = match &tls.ca_file {
        Some(p) => reqwest::Certificate::from_pem_bundle(&read(p)?)
            .map_err(|e| format!("auth.oidc.tls.ca_file: {e}"))?,
        None => Vec::new(),
    };
    let identity = match (&tls.cert_file, &tls.key_file) {
        (Some(cert), Some(key)) => {
            let mut pem = read(cert)?;
            pem.push(b'\n');
            pem.extend(read(key)?);
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

/// Вход с устройства по config.yaml `config`: приглашение — в stderr, токен доступа — строкой в stdout.
/// Код возврата: 0 — токен получен, 1 — отказ или ошибка.
pub async fn command(config: &Path) -> i32 {
    match run(config).await {
        Ok(token) => {
            println!("{token}");
            eprintln!(
                "thepost: admin: токен OIDC получен; без записи в историю оболочки: \
                 export {}=\"$(thepost admin login --oidc)\"",
                crate::admin_client::ENV_TOKEN
            );
            0
        }
        Err(msg) => {
            eprintln!("thepost: admin: {msg}");
            1
        }
    }
}

async fn run(config: &Path) -> CliResult<String> {
    let registry = sections::registry().map_err(|e| e.to_string())?;
    let path = config.to_owned();
    let cfg = tokio::task::spawn_blocking(move || registry.load(&path))
        .await
        .map_err(|e| format!("чтение конфигурации прервано: {e}"))?
        .map_err(|e| format!("{}: {e}", config.display()))?;
    let auth = cfg
        .section::<AuthSection>(AUTH)
        .map_err(|e| format!("{}: секция auth: {e}", config.display()))?;
    let oidc = auth.oidc.as_ref().ok_or_else(|| {
        format!(
            "{}: нет auth.oidc — поставщик OIDC не настроен",
            config.display()
        )
    })?;
    let cli = cfg
        .section::<CliSection>(CLI)
        .map_err(|e| format!("{}: секция cli: {e}", config.display()))?;
    DeviceLogin::new(oidc, &cli.oidc, cfg.profile())
        .await?
        .login(|code| eprint!("{}", prompt_text(code)))
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    use axum::extract::State;
    use axum::http::StatusCode as Status;
    use axum::routing::{get, post};
    use axum::{Form, Json, Router};
    use serde_json::{Value, json};

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
        let oidc = OidcConfig {
            issuer,
            audience: vec!["thepost".into()],
            tls: InternalTls {
                allow_plaintext: true,
                ..InternalTls::default()
            },
            ..OidcConfig::default()
        };
        let cli = CliOidc {
            client_id: Some("thepost cli".into()),
            slow_down_step: Duration::from_millis(400),
            ..CliOidc::default()
        };
        let login = DeviceLogin::new(&oidc, &cli, Profile::Dev)
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
        assert!(e.contains("код устройства истёк"), "{e}");

        let mut code = device_code(1);
        code["interval"] = json!(null);
        let (p, mut login) = provider(code, vec![pending("authorization_pending")]).await;
        login.cli.poll_interval = Duration::from_millis(300);
        let e = login.login(|_| {}).await.expect_err("expired");
        assert!(e.contains("код устройства истёк"), "{e}");
        assert!(!p.polls.lock().expect("lock").is_empty());
    }

    /// Отказ пользователя и поставщик без входа с устройства — отдельные понятные ошибки; без `client_id`
    /// клиент не создаётся.
    #[tokio::test]
    async fn denied_unsupported_and_no_client() {
        let (_p, login) = provider(device_code(600), vec![pending("access_denied")]).await;
        let e = login.login(|_| {}).await.expect_err("denied");
        assert!(e.contains("вход отклонён"), "{e}");

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
        let oidc = OidcConfig {
            issuer,
            audience: vec!["thepost".into()],
            tls: InternalTls {
                allow_plaintext: true,
                ..InternalTls::default()
            },
            ..OidcConfig::default()
        };
        let cli = CliOidc {
            client_id: Some("thepost-cli".into()),
            ..CliOidc::default()
        };
        let e = DeviceLogin::new(&oidc, &cli, Profile::Dev)
            .await
            .expect("client")
            .login(|_| {})
            .await
            .expect_err("unsupported");
        assert!(e.contains("device_authorization_endpoint"), "{e}");

        let e = DeviceLogin::new(&oidc, &CliOidc::default(), Profile::Dev)
            .await
            .expect_err("no client");
        assert!(e.contains("cli.oidc.client_id"), "{e}");
    }
}
