//! Интерфейс SASL для smtp (submission) и imap (CONTRACTS.md#Auth; SRS 5.1, RFC 4422).
//!
//! Механизмы (PLAIN, LOGIN, OAUTHBEARER, XOAUTH2, GSSAPI) реализует `tp-auth` поверх
//! [`Authenticator`](crate::auth::Authenticator). Протокол разбирает команду (`AUTH`, `AUTHENTICATE`),
//! кодирует и декодирует base64, обрабатывает отмену клиентом (`*`) и передаёт байты сеансу.
//!
//! Обмен PLAIN (RFC 4616) с начальным ответом:
//!
//! ```
//! use async_trait::async_trait;
//! use futures::executor::block_on;
//! use tp_contracts::auth::Principal;
//! use tp_contracts::sasl::{Mechanism, SaslContext, SaslFailure, SaslServer, SaslSession, SaslStep};
//! use tp_contracts::{AccountId, Error, Result, TenantId};
//!
//! struct Plain;
//!
//! #[async_trait]
//! impl SaslSession for Plain {
//!     async fn step(&mut self, response: Option<&[u8]>) -> Result<SaslStep> {
//!         // Нет начального ответа — пустой вызов, клиент присылает данные следующим шагом.
//!         let Some(msg) = response else { return Ok(SaslStep::Challenge(Vec::new())) };
//!         // [authzid] NUL authcid NUL passwd
//!         let mut it = msg.split(|b| *b == 0);
//!         let (Some(_authzid), Some(login), Some(pass), None) = (it.next(), it.next(), it.next(), it.next())
//!         else {
//!             return Err(Error::invalid("PLAIN: malformed message"));
//!         };
//!         if login == b"user" && pass == b"secret" {
//!             Ok(SaslStep::Done(Principal {
//!                 tenant: TenantId::DEFAULT,
//!                 account: AccountId::new(),
//!                 login: "user".into(),
//!                 groups: Default::default(),
//!                 scopes: None,
//!             }))
//!         } else {
//!             Ok(SaslStep::Fail(SaslFailure::Authentication))
//!         }
//!     }
//! }
//!
//! struct Server;
//!
//! impl SaslServer for Server {
//!     fn mechanisms(&self, tls: bool) -> Vec<Mechanism> {
//!         [Mechanism::Plain].into_iter().filter(|m| tls || !m.plaintext()).collect()
//!     }
//!
//!     fn start(&self, mechanism: Mechanism, ctx: &SaslContext) -> Result<Box<dyn SaslSession>> {
//!         if !self.mechanisms(ctx.tls).contains(&mechanism) {
//!             return Err(Error::invalid("mechanism not available"));
//!         }
//!         Ok(Box::new(Plain))
//!     }
//! }
//!
//! // C: AUTH PLAIN AHVzZXIAc2VjcmV0  (base64 "\0user\0secret" декодирует протокол)
//! let ctx = SaslContext { service: "smtp", tls: true, peer: Some([192, 0, 2, 7].into()) };
//! let mech: Mechanism = "plain".parse()?;
//! assert_eq!(Server.mechanisms(false), Vec::<Mechanism>::new());
//! let mut s = Server.start(mech, &ctx)?;
//! let SaslStep::Done(p) = block_on(s.step(Some(b"\0user\0secret")))? else { panic!("not done") };
//! assert_eq!(p.login, "user");
//!
//! // Без начального ответа: S: 334 (пустой вызов), C: base64 сообщения; неверный пароль — Fail.
//! let mut s = Server.start(mech, &ctx)?;
//! assert_eq!(block_on(s.step(None))?, SaslStep::Challenge(Vec::new()));
//! assert_eq!(block_on(s.step(Some(b"\0user\0wrong")))?, SaslStep::Fail(SaslFailure::Authentication));
//! # Ok::<(), Error>(())
//! ```

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::auth::Principal;
use crate::error::{Error, Result};

/// Механизм SASL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Mechanism {
    /// `PLAIN` (RFC 4616): пароль открытым текстом.
    Plain,
    /// `LOGIN` (draft-murchison-sasl-login): пароль открытым текстом, для старых клиентов.
    Login,
    /// `OAUTHBEARER` (RFC 7628): токен OpenID Connect.
    OauthBearer,
    /// `XOAUTH2` (Google, Microsoft): токен OAuth 2.0.
    Xoauth2,
    /// `GSSAPI` (RFC 4752): Kerberos.
    Gssapi,
}

impl Mechanism {
    /// Все механизмы.
    pub const ALL: [Mechanism; 5] = [
        Mechanism::Plain,
        Mechanism::Login,
        Mechanism::OauthBearer,
        Mechanism::Xoauth2,
        Mechanism::Gssapi,
    ];

    /// Имя механизма (IANA, верхний регистр).
    pub const fn name(self) -> &'static str {
        match self {
            Mechanism::Plain => "PLAIN",
            Mechanism::Login => "LOGIN",
            Mechanism::OauthBearer => "OAUTHBEARER",
            Mechanism::Xoauth2 => "XOAUTH2",
            Mechanism::Gssapi => "GSSAPI",
        }
    }

    /// Секрет передаётся открытым текстом (или токен на предъявителя): без TLS не предлагается.
    pub const fn plaintext(self) -> bool {
        !matches!(self, Mechanism::Gssapi)
    }
}

impl fmt::Display for Mechanism {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Mechanism {
    type Err = Error;

    /// Имя без учёта регистра; неизвестное — `Error::Invalid` (SMTP 504, IMAP `NO`).
    fn from_str(s: &str) -> Result<Self> {
        Mechanism::ALL
            .into_iter()
            .find(|m| m.name().eq_ignore_ascii_case(s))
            .ok_or_else(|| Error::Invalid(format!("unknown SASL mechanism: {s}")))
    }
}

/// Условия обмена.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SaslContext {
    /// Служба (`smtp`, `imap`): имя сервиса GSSAPI (RFC 4752), область OAUTHBEARER.
    pub service: &'static str,
    /// Соединение защищено TLS.
    pub tls: bool,
    /// Адрес клиента соединения (CTR-57; SRS 6.3): сеанс передаёт его в
    /// [`Authenticator::verify`](crate::auth::Authenticator::verify) — счётчики защиты от перебора. `None` — только
    /// когда адрес неизвестен (обмен без соединения клиента).
    pub peer: Option<IpAddr>,
}

/// Неуспешная аутентификация; причина клиенту не уточняется сверх кода (SMTP 535 5.7.8, IMAP RFC 5530).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SaslFailure {
    /// Неверные учётные данные или запись отключена (IMAP `AUTHENTICATIONFAILED`).
    Authentication,
    /// Учётные данные верны, но вход от имени `authzid` запрещён (IMAP `AUTHORIZATIONFAILED`).
    Authorization,
    /// Токен или пароль истёк (IMAP `EXPIRED`).
    Expired,
}

/// Шаг обмена SASL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaslStep {
    /// Вызов сервера (байты до base64), ждать ответа клиента.
    Challenge(Vec<u8>),
    /// Обмен завершён успешно.
    Done(Principal),
    /// Обмен завершён неуспешно; сеанс больше не используется.
    Fail(SaslFailure),
}

/// Сеанс одного обмена SASL.
#[async_trait]
pub trait SaslSession: Send {
    /// Обрабатывает ответ клиента. Первый вызов — начальный ответ: `None` — клиент его не прислал,
    /// `Some(&[])` — прислал пустой (`=`). Нарушение формата механизма — `Error::Invalid`;
    /// недоступность каталога, поставщика токенов — `Error::Unavailable` (временная ошибка протокола).
    async fn step(&mut self, response: Option<&[u8]>) -> Result<SaslStep>;
}

/// Механизмы SASL организации для smtp и imap. Реализация — `tp-auth`; фейк — `tp-testkit`.
pub trait SaslServer: Send + Sync + 'static {
    /// Предлагаемые механизмы (`EHLO`, `CAPABILITY`); без TLS — без [`Mechanism::plaintext`].
    fn mechanisms(&self, tls: bool) -> Vec<Mechanism>;

    /// Начинает обмен с клиентом `ctx.peer`; механизм не из `mechanisms(ctx.tls)` — `Error::Invalid`.
    fn start(&self, mechanism: Mechanism, ctx: &SaslContext) -> Result<Box<dyn SaslSession>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{Authenticator, Credentials};
    use futures::executor::block_on;
    use std::sync::{Arc, Mutex};

    #[test]
    fn object_safe() {
        fn _sasl(_: Arc<dyn SaslServer>) {}
        fn _session(_: Box<dyn SaslSession>) {}
    }

    /// Проверка, запоминающая адреса клиента.
    #[derive(Default)]
    struct Recording {
        peers: Mutex<Vec<Option<IpAddr>>>,
    }

    #[async_trait]
    impl Authenticator for Recording {
        async fn verify(&self, _: Credentials, peer: Option<IpAddr>) -> Result<Principal> {
            if let Ok(mut peers) = self.peers.lock() {
                peers.push(peer);
            }
            Err(Error::Forbidden)
        }
    }

    struct Session {
        auth: Arc<dyn Authenticator>,
        peer: Option<IpAddr>,
    }

    #[async_trait]
    impl SaslSession for Session {
        async fn step(&mut self, _: Option<&[u8]>) -> Result<SaslStep> {
            let credentials = Credentials::Bearer("t".into());
            match self.auth.verify(credentials, self.peer).await {
                Ok(p) => Ok(SaslStep::Done(p)),
                Err(Error::Forbidden) => Ok(SaslStep::Fail(SaslFailure::Authentication)),
                Err(e) => Err(e),
            }
        }
    }

    struct Server(Arc<dyn Authenticator>);

    impl SaslServer for Server {
        fn mechanisms(&self, _: bool) -> Vec<Mechanism> {
            vec![Mechanism::OauthBearer]
        }

        fn start(&self, _: Mechanism, ctx: &SaslContext) -> Result<Box<dyn SaslSession>> {
            Ok(Box::new(Session {
                auth: self.0.clone(),
                peer: ctx.peer,
            }))
        }
    }

    #[test]
    fn peer_reaches_verify() {
        // Адрес соединения из `SaslContext` доходит до `verify`; `None` — адрес неизвестен.
        let peer = Some(IpAddr::from([192, 0, 2, 7]));
        let auth = Arc::new(Recording::default());
        let server = Server(auth.clone());
        for peer in [peer, None] {
            let ctx = SaslContext {
                service: "imap",
                tls: true,
                peer,
            };
            let step = server
                .start(Mechanism::OauthBearer, &ctx)
                .map(|mut s| block_on(s.step(None)));
            assert_eq!(step, Ok(Ok(SaslStep::Fail(SaslFailure::Authentication))));
        }
        assert_eq!(
            auth.peers.lock().map(|p| p.clone()).ok(),
            Some(vec![peer, None])
        );
    }

    #[test]
    fn mechanism_names() {
        for m in Mechanism::ALL {
            assert_eq!(m.name().parse::<Mechanism>(), Ok(m));
            assert_eq!(m.name().to_lowercase().parse::<Mechanism>(), Ok(m));
            assert_eq!(m.to_string(), m.name());
        }
        assert!("CRAM-MD5".parse::<Mechanism>().is_err());
        assert!(Mechanism::Plain.plaintext() && !Mechanism::Gssapi.plaintext());
    }
}
