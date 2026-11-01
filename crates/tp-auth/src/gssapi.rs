//! Kerberos: механизм SASL GSSAPI (RFC 4752; SRS 5.1) для smtp и imap.
//!
//! Установление контекста Kerberos — системная MIT Kerberos через `libgssapi` (feature `gssapi`,
//! [`Gssapi::system`]); здесь — только обёртка SASL поверх готового контекста (RFC 4752 3.1): после
//! установления сервер предлагает слои защиты, клиент выбирает и передаёт `authzid`. Слои целостности и
//! конфиденциальности не предлагаются — канал защищает TLS. Механизм GSSAPI из `rsasl` не подходит: его
//! сервер не сообщает принципал клиента и отвергает ответ клиента с `authzid`.
//!
//! Принципал `имя@ОБЛАСТЬ` сопоставляется с учётной записью [`KerberosAuthenticator`]: имя входа —
//! `имя@домен`, домен — из `gssapi.realms` (по умолчанию — область в нижнем регистре). Принципалы служб
//! (`имя/узел@ОБЛАСТЬ`) не принимаются. Вход от имени другого субъекта (`authzid`) не поддерживается.
//!
//! Keytab службы MIT Kerberos читает из переменной окружения `KRB5_KTNAME` (`libgssapi` не даёт задать
//! файл вызовом), поэтому `gssapi.keytab` — проверка: файл есть и `KRB5_KTNAME` указывает на него.

use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tp_contracts::auth::{Authenticator, Credentials, Principal};
use tp_contracts::sasl::{SaslFailure, SaslSession, SaslStep};
use tp_contracts::{Error, Result};

use crate::Accounts;

/// Слой защиты «без защиты» (RFC 4752 3.3).
const NO_SECURITY_LAYER: u8 = 0x01;

/// Секция Kerberos (`auth.gssapi`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct GssapiConfig {
    /// Keytab служб (`smtp/узел`, `imap/узел`); не задан — keytab MIT по умолчанию или `KRB5_KTNAME`.
    pub keytab: Option<PathBuf>,
    /// Узел в принципале службы (`<служба>/<узел>@ОБЛАСТЬ`); не задан — любой ключ из keytab.
    pub hostname: Option<String>,
    /// Область Kerberos (верхний регистр) → почтовый домен имени входа; нет области — она в нижнем регистре.
    pub realms: BTreeMap<String, String>,
}

impl GssapiConfig {
    /// Проверяет секцию: `keytab` — существующий файл, на который указывает `KRB5_KTNAME`.
    pub fn check(&self) -> Result<()> {
        if let Some(path) = &self.keytab {
            if !path.is_file() {
                return Err(Error::Invalid(format!(
                    "gssapi.keytab: {} is not a file",
                    path.display()
                )));
            }
            let env = std::env::var_os("KRB5_KTNAME").map(PathBuf::from);
            let file = |p: PathBuf| match p.to_str().and_then(|s| s.strip_prefix("FILE:")) {
                Some(s) => PathBuf::from(s),
                None => p,
            };
            if env.map(file).as_deref() != Some(path.as_path()) {
                return Err(Error::Invalid(format!(
                    "gssapi.keytab: set KRB5_KTNAME=FILE:{} for the process",
                    path.display()
                )));
            }
        }
        Ok(())
    }

    /// Имя входа для принципала `имя@ОБЛАСТЬ`; принципал службы или без области — `None`.
    fn login(&self, principal: &str) -> Option<String> {
        let (name, realm) = principal.rsplit_once('@')?;
        if name.is_empty() || realm.is_empty() || name.contains('/') {
            return None;
        }
        let domain = self
            .realms
            .get(realm)
            .cloned()
            .unwrap_or_else(|| realm.to_ascii_lowercase());
        Some(format!("{name}@{domain}"))
    }
}

/// Контекст безопасности стороны сервера (GSS_Accept_sec_context, RFC 2743).
pub(crate) trait Acceptor: Send {
    /// Шаг установления: токен клиенту (может быть пустым) и признак «контекст установлен».
    fn step(&mut self, token: &[u8]) -> Result<(Vec<u8>, bool)>;
    /// GSS_Wrap без шифрования.
    fn wrap(&mut self, msg: &[u8]) -> Result<Vec<u8>>;
    /// GSS_Unwrap.
    fn unwrap(&mut self, msg: &[u8]) -> Result<Vec<u8>>;
    /// Принципал клиента установленного контекста (`имя@ОБЛАСТЬ`).
    fn client(&self) -> Result<String>;
}

/// Источник контекстов для службы (`smtp`, `imap`).
pub(crate) trait Acceptors: Send + Sync + 'static {
    fn acceptor(&self, service: &str) -> Result<Box<dyn Acceptor>>;
}

/// Механизм GSSAPI для [`AuthSasl::with_gssapi`](crate::AuthSasl::with_gssapi).
#[derive(Clone)]
pub struct Gssapi {
    acceptors: Arc<dyn Acceptors>,
}

impl fmt::Debug for Gssapi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Gssapi").finish_non_exhaustive()
    }
}

impl Gssapi {
    /// Системная MIT Kerberos по секции `cfg` (проверяется [`GssapiConfig::check`]).
    #[cfg(feature = "gssapi")]
    pub fn system(cfg: &GssapiConfig) -> Result<Self> {
        cfg.check()?;
        Ok(Gssapi {
            acceptors: Arc::new(system::System {
                hostname: cfg.hostname.clone(),
            }),
        })
    }

    #[cfg(test)]
    pub(crate) fn with_acceptors(acceptors: Arc<dyn Acceptors>) -> Self {
        Gssapi { acceptors }
    }

    /// Сеанс обмена для службы `service`; принципал проверяет `authn` ([`Credentials::Gssapi`]).
    pub(crate) fn session(
        &self,
        service: &str,
        authn: Arc<dyn Authenticator>,
    ) -> Result<Box<dyn SaslSession>> {
        Ok(Box::new(Exchange {
            authn,
            state: State::Accept(self.acceptors.acceptor(service)?),
        }))
    }
}

/// [`Authenticator`] для [`Credentials::Gssapi`]: принципал → включённая учётная запись ([`GssapiConfig`]).
/// Иные учётные данные, принципал службы, нет или отключена запись — `Forbidden`.
#[derive(Debug, Clone)]
pub struct KerberosAuthenticator {
    accounts: Accounts,
    cfg: GssapiConfig,
}

impl KerberosAuthenticator {
    /// Учётные записи `accounts`, сопоставление областей — `cfg.realms`.
    pub fn new(accounts: Accounts, cfg: GssapiConfig) -> Self {
        KerberosAuthenticator { accounts, cfg }
    }
}

#[async_trait]
impl Authenticator for KerberosAuthenticator {
    async fn verify(&self, credentials: Credentials) -> Result<Principal> {
        let Credentials::Gssapi { principal, .. } = credentials else {
            return Err(Error::Forbidden);
        };
        let login = self.cfg.login(&principal).ok_or(Error::Forbidden)?;
        let account = self.accounts.enabled(&login).await?;
        self.accounts.principal(account, None).await
    }
}

enum State {
    /// Установление контекста.
    Accept(Box<dyn Acceptor>),
    /// Контекст установлен с последним токеном; ждём пустого ответа клиента.
    Ack(Box<dyn Acceptor>),
    /// Отправлены слои защиты; ждём выбора клиента и `authzid`.
    Layer(Box<dyn Acceptor>),
    /// Обмен завершён.
    Done,
}

/// Один обмен GSSAPI.
struct Exchange {
    authn: Arc<dyn Authenticator>,
    state: State,
}

impl Exchange {
    /// Предложение слоёв (RFC 4752 3.1): только «без защиты», размер буфера 0.
    fn offer(&mut self, mut ctx: Box<dyn Acceptor>) -> Result<SaslStep> {
        let msg = ctx.wrap(&[NO_SECURITY_LAYER, 0, 0, 0])?;
        self.state = State::Layer(ctx);
        Ok(SaslStep::Challenge(msg))
    }

    async fn finish(&mut self, mut ctx: Box<dyn Acceptor>, response: &[u8]) -> Result<SaslStep> {
        let Ok(msg) = ctx.unwrap(response) else {
            return Ok(SaslStep::Fail(SaslFailure::Authentication));
        };
        let (Some(&layer), Some(authzid)) = (msg.first(), msg.get(4..)) else {
            return Err(Error::invalid("GSSAPI: malformed security layer message"));
        };
        if layer != NO_SECURITY_LAYER {
            return Err(Error::invalid("GSSAPI: unsupported security layer"));
        }
        let authzid = std::str::from_utf8(authzid)
            .map_err(|_| Error::invalid("GSSAPI: authzid is not UTF-8"))?;
        let authzid = (!authzid.is_empty()).then(|| authzid.to_owned());
        let principal = ctx.client()?;
        let credentials = Credentials::Gssapi {
            principal,
            authzid: authzid.clone(),
        };
        match self.authn.verify(credentials).await {
            Ok(p) if authzid.is_none_or(|z| z.eq_ignore_ascii_case(&p.login)) => {
                Ok(SaslStep::Done(p))
            }
            Ok(_) => Ok(SaslStep::Fail(SaslFailure::Authorization)),
            Err(Error::Forbidden) => Ok(SaslStep::Fail(SaslFailure::Authentication)),
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl SaslSession for Exchange {
    async fn step(&mut self, response: Option<&[u8]>) -> Result<SaslStep> {
        match std::mem::replace(&mut self.state, State::Done) {
            State::Accept(ctx) => {
                let token = match response {
                    // Нет начального ответа: пустой вызов, токен — следующим шагом.
                    None => {
                        self.state = State::Accept(ctx);
                        return Ok(SaslStep::Challenge(Vec::new()));
                    }
                    Some([]) => return Err(Error::invalid("GSSAPI: empty context token")),
                    Some(t) => t.to_vec(),
                };
                // GSS_Accept_sec_context читает keytab и кэш повторов — блокирующий вызов.
                let (ctx, res) = tokio::task::spawn_blocking(move || {
                    let mut ctx = ctx;
                    let res = ctx.step(&token);
                    (ctx, res)
                })
                .await
                .map_err(|e| Error::Internal(format!("GSSAPI: {e}")))?;
                let Ok((out, done)) = res else {
                    // Неверный или чужой билет, истёкший билет — отказ без подробностей клиенту.
                    tracing::debug!("GSSAPI: security context rejected");
                    return Ok(SaslStep::Fail(SaslFailure::Authentication));
                };
                match (done, out.is_empty()) {
                    (false, _) => {
                        self.state = State::Accept(ctx);
                        Ok(SaslStep::Challenge(out))
                    }
                    (true, false) => {
                        self.state = State::Ack(ctx);
                        Ok(SaslStep::Challenge(out))
                    }
                    (true, true) => self.offer(ctx),
                }
            }
            State::Ack(ctx) => self.offer(ctx),
            State::Layer(ctx) => self.finish(ctx, response.unwrap_or_default()).await,
            State::Done => Err(Error::invalid("GSSAPI: exchange is finished")),
        }
    }
}

/// Системная MIT Kerberos через `libgssapi`.
#[cfg(feature = "gssapi")]
mod system {
    use libgssapi::context::{SecurityContext, ServerCtx};
    use libgssapi::credential::{Cred, CredUsage};
    use libgssapi::name::Name;
    use libgssapi::oid::{GSS_MECH_KRB5, GSS_NT_HOSTBASED_SERVICE, OidSet};
    use tp_contracts::{Error, Result};

    use super::{Acceptor, Acceptors};

    fn gss(e: libgssapi::error::Error) -> Error {
        Error::Internal(format!("GSSAPI: {e}"))
    }

    pub(super) struct System {
        pub(super) hostname: Option<String>,
    }

    impl Acceptors for System {
        fn acceptor(&self, service: &str) -> Result<Box<dyn Acceptor>> {
            Ok(Box::new(SystemAcceptor {
                target: self.hostname.as_ref().map(|h| format!("{service}@{h}")),
                ctx: None,
            }))
        }
    }

    struct SystemAcceptor {
        /// Служба `<служба>@<узел>` (GSS_C_NT_HOSTBASED_SERVICE); `None` — любой ключ keytab.
        target: Option<String>,
        ctx: Option<ServerCtx>,
    }

    impl SystemAcceptor {
        fn ctx(&mut self) -> Result<&mut ServerCtx> {
            if self.ctx.is_none() {
                let mut mechs = OidSet::new();
                mechs.add(GSS_MECH_KRB5).map_err(gss)?;
                let name = match &self.target {
                    Some(t) => {
                        Some(Name::new(t.as_bytes(), Some(GSS_NT_HOSTBASED_SERVICE)).map_err(gss)?)
                    }
                    None => None,
                };
                let cred = Cred::acquire(name.as_ref(), None, CredUsage::Accept, Some(&mechs))
                    .map_err(gss)?;
                self.ctx = Some(ServerCtx::new(Some(cred)));
            }
            self.ctx
                .as_mut()
                .ok_or_else(|| Error::Internal("GSSAPI: no context".into()))
        }
    }

    impl Acceptor for SystemAcceptor {
        fn step(&mut self, token: &[u8]) -> Result<(Vec<u8>, bool)> {
            let ctx = self.ctx()?;
            let out = ctx.step(token, None).map_err(gss)?;
            Ok((
                out.map(|b| b.to_vec()).unwrap_or_default(),
                ctx.is_complete(),
            ))
        }

        fn wrap(&mut self, msg: &[u8]) -> Result<Vec<u8>> {
            Ok(self.ctx()?.wrap(false, msg).map_err(gss)?.to_vec())
        }

        fn unwrap(&mut self, msg: &[u8]) -> Result<Vec<u8>> {
            Ok(self.ctx()?.unwrap(msg).map_err(gss)?.to_vec())
        }

        fn client(&self) -> Result<String> {
            let ctx = self
                .ctx
                .as_ref()
                .ok_or_else(|| Error::Internal("GSSAPI: no context".into()))?;
            let name = ctx.source_name().map_err(gss)?;
            let shown = name.display_name().map_err(gss)?;
            String::from_utf8(shown.to_vec())
                .map_err(|_| Error::Internal("GSSAPI: principal is not UTF-8".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use tp_contracts::sasl::{Mechanism, SaslContext, SaslServer};

    use super::*;
    use crate::AuthSasl;
    use crate::testutil::accounts;

    /// Фейковый контекст: два токена клиента; «wrap» — префикс `W`, принципал задан.
    struct Fake {
        client: &'static str,
        steps: usize,
    }

    impl Acceptor for Fake {
        fn step(&mut self, token: &[u8]) -> Result<(Vec<u8>, bool)> {
            if token == b"bad" {
                return Err(Error::Internal("GSSAPI: bad ticket".into()));
            }
            self.steps += 1;
            Ok(match self.steps {
                1 => (b"srv1".to_vec(), false),
                _ => (b"mutual".to_vec(), true),
            })
        }

        fn wrap(&mut self, msg: &[u8]) -> Result<Vec<u8>> {
            Ok([b"W".as_slice(), msg].concat())
        }

        fn unwrap(&mut self, msg: &[u8]) -> Result<Vec<u8>> {
            msg.strip_prefix(b"W")
                .map(<[u8]>::to_vec)
                .ok_or_else(|| Error::Internal("GSSAPI: bad wrap".into()))
        }

        fn client(&self) -> Result<String> {
            Ok(self.client.to_owned())
        }
    }

    struct Factory(&'static str, Arc<Mutex<Vec<String>>>);

    impl Acceptors for Factory {
        fn acceptor(&self, service: &str) -> Result<Box<dyn Acceptor>> {
            self.1.lock().expect("lock").push(service.to_owned());
            Ok(Box::new(Fake {
                client: self.0,
                steps: 0,
            }))
        }
    }

    fn cfg() -> GssapiConfig {
        GssapiConfig {
            realms: BTreeMap::from([("CORP.EXAMPLE".to_owned(), "example.org".to_owned())]),
            ..GssapiConfig::default()
        }
    }

    async fn server(client: &'static str) -> (AuthSasl, Arc<Mutex<Vec<String>>>) {
        let (accounts, _) =
            accounts(&[("alice@example.org", true), ("bob@example.org", false)]).await;
        let log = Arc::default();
        let gss = Gssapi::with_acceptors(Arc::new(Factory(client, Arc::clone(&log))));
        let authn = Arc::new(KerberosAuthenticator::new(accounts, cfg()));
        (AuthSasl::new(authn).with_gssapi(gss), log)
    }

    const IMAP: SaslContext = SaslContext {
        service: "imap",
        tls: false,
    };

    /// Полный обмен RFC 4752: токены контекста, пустой ответ, слои, выбор клиента с authzid.
    async fn exchange(client: &'static str, choice: &[u8]) -> Result<SaslStep> {
        let (sasl, log) = server(client).await;
        assert_eq!(sasl.mechanisms(false), vec![Mechanism::Gssapi]);
        let mut s = sasl.start(Mechanism::Gssapi, &IMAP)?;
        assert_eq!(s.step(None).await?, SaslStep::Challenge(Vec::new()));
        assert_eq!(
            s.step(Some(b"t1")).await?,
            SaslStep::Challenge(b"srv1".to_vec())
        );
        assert_eq!(
            s.step(Some(b"t2")).await?,
            SaslStep::Challenge(b"mutual".to_vec())
        );
        assert_eq!(
            s.step(Some(b"")).await?,
            SaslStep::Challenge(b"W\x01\0\0\0".to_vec())
        );
        let res = s.step(Some(&[b"W".as_slice(), choice].concat())).await;
        assert_eq!(
            log.lock().expect("lock").first().map(String::as_str),
            Some("imap")
        );
        res
    }

    #[tokio::test]
    async fn rfc4752_exchange() {
        let SaslStep::Done(p) = exchange("alice@CORP.EXAMPLE", b"\x01\0\0\0")
            .await
            .expect("done")
        else {
            panic!("not done");
        };
        assert_eq!(p.login, "alice@example.org");
        assert_eq!(p.scopes, None);
        // authzid совпадает с именем входа — допустимо.
        assert!(matches!(
            exchange("alice@CORP.EXAMPLE", b"\x01\0\0\0Alice@example.org").await,
            Ok(SaslStep::Done(_))
        ));
    }

    #[tokio::test]
    async fn rejected() {
        let fail = |f| Ok(SaslStep::Fail(f));
        // Вход от имени другого.
        assert_eq!(
            exchange("alice@CORP.EXAMPLE", b"\x01\0\0\0bob@example.org").await,
            fail(SaslFailure::Authorization)
        );
        // Отключённая запись, нет записи, принципал службы.
        for who in [
            "bob@CORP.EXAMPLE",
            "carol@CORP.EXAMPLE",
            "imap/mx@CORP.EXAMPLE",
        ] {
            assert_eq!(
                exchange(who, b"\x01\0\0\0").await,
                fail(SaslFailure::Authentication),
                "{who}"
            );
        }
        // Слой защиты, который не предлагался; короткое сообщение.
        assert!(
            exchange("alice@CORP.EXAMPLE", b"\x04\xff\xff\xff")
                .await
                .is_err()
        );
        assert!(exchange("alice@CORP.EXAMPLE", b"\x01").await.is_err());

        // Неверный билет — отказ; пустой токен — нарушение формата.
        let (sasl, _) = server("alice@CORP.EXAMPLE").await;
        let mut s = sasl.start(Mechanism::Gssapi, &IMAP).expect("start");
        assert_eq!(
            s.step(Some(b"bad")).await,
            fail(SaslFailure::Authentication)
        );
        assert!(s.step(Some(b"t1")).await.is_err(), "session is finished");
        let mut s = sasl.start(Mechanism::Gssapi, &IMAP).expect("start");
        assert!(s.step(Some(b"")).await.is_err());
    }

    #[tokio::test]
    async fn not_offered_without_gssapi() {
        let (accounts, _) = accounts(&[]).await;
        let sasl = AuthSasl::new(Arc::new(KerberosAuthenticator::new(accounts, cfg())));
        assert!(!sasl.mechanisms(true).contains(&Mechanism::Gssapi));
        assert!(sasl.start(Mechanism::Gssapi, &IMAP).is_err());
    }

    #[test]
    fn principal_to_login() {
        let c = cfg();
        assert_eq!(
            c.login("alice@CORP.EXAMPLE").as_deref(),
            Some("alice@example.org")
        );
        assert_eq!(c.login("bob@OTHER.ORG").as_deref(), Some("bob@other.org"));
        for bad in ["alice", "@CORP.EXAMPLE", "alice@", "HTTP/mx@CORP.EXAMPLE"] {
            assert_eq!(c.login(bad), None, "{bad}");
        }
    }

    #[test]
    fn config() {
        assert_eq!(GssapiConfig::default().check(), Ok(()));
        let missing = GssapiConfig {
            keytab: Some(PathBuf::from("/nonexistent/thepost.keytab")),
            ..GssapiConfig::default()
        };
        assert!(missing.check().is_err());
        let parsed: GssapiConfig = serde_json::from_str(
            r#"{"hostname": "mx.example.org", "realms": {"CORP": "example.org"}}"#,
        )
        .expect("parse");
        assert_eq!(parsed.hostname.as_deref(), Some("mx.example.org"));
        assert!(serde_json::from_str::<GssapiConfig>(r#"{"bogus": 1}"#).is_err());
    }
}
