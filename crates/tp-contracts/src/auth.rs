//! Каталог, аутентификация и права (CONTRACTS.md#Directory, #Auth; SRS 5.1, 3.8).
//!
//! Реализации — `tp-auth` (LDAP / Active Directory, Kerberos, OAuth 2.0); модуль «Мультитенантность»
//! подключает каталог и поставщика удостоверений на организацию. Секреты (пароли, токены) не попадают
//! в `Debug` и логи.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::{BitOr, BitOrAssign, Sub};
use std::str::FromStr;

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};
use crate::ids::{AccountId, MailboxId, TenantId};

// ---------- Каталог ----------

/// Позиция инкрементальной синхронизации каталога (LDAP USN, cookie DirSync и т. п.); непрозрачна.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DirCursor(pub String);

/// Изменение записи каталога.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirChange<T> {
    /// Запись создана или изменена (полное состояние).
    Upsert(T),
    /// Запись удалена из каталога.
    Removed {
        /// Идентификатор в каталоге.
        external_id: String,
    },
}

/// Порция изменений каталога.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirPage<T> {
    /// Изменения.
    pub changes: Vec<DirChange<T>>,
    /// Позиция для следующего запроса.
    pub next: DirCursor,
    /// Получены не все изменения: повторить с `next`.
    pub has_more: bool,
}

/// Учётная запись в каталоге.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirAccount {
    /// Идентификатор в каталоге (`objectGUID` / `entryUUID`), постоянный.
    pub external_id: String,
    /// Имя входа.
    pub login: String,
    /// Отображаемое имя.
    pub display_name: String,
    /// Адреса: первый — основной, остальные — псевдонимы учётной записи.
    pub emails: Vec<String>,
    /// Вход разрешён.
    pub enabled: bool,
    /// Атрибуты адресной книги (GAL): имя из [`Directory::gal_attributes`] → значение.
    pub attributes: BTreeMap<String, String>,
}

/// Группа (список рассылки, группа безопасности).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirGroup {
    /// Идентификатор в каталоге.
    pub external_id: String,
    /// Имя группы (идентификатор в ACL).
    pub name: String,
    /// Адрес рассылки; `None` — группа без почты.
    pub email: Option<String>,
    /// Члены: `external_id` учётных записей и вложенных групп.
    pub members: Vec<String>,
}

/// Дополнительный адрес, не принадлежащий записи каталога напрямую (`postmaster@`, перенаправления).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirAlias {
    /// Адрес.
    pub address: String,
    /// `external_id` учётной записи или группы-получателя.
    pub target: String,
}

/// Почтовый домен организации.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirDomain {
    /// Имя домена (в нижнем регистре, A-label).
    pub name: String,
    /// Домен по умолчанию организации.
    pub default: bool,
}

/// Каталог организации. Учётные записи и группы — инкрементально; `cursor = None` — полная выгрузка.
/// Устаревший `cursor` — `Error::Conflict`: повторить полную выгрузку.
#[async_trait]
pub trait Directory: Send + Sync + 'static {
    /// Изменения учётных записей после `cursor`, не более `limit`.
    async fn accounts_since(
        &self,
        tenant: TenantId,
        cursor: Option<&DirCursor>,
        limit: usize,
    ) -> Result<DirPage<DirAccount>>;

    /// Изменения групп после `cursor`, не более `limit`.
    async fn groups_since(
        &self,
        tenant: TenantId,
        cursor: Option<&DirCursor>,
        limit: usize,
    ) -> Result<DirPage<DirGroup>>;

    /// Дополнительные адреса (полный список).
    async fn aliases(&self, tenant: TenantId) -> Result<Vec<DirAlias>>;

    /// Почтовые домены (полный список).
    async fn domains(&self, tenant: TenantId) -> Result<Vec<DirDomain>>;

    /// Имена атрибутов, публикуемых в адресной книге (`title`, `department`, `telephoneNumber`, ...).
    async fn gal_attributes(&self, tenant: TenantId) -> Result<Vec<String>>;
}

// ---------- Аутентификация ----------

/// Предъявленные учётные данные.
#[derive(Clone, PartialEq, Eq)]
pub enum Credentials {
    /// Имя входа и пароль (PLAIN, LOGIN, Basic).
    Password {
        /// Имя входа; организация определяется по домену.
        login: String,
        /// Пароль.
        secret: String,
    },
    /// Токен доступа OAuth 2.0 (OAUTHBEARER, XOAUTH2, Bearer).
    Bearer(String),
    /// Завершённый обмен GSSAPI (Kerberos): принципал проверен механизмом SASL.
    Gssapi {
        /// Принципал Kerberos (`user@REALM`).
        principal: String,
        /// Запрошенное имя авторизации, если отличается.
        authzid: Option<String>,
    },
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Credentials::Password { login, .. } => f
                .debug_struct("Password")
                .field("login", login)
                .finish_non_exhaustive(),
            Credentials::Bearer(_) => f.write_str("Bearer(..)"),
            Credentials::Gssapi { principal, authzid } => f
                .debug_struct("Gssapi")
                .field("principal", principal)
                .field("authzid", authzid)
                .finish(),
        }
    }
}

/// Аутентифицированный субъект.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Principal {
    /// Организация.
    pub tenant: TenantId,
    /// Учётная запись.
    pub account: AccountId,
    /// Имя входа (идентификатор в ACL).
    pub login: String,
    /// Группы с учётом вложенности (имена, идентификаторы в ACL).
    pub groups: BTreeSet<String>,
    /// Области доступа токена OAuth; `None` — без ограничений (пароль, Kerberos).
    pub scopes: Option<BTreeSet<String>>,
}

impl Principal {
    /// Область доступа разрешена.
    pub fn has_scope(&self, scope: &str) -> bool {
        self.scopes.as_ref().is_none_or(|s| s.contains(scope))
    }
}

/// Проверка учётных данных. Неверные данные, отключённая запись — `Error::Forbidden`
/// (без уточнения причины клиенту).
#[async_trait]
pub trait Authenticator: Send + Sync + 'static {
    /// Проверяет учётные данные и возвращает субъекта.
    async fn verify(&self, credentials: Credentials) -> Result<Principal>;
}

/// Шаг обмена SASL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaslStep {
    /// Вызов сервера (байты до base64), ждать ответа клиента.
    Challenge(Vec<u8>),
    /// Обмен завершён успешно.
    Done(Principal),
}

/// Сеанс одного обмена SASL.
#[async_trait]
pub trait SaslSession: Send {
    /// Обрабатывает ответ клиента (первый вызов — начальный ответ, возможно пустой).
    /// Неверные данные — `Error::Forbidden`, нарушение протокола — `Error::Invalid`.
    async fn step(&mut self, response: &[u8]) -> Result<SaslStep>;
}

/// Механизмы SASL (PLAIN, LOGIN, OAUTHBEARER, XOAUTH2, GSSAPI) для smtp, imap.
pub trait SaslServer: Send + Sync + 'static {
    /// Доступные механизмы; без TLS — без механизмов с открытым паролем.
    fn mechanisms(&self, tls: bool) -> Vec<&'static str>;

    /// Начинает обмен; неизвестный механизм — `Error::Invalid`.
    fn start(&self, mechanism: &str) -> Result<Box<dyn SaslSession>>;
}

// ---------- Права ----------

/// Права доступа к папке (RFC 4314). Строка — подмножество `lrswipkxtea` в каноническом порядке.
///
/// Устаревшие права `c`, `d` (RFC 2086) разворачивает протокол IMAP; здесь они — ошибка разбора.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rights(u16);

/// Буквы прав в каноническом порядке; бит `i` — буква `i`.
const LETTERS: &[u8; 11] = b"lrswipkxtea";

impl Rights {
    /// Нет прав.
    pub const NONE: Rights = Rights(0);
    /// `l` — папка видна в LIST.
    pub const LOOKUP: Rights = Rights(1 << 0);
    /// `r` — SELECT, FETCH, SEARCH, COPY из папки.
    pub const READ: Rights = Rights(1 << 1);
    /// `s` — флаг `\Seen` сохраняется.
    pub const SEEN: Rights = Rights(1 << 2);
    /// `w` — прочие флаги и ключевые слова.
    pub const WRITE: Rights = Rights(1 << 3);
    /// `i` — APPEND, COPY/MOVE в папку.
    pub const INSERT: Rights = Rights(1 << 4);
    /// `p` — отправка в папку (адрес `+папка`).
    pub const POST: Rights = Rights(1 << 5);
    /// `k` — создание подпапок.
    pub const CREATE: Rights = Rights(1 << 6);
    /// `x` — удаление и переименование папки.
    pub const DELETE_MAILBOX: Rights = Rights(1 << 7);
    /// `t` — флаг `\Deleted`.
    pub const DELETE_MESSAGES: Rights = Rights(1 << 8);
    /// `e` — EXPUNGE.
    pub const EXPUNGE: Rights = Rights(1 << 9);
    /// `a` — администрирование (SETACL).
    pub const ADMIN: Rights = Rights(1 << 10);
    /// Все права (`lrswipkxtea`), у владельца папки.
    pub const ALL: Rights = Rights((1 << 11) - 1);

    /// Все права из `other` есть в `self`.
    pub const fn contains(self, other: Rights) -> bool {
        self.0 & other.0 == other.0
    }

    /// Прав нет.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl BitOr for Rights {
    type Output = Rights;

    fn bitor(self, rhs: Rights) -> Rights {
        Rights(self.0 | rhs.0)
    }
}

impl BitOrAssign for Rights {
    fn bitor_assign(&mut self, rhs: Rights) {
        self.0 |= rhs.0;
    }
}

impl Sub for Rights {
    type Output = Rights;

    /// Права `self` без прав `rhs` (отрицательные права ACL).
    fn sub(self, rhs: Rights) -> Rights {
        Rights(self.0 & !rhs.0)
    }
}

impl fmt::Display for Rights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, c) in LETTERS.iter().enumerate() {
            if self.0 & (1 << i) != 0 {
                fmt::Write::write_char(f, char::from(*c))?;
            }
        }
        Ok(())
    }
}

impl fmt::Debug for Rights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Rights(\"{self}\")")
    }
}

impl FromStr for Rights {
    type Err = Error;

    /// Буквы в любом порядке, повторы допустимы; пустая строка — [`Rights::NONE`].
    fn from_str(s: &str) -> Result<Self> {
        s.bytes().try_fold(Rights::NONE, |acc, c| {
            LETTERS
                .iter()
                .position(|l| *l == c)
                .map(|i| acc | Rights(1 << i))
                .ok_or_else(|| Error::Invalid(format!("unknown right: {}", char::from(c))))
        })
    }
}

impl Serialize for Rights {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Rights {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Объект проверки прав.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Resource {
    /// Корень учётной записи (создание папок верхнего уровня: право `k`).
    Account(AccountId),
    /// Папка учётной записи.
    Mailbox {
        /// Владелец.
        account: AccountId,
        /// Папка.
        mailbox: MailboxId,
    },
}

/// Право отправки от чужого адреса.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SendRight {
    /// Send As: `From` — чужой адрес, `Sender` не добавляется (и для собственных адресов).
    SendAs,
    /// Send on Behalf: `From` — чужой адрес, `Sender` — адрес отправителя.
    SendOnBehalf,
}

/// Проверка прав субъекта (ACL RFC 4314 папок, делегирование отправки). Реализация — `tp-auth`
/// поверх ACL в метаданных; владелец учётной записи имеет [`Rights::ALL`].
#[async_trait]
pub trait Authorizer: Send + Sync + 'static {
    /// Действующие права субъекта на объект (IMAP MYRIGHTS, JMAP `myRights`).
    async fn rights(&self, principal: &Principal, resource: &Resource) -> Result<Rights>;

    /// Есть все права `required`, иначе `Error::Forbidden`.
    async fn check(
        &self,
        principal: &Principal,
        resource: &Resource,
        required: Rights,
    ) -> Result<()> {
        if self.rights(principal, resource).await?.contains(required) {
            Ok(())
        } else {
            Err(Error::Forbidden)
        }
    }

    /// Право отправки от адреса `from`; `None` — отправка от него запрещена.
    async fn send_right(&self, principal: &Principal, from: &str) -> Result<Option<SendRight>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::_private::serde_json;
    use std::sync::Arc;

    #[test]
    fn object_safe() {
        fn _dir(_: Arc<dyn Directory>) {}
        fn _authn(_: Arc<dyn Authenticator>) {}
        fn _authz(_: Arc<dyn Authorizer>) {}
        fn _sasl(_: Arc<dyn SaslServer>) {}
        fn _session(_: Box<dyn SaslSession>) {}
    }

    #[test]
    fn rights_parse_print() {
        let all: Rights = "lrswipkxtea".parse().expect("all");
        assert_eq!(all, Rights::ALL);
        assert_eq!(all.to_string(), "lrswipkxtea");
        assert_eq!("aelr".parse::<Rights>().expect("parse").to_string(), "lrea");
        assert_eq!("ll".parse::<Rights>(), Ok(Rights::LOOKUP));
        assert_eq!("".parse::<Rights>(), Ok(Rights::NONE));
        assert!("lrc".parse::<Rights>().is_err());
        assert!("lrd".parse::<Rights>().is_err());
        for (i, c) in LETTERS.iter().enumerate() {
            let r = Rights(1 << i);
            assert_eq!(r.to_string(), char::from(*c).to_string());
        }
    }

    #[test]
    fn rights_ops_serde() {
        let rs = Rights::READ | Rights::SEEN;
        assert!(rs.contains(Rights::READ));
        assert!(!rs.contains(Rights::READ | Rights::WRITE));
        assert!((rs - Rights::READ - Rights::SEEN).is_empty());
        assert_eq!(format!("{rs:?}"), "Rights(\"rs\")");
        let s = serde_json::to_string(&Rights::ALL).expect("serialize");
        assert_eq!(s, "\"lrswipkxtea\"");
        assert_eq!(
            serde_json::from_str::<Rights>(&s).expect("parse"),
            Rights::ALL
        );
    }

    #[test]
    fn secrets_hidden() {
        let p = Credentials::Password {
            login: "user".into(),
            secret: "hunter2".into(),
        };
        let b = Credentials::Bearer("tok3n".into());
        let s = format!("{p:?} {b:?}");
        assert!(s.contains("user") && !s.contains("hunter2") && !s.contains("tok3n"));
    }

    #[test]
    fn scopes() {
        let mut p = Principal {
            tenant: TenantId::DEFAULT,
            account: AccountId::new(),
            login: "u".into(),
            groups: BTreeSet::new(),
            scopes: None,
        };
        assert!(p.has_scope("mail"));
        p.scopes = Some(["mail".to_string()].into());
        assert!(p.has_scope("mail") && !p.has_scope("admin"));
    }
}
