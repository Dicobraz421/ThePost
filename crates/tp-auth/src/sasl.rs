//! Механизмы SASL PLAIN, LOGIN, OAUTHBEARER, XOAUTH2 (SRS 5.1; CONTRACTS.md#Auth) поверх [`Authenticator`].
//!
//! Разбор сообщений механизмов — `rsasl` (PLAIN с SASLprep, RFC 4616; LOGIN; OAUTHBEARER, RFC 7628;
//! XOAUTH2). Обратный вызов `rsasl` синхронный, поэтому он только забирает учётные данные из обмена, а
//! проверка — асинхронный [`Authenticator::verify`] после разбора. Для OAUTHBEARER и XOAUTH2 при неверном
//! токене сервер отвечает вызовом с ошибкой в JSON, клиент — пустым ответом (`%x01` для OAUTHBEARER), и обмен
//! завершается [`SaslFailure::Authentication`]; этот вызов строит тот же механизм `rsasl` повторным разбором.
//!
//! Вход от имени другого субъекта (`authzid`, `a=`, `user=`, отличный от имени входа) не поддерживается —
//! [`SaslFailure::Authorization`]. GSSAPI — AUTH-05.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use rsasl::callback::{Context, Request, SessionCallback, SessionData};
use rsasl::mechanisms::oauthbearer::properties::{OAuthBearerError, OAuthBearerValidate};
use rsasl::mechanisms::xoauth2::properties::XOAuth2Validate;
use rsasl::prelude::{Mechname, SASLConfig, SASLServer, Session, SessionError, Validation};
use rsasl::property::{AuthId, AuthzId, OAuthBearerToken, Password};
use rsasl::validate::{Validate, ValidationError};
use tp_contracts::auth::{Authenticator, Credentials, Principal};
use tp_contracts::sasl::{Mechanism, SaslContext, SaslFailure, SaslServer, SaslSession, SaslStep};
use tp_contracts::{Error, Result};

/// Механизмы по умолчанию: всё, кроме GSSAPI.
const DEFAULT: [Mechanism; 4] = [
    Mechanism::Plain,
    Mechanism::Login,
    Mechanism::OauthBearer,
    Mechanism::Xoauth2,
];

/// [`SaslServer`] поверх [`Authenticator`]: пароль — [`Credentials::Password`], токен — [`Credentials::Bearer`].
#[derive(Clone)]
pub struct AuthSasl {
    authn: Arc<dyn Authenticator>,
    mechanisms: Vec<Mechanism>,
    openid_configuration: Option<String>,
}

impl fmt::Debug for AuthSasl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthSasl")
            .field("mechanisms", &self.mechanisms)
            .field("openid_configuration", &self.openid_configuration)
            .finish_non_exhaustive()
    }
}

impl AuthSasl {
    /// PLAIN, LOGIN, OAUTHBEARER, XOAUTH2 поверх `authn`.
    pub fn new(authn: Arc<dyn Authenticator>) -> Self {
        AuthSasl {
            authn,
            mechanisms: DEFAULT.to_vec(),
            openid_configuration: None,
        }
    }

    /// Предлагать только `mechanisms` (GSSAPI здесь не реализован и пропускается).
    pub fn with_mechanisms(mut self, mechanisms: &[Mechanism]) -> Self {
        self.mechanisms = mechanisms
            .iter()
            .copied()
            .filter(|m| DEFAULT.contains(m))
            .collect();
        self
    }

    /// Адрес discovery поставщика OIDC для ответа-ошибки OAUTHBEARER (`openid-configuration`, RFC 7628 3.2.2).
    pub fn with_openid_configuration(mut self, url: impl Into<String>) -> Self {
        self.openid_configuration = Some(url.into());
        self
    }
}

impl SaslServer for AuthSasl {
    fn mechanisms(&self, tls: bool) -> Vec<Mechanism> {
        self.mechanisms
            .iter()
            .copied()
            .filter(|m| tls || !m.plaintext())
            .collect()
    }

    fn start(&self, mechanism: Mechanism, ctx: &SaslContext) -> Result<Box<dyn SaslSession>> {
        if !self.mechanisms(ctx.tls).contains(&mechanism) {
            return Err(Error::Invalid(format!(
                "SASL mechanism {mechanism} is not available"
            )));
        }
        Ok(Box::new(Exchange {
            authn: self.authn.clone(),
            mechanism,
            openid_configuration: self.openid_configuration.clone(),
            state: State::Start,
        }))
    }
}

/// Учётные данные, извлечённые механизмом.
#[derive(Default)]
struct Captured {
    authid: Option<String>,
    authzid: Option<String>,
    password: Option<Vec<u8>>,
    token: Option<String>,
}

impl Drop for Captured {
    fn drop(&mut self) {
        if let Some(p) = &mut self.password {
            p.fill(0);
        }
    }
}

/// Результат обмена `rsasl` — учётные данные для асинхронной проверки.
struct Capture;

impl Validation for Capture {
    type Value = Captured;
}

/// Обратный вызов `rsasl`: принимает токен (или отвечает ошибкой при повторном разборе) и забирает данные.
struct Callback {
    /// JSON ошибки для OAUTHBEARER / XOAUTH2: `Some` — повторный разбор после отказа.
    reject: Option<String>,
}

impl SessionCallback for Callback {
    fn callback(
        &self,
        _: &SessionData,
        _: &Context,
        request: &mut Request<'_>,
    ) -> std::result::Result<(), SessionError> {
        match &self.reject {
            None => {
                request.satisfy::<OAuthBearerValidate>(&Ok(()))?;
                request.satisfy::<XOAuth2Validate>(&Ok(()))?;
            }
            Some(json) => {
                let err: OAuthBearerError<'_> = serde_json::from_str(json)
                    .unwrap_or(OAuthBearerError::new("invalid_token", None, None));
                request.satisfy::<OAuthBearerValidate>(&Err(err))?;
                request.satisfy::<XOAuth2Validate>(&Err(json.as_str()))?;
            }
        }
        Ok(())
    }

    fn validate(
        &self,
        _: &SessionData,
        context: &Context,
        validate: &mut Validate<'_>,
    ) -> std::result::Result<(), ValidationError> {
        validate.finalize::<Capture>(Captured {
            authid: context.get_ref::<AuthId>().map(str::to_owned),
            authzid: context
                .get_ref::<AuthzId>()
                .filter(|z| !z.is_empty())
                .map(str::to_owned),
            password: context.get_ref::<Password>().map(<[u8]>::to_vec),
            token: context.get_ref::<OAuthBearerToken>().map(str::to_owned),
        });
        Ok(())
    }
}

fn rsasl_session(mechanism: Mechanism, reject: Option<String>) -> Result<Session<Capture>> {
    let config: Arc<SASLConfig> = SASLConfig::builder()
        .with_default_mechanisms()
        .with_callback(Callback { reject })
        .map_err(|e| Error::Internal(format!("SASL: {e}")))?;
    let name = Mechname::parse(mechanism.name().as_bytes())
        .map_err(|e| Error::Internal(format!("SASL: {e}")))?;
    SASLServer::<Capture>::new(config)
        .start_suggested(name)
        .map_err(|e| Error::Internal(format!("SASL {mechanism}: {e}")))
}

enum State {
    /// Ждём первого ответа.
    Start,
    /// Обмен `rsasl` идёт (LOGIN: имя → пароль).
    Running(Box<Session<Capture>>),
    /// Отправлен вызов с ошибкой токена; ответ клиента завершает обмен отказом.
    Rejected,
    /// Обмен завершён.
    Done,
}

/// Один обмен SASL.
struct Exchange {
    authn: Arc<dyn Authenticator>,
    mechanism: Mechanism,
    openid_configuration: Option<String>,
    state: State,
}

impl Exchange {
    /// Шаг механизма `rsasl`: вызов клиенту или извлечённые данные.
    fn run(
        session: &mut Session<Capture>,
        input: Option<&[u8]>,
    ) -> Result<std::result::Result<Vec<u8>, Captured>> {
        let mut out = Vec::new();
        let state = session
            .step(input, &mut out)
            .map_err(|e| Error::Invalid(format!("SASL: {e}")))?;
        if state.is_running() {
            return Ok(Ok(out));
        }
        session
            .validation()
            .map(Err)
            .ok_or_else(|| Error::invalid("SASL: incomplete exchange"))
    }

    /// Ответ-ошибка OAUTHBEARER (RFC 7628 3.2.2) / XOAUTH2.
    fn error_json(&self) -> String {
        match self.mechanism {
            Mechanism::Xoauth2 => r#"{"status":"401","schemes":"bearer"}"#.to_owned(),
            _ => {
                let mut v = serde_json::json!({ "status": "invalid_token" });
                if let Some(url) = &self.openid_configuration {
                    v["openid-configuration"] = url.clone().into();
                }
                v.to_string()
            }
        }
    }

    async fn finish(&mut self, captured: Captured, input: &[u8]) -> Result<SaslStep> {
        let token = matches!(self.mechanism, Mechanism::OauthBearer | Mechanism::Xoauth2);
        let (credentials, authzid) = if token {
            // OAUTHBEARER: `auth=Bearer <токен>` (RFC 6750 2.1); XOAUTH2 префикс снимает rsasl.
            let raw = captured.token.as_deref().unwrap_or_default();
            let t = match raw.split_once(' ') {
                Some((scheme, t)) if scheme.eq_ignore_ascii_case("bearer") => t,
                _ if self.mechanism == Mechanism::Xoauth2 => raw,
                _ => return Err(Error::invalid("OAUTHBEARER: auth is not a Bearer token")),
            };
            let who = captured.authzid.clone().or_else(|| captured.authid.clone());
            (Credentials::Bearer(t.trim().to_owned()), who)
        } else {
            let login = captured.authid.clone().unwrap_or_default();
            let secret = String::from_utf8(captured.password.clone().unwrap_or_default())
                .map_err(|_| Error::invalid("SASL: password is not UTF-8"))?;
            (
                Credentials::Password { login, secret },
                captured.authzid.clone(),
            )
        };
        match self.authn.verify(credentials).await {
            Ok(p) => {
                self.state = State::Done;
                Ok(if authorized(&p, authzid.as_deref()) {
                    SaslStep::Done(p)
                } else {
                    SaslStep::Fail(SaslFailure::Authorization)
                })
            }
            Err(Error::Forbidden) if token => {
                // Тот же механизм пишет вызов с ошибкой: повторный разбор с отказом в обратном вызове.
                let mut s = rsasl_session(self.mechanism, Some(self.error_json()))?;
                let challenge = match Self::run(&mut s, Some(input))? {
                    Ok(c) => c,
                    Err(_) => self.error_json().into_bytes(),
                };
                self.state = State::Rejected;
                Ok(SaslStep::Challenge(challenge))
            }
            Err(Error::Forbidden) => {
                self.state = State::Done;
                Ok(SaslStep::Fail(SaslFailure::Authentication))
            }
            Err(e) => {
                self.state = State::Done;
                Err(e)
            }
        }
    }
}

/// `authzid` пуст или совпадает с именем входа субъекта.
fn authorized(p: &Principal, authzid: Option<&str>) -> bool {
    authzid.is_none_or(|z| z.eq_ignore_ascii_case(&p.login))
}

#[async_trait]
impl SaslSession for Exchange {
    async fn step(&mut self, response: Option<&[u8]>) -> Result<SaslStep> {
        let (captured, input) = match std::mem::replace(&mut self.state, State::Done) {
            State::Start => {
                let mut s = Box::new(rsasl_session(self.mechanism, None)?);
                match (self.mechanism, response) {
                    // Нет начального ответа: пустой вызов, данные — следующим шагом.
                    (Mechanism::Login, None | Some([])) => {}
                    (_, None) => {
                        self.state = State::Start;
                        return Ok(SaslStep::Challenge(Vec::new()));
                    }
                    // LOGIN с начальным ответом (имя входа, `AUTH LOGIN <base64>`): вызов «User Name» пропущен.
                    (Mechanism::Login, Some(_)) => {
                        let _user_name = Self::run(&mut s, None)?;
                    }
                    _ => {}
                }
                let input = match (self.mechanism, response) {
                    (Mechanism::Login, Some([])) => None,
                    (_, r) => r,
                };
                match Self::run(&mut s, input)? {
                    Ok(challenge) => {
                        self.state = State::Running(s);
                        return Ok(SaslStep::Challenge(challenge));
                    }
                    Err(c) => (c, input.unwrap_or_default().to_vec()),
                }
            }
            State::Running(mut s) => {
                let input = response.unwrap_or_default();
                match Self::run(&mut s, Some(input))? {
                    Ok(challenge) => {
                        self.state = State::Running(s);
                        return Ok(SaslStep::Challenge(challenge));
                    }
                    Err(c) => (c, input.to_vec()),
                }
            }
            State::Rejected => return Ok(SaslStep::Fail(SaslFailure::Authentication)),
            State::Done => return Err(Error::invalid("SASL: exchange is finished")),
        };
        self.finish(captured, &input).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use tp_contracts::{AccountId, TenantId};

    /// Проверка по таблице: пароль `tanstaaftanstaaf` у `tim`, `xipj3plmq` у `Kurt`; токен
    /// `vF9dft4qmTc2Nvb3RlckBhbHRhdmlzdGEuY29tCg==` — у `user@example.com`.
    struct Table;

    const TOKEN: &str = "vF9dft4qmTc2Nvb3RlckBhbHRhdmlzdGEuY29tCg==";

    fn principal(login: &str, scopes: Option<BTreeSet<String>>) -> Principal {
        Principal {
            tenant: TenantId::DEFAULT,
            account: AccountId::new(),
            login: login.into(),
            groups: BTreeSet::new(),
            scopes,
        }
    }

    #[async_trait]
    impl Authenticator for Table {
        async fn verify(&self, c: Credentials) -> Result<Principal> {
            match c {
                Credentials::Password { login, secret } => {
                    match (login.as_str(), secret.as_str()) {
                        ("tim", "tanstaaftanstaaf") | ("Kurt", "xipj3plmq") => {
                            Ok(principal(&login, None))
                        }
                        ("down", _) => Err(Error::Unavailable { retry_after: None }),
                        _ => Err(Error::Forbidden),
                    }
                }
                Credentials::Bearer(t) if t == TOKEN => {
                    Ok(principal("user@example.com", Some(BTreeSet::new())))
                }
                _ => Err(Error::Forbidden),
            }
        }
    }

    fn server() -> AuthSasl {
        AuthSasl::new(Arc::new(Table))
            .with_openid_configuration("https://example.com/.well-known/openid-configuration")
    }

    const TLS: SaslContext = SaslContext {
        service: "imap",
        tls: true,
    };

    fn start(m: Mechanism) -> Box<dyn SaslSession> {
        server().start(m, &TLS).expect("start")
    }

    fn done(step: SaslStep) -> String {
        match step {
            SaslStep::Done(p) => p.login,
            other => panic!("not done: {other:?}"),
        }
    }

    #[test]
    fn offered_mechanisms() {
        let s = server();
        assert_eq!(s.mechanisms(true), DEFAULT.to_vec());
        assert!(
            s.mechanisms(false).is_empty(),
            "без TLS — только не открытые"
        );
        let plain = SaslContext {
            service: "smtp",
            tls: false,
        };
        assert!(s.start(Mechanism::Plain, &plain).is_err());
        assert!(s.start(Mechanism::Gssapi, &TLS).is_err());
        let only = s.with_mechanisms(&[Mechanism::Plain, Mechanism::Gssapi]);
        assert_eq!(only.mechanisms(true), [Mechanism::Plain]);
    }

    /// RFC 4616, раздел 4.
    #[tokio::test]
    async fn plain_rfc4616() {
        let mut s = start(Mechanism::Plain);
        assert_eq!(
            done(
                s.step(Some(b"\0tim\0tanstaaftanstaaf"))
                    .await
                    .expect("step")
            ),
            "tim"
        );
        // authzid совпадает с authcid — тот же субъект.
        let mut s = start(Mechanism::Plain);
        assert_eq!(
            done(
                s.step(Some(b"tim\0tim\0tanstaaftanstaaf"))
                    .await
                    .expect("step")
            ),
            "tim"
        );
        // Kurt входит паролем и просит права Ursel — вход от имени другого не поддерживается.
        let mut s = start(Mechanism::Plain);
        assert_eq!(
            s.step(Some(b"Ursel\0Kurt\0xipj3plmq")).await,
            Ok(SaslStep::Fail(SaslFailure::Authorization))
        );
        // Без начального ответа: пустой вызов, затем сообщение; неверный пароль.
        let mut s = start(Mechanism::Plain);
        assert_eq!(s.step(None).await, Ok(SaslStep::Challenge(Vec::new())));
        assert_eq!(
            s.step(Some(b"\0tim\0wrong")).await,
            Ok(SaslStep::Fail(SaslFailure::Authentication))
        );
        assert!(
            s.step(Some(b"\0tim\0wrong")).await.is_err(),
            "обмен завершён"
        );
        // Нарушение формата — Invalid.
        let mut s = start(Mechanism::Plain);
        assert!(matches!(s.step(Some(b"tim")).await, Err(Error::Invalid(_))));
        // Каталог недоступен — ошибка, а не отказ.
        let mut s = start(Mechanism::Plain);
        assert_eq!(
            s.step(Some(b"\0down\0x")).await,
            Err(Error::Unavailable { retry_after: None })
        );
    }

    #[tokio::test]
    async fn login_mechanism() {
        // S: "User Name" C: tim S: "Password" C: пароль.
        let mut s = start(Mechanism::Login);
        let SaslStep::Challenge(c) = s.step(None).await.expect("user") else {
            panic!("challenge")
        };
        assert!(c.starts_with(b"User Name"));
        let SaslStep::Challenge(c) = s.step(Some(b"tim")).await.expect("pass") else {
            panic!("challenge")
        };
        assert!(c.starts_with(b"Password"));
        assert_eq!(
            done(s.step(Some(b"tanstaaftanstaaf")).await.expect("done")),
            "tim"
        );
        // Начальный ответ с именем (`AUTH LOGIN dGlt`): сразу вызов пароля.
        let mut s = start(Mechanism::Login);
        let SaslStep::Challenge(c) = s.step(Some(b"tim")).await.expect("pass") else {
            panic!("challenge")
        };
        assert!(c.starts_with(b"Password"));
        assert_eq!(
            s.step(Some(b"bad")).await,
            Ok(SaslStep::Fail(SaslFailure::Authentication))
        );
    }

    /// RFC 7628, раздел 4.1 (успех) и 4.3 (ошибка токена).
    #[tokio::test]
    async fn oauthbearer_rfc7628() {
        let ok = format!(
            "n,a=user@example.com,\x01host=server.example.com\x01port=143\x01auth=Bearer {TOKEN}\x01\x01"
        );
        let mut s = start(Mechanism::OauthBearer);
        assert_eq!(
            done(s.step(Some(ok.as_bytes())).await.expect("ok")),
            "user@example.com"
        );
        // Без a= — субъект из токена.
        let mut s = start(Mechanism::OauthBearer);
        let no_a = format!("n,,\x01auth=Bearer {TOKEN}\x01\x01");
        assert_eq!(
            done(s.step(Some(no_a.as_bytes())).await.expect("ok")),
            "user@example.com"
        );
        // a= другого субъекта — Authorization.
        let mut s = start(Mechanism::OauthBearer);
        let other = format!("n,a=boss@example.com,\x01auth=Bearer {TOKEN}\x01\x01");
        assert_eq!(
            s.step(Some(other.as_bytes())).await,
            Ok(SaslStep::Fail(SaslFailure::Authorization))
        );

        // Неверный токен: вызов с JSON, клиент отвечает %x01, отказ.
        let bad = "n,a=user@example.com,\x01host=server.example.com\x01port=143\x01auth=Bearer vF9dft4qmTc2Nvb3RlckBhdHRhdmlzdGEuY29tCg==\x01\x01";
        let mut s = start(Mechanism::OauthBearer);
        let SaslStep::Challenge(c) = s.step(Some(bad.as_bytes())).await.expect("err") else {
            panic!("challenge")
        };
        let v: serde_json::Value = serde_json::from_slice(&c).expect("json");
        assert_eq!(v["status"], "invalid_token");
        assert_eq!(
            v["openid-configuration"],
            "https://example.com/.well-known/openid-configuration"
        );
        assert_eq!(
            s.step(Some(b"\x01")).await,
            Ok(SaslStep::Fail(SaslFailure::Authentication))
        );
        // Нарушение формата (нет завершающих %x01%x01, не Bearer).
        let mut s = start(Mechanism::OauthBearer);
        assert!(matches!(
            s.step(Some(b"n,,\x01auth=Bearer x")).await,
            Err(Error::Invalid(_))
        ));
        let mut s = start(Mechanism::OauthBearer);
        assert!(matches!(
            s.step(Some(b"n,,\x01auth=Basic x\x01\x01")).await,
            Err(Error::Invalid(_))
        ));
    }

    /// Пример Google (developers.google.com/gmail/imap/xoauth2-protocol).
    #[tokio::test]
    async fn xoauth2() {
        let ok = format!("user=user@example.com\x01auth=Bearer {TOKEN}\x01\x01");
        let mut s = start(Mechanism::Xoauth2);
        assert_eq!(
            done(s.step(Some(ok.as_bytes())).await.expect("ok")),
            "user@example.com"
        );
        let other = format!("user=boss@example.com\x01auth=Bearer {TOKEN}\x01\x01");
        let mut s = start(Mechanism::Xoauth2);
        assert_eq!(
            s.step(Some(other.as_bytes())).await,
            Ok(SaslStep::Fail(SaslFailure::Authorization))
        );
        let bad = "user=user@example.com\x01auth=Bearer nope\x01\x01";
        let mut s = start(Mechanism::Xoauth2);
        let SaslStep::Challenge(c) = s.step(Some(bad.as_bytes())).await.expect("err") else {
            panic!("challenge")
        };
        let v: serde_json::Value = serde_json::from_slice(&c).expect("json");
        assert_eq!(v["status"], "401");
        assert_eq!(
            s.step(Some(b"")).await,
            Ok(SaslStep::Fail(SaslFailure::Authentication))
        );
    }
}
