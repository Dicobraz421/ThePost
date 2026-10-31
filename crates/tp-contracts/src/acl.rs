//! ACL папок (RFC 4314) и делегирование отправки (CONTRACTS.md#Auth; SRS 4.5).
//!
//! [`AclStore`] — хранилище для `Authorizer` (`tp-auth`), IMAP ACL, JMAP и Admin API: записи ACL папок
//! учётной записи-владельца и права отправки от её адресов. Идентификаторы — как у
//! [`Principal`](crate::auth::Principal): имя входа, имя группы, [`ANYONE`]; с `-` в начале — отрицательные
//! права (RFC 4314 §2). Владелец имеет все права на свои папки без записей ACL — это правило `Authorizer`,
//! хранилище его не хранит.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::auth::{Rights, SendRight};
use crate::error::{Error, Result};
use crate::ids::{AccountId, MailboxId, TenantId};
use crate::meta::AccountScope;

/// Идентификатор «все пользователи организации» (RFC 4314 §2).
pub const ANYONE: &str = "anyone";

/// Наибольшая длина идентификатора, байт.
pub const MAX_IDENTIFIER_LEN: usize = 255;

/// Проверяет идентификатор: непустой, не длиннее [`MAX_IDENTIFIER_LEN`], без пробелов и управляющих символов,
/// не `-`; `-` в начале — только при `negative` (запись отрицательных прав). Иначе — `Error::Invalid`.
pub fn validate_identifier(id: &str, negative: bool) -> Result<()> {
    let bad = id.is_empty()
        || id.len() > MAX_IDENTIFIER_LEN
        || id == "-"
        || id.chars().any(|c| c.is_whitespace() || c.is_control())
        || (!negative && id.starts_with('-'));
    if bad {
        return Err(Error::invalid(format!(
            "недопустимый идентификатор: {id:?}"
        )));
    }
    Ok(())
}

/// Действующие права по записям субъекта: объединение положительных записей минус объединение отрицательных.
pub fn effective_rights<S: AsRef<str>>(entries: impl IntoIterator<Item = (S, Rights)>) -> Rights {
    let (mut plus, mut minus) = (Rights::NONE, Rights::NONE);
    for (id, rights) in entries {
        if id.as_ref().starts_with('-') {
            minus |= rights;
        } else {
            plus |= rights;
        }
    }
    plus - minus
}

/// Запись ACL папки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AclEntry {
    /// Идентификатор (с `-` в начале — отрицательные права).
    pub identifier: String,
    /// Права (непустые).
    pub rights: Rights,
}

/// Папка учётной записи, доступная субъекту по ACL (пространство имён общих папок IMAP).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedMailbox {
    /// Владелец.
    pub account: AccountId,
    /// Папка.
    pub mailbox: MailboxId,
    /// Действующие права субъекта по ACL (без прав владельца).
    pub rights: Rights,
}

/// Право отправки, выданное представителю.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendDelegation {
    /// Представитель: имя входа или группы.
    pub delegate: String,
    /// Вид права.
    pub right: SendRight,
}

/// ACL папок и делегирование отправки. Реализация — `tp-meta`; in-memory — `tp-testkit`.
///
/// `identifiers` субъекта — его имя входа и группы; `-` в начале — `Error::Invalid`.
#[async_trait]
pub trait AclStore: Send + Sync + 'static {
    /// ACL папки владельца (IMAP GETACL), по идентификатору (побайтно). Папки нет — `Error::NotFound`.
    async fn acl(&self, owner: AccountScope, mailbox: MailboxId) -> Result<Vec<AclEntry>>;

    /// Задаёт права идентификатора на папку целиком (IMAP SETACL); пустые права удаляют запись (DELETEACL),
    /// отсутствие записи — не ошибка. Недопустимый идентификатор — `Error::Invalid` (проверяется первым),
    /// папки нет — `Error::NotFound`. Удаление папки удаляет её ACL.
    async fn set_acl(
        &self,
        owner: AccountScope,
        mailbox: MailboxId,
        identifier: &str,
        rights: Rights,
    ) -> Result<()>;

    /// Права субъекта на папку по ACL (IMAP MYRIGHTS): записи `identifiers` и [`ANYONE`] минус их
    /// отрицательные записи ([`effective_rights`]). Права владельца не учитываются; папки нет — `Rights::NONE`.
    async fn acl_rights(
        &self,
        owner: AccountScope,
        mailbox: MailboxId,
        identifiers: &[String],
    ) -> Result<Rights>;

    /// Папки организации, на которые у субъекта есть право `l` по ACL, с действующими правами,
    /// по возрастанию (`account`, `mailbox`). Папки самого субъекта отбрасывает вызывающий.
    async fn shared_mailboxes(
        &self,
        tenant: TenantId,
        identifiers: &[String],
    ) -> Result<Vec<SharedMailbox>>;

    /// Делегирования отправки от адресов владельца, по представителю (побайтно).
    async fn send_delegations(&self, owner: AccountScope) -> Result<Vec<SendDelegation>>;

    /// Выдаёт представителю право отправки от адресов владельца (Send As / Send on Behalf), заменяя прежнее;
    /// `None` — отзывает (отсутствие — не ошибка). Недопустимый идентификатор — `Error::Invalid`,
    /// учётной записи нет — `Error::NotFound`.
    async fn set_send_delegation(
        &self,
        owner: AccountScope,
        delegate: &str,
        right: Option<SendRight>,
    ) -> Result<()>;

    /// Право субъекта отправлять от адресов владельца: сильнейшее из выданных `identifiers`
    /// (Send As сильнее Send on Behalf; [`ANYONE`] не добавляется); `None` — права нет.
    async fn send_right_of(
        &self,
        owner: AccountScope,
        identifiers: &[String],
    ) -> Result<Option<SendRight>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn r(s: &str) -> Rights {
        s.parse().expect("rights")
    }

    #[test]
    fn object_safe() {
        fn _acl(_: Arc<dyn AclStore>) {}
    }

    #[test]
    fn identifiers() {
        for ok in ["fred", "group:sales", ANYONE, "a@b.test"] {
            assert!(validate_identifier(ok, false).is_ok(), "{ok}");
        }
        assert!(validate_identifier("-fred", true).is_ok());
        for bad in ["", "-", "a b", "a\tb", "a\u{7}"] {
            assert!(validate_identifier(bad, true).is_err(), "{bad:?}");
        }
        assert!(validate_identifier("-fred", false).is_err());
        assert!(validate_identifier(&"x".repeat(MAX_IDENTIFIER_LEN), false).is_ok());
        assert!(validate_identifier(&"x".repeat(MAX_IDENTIFIER_LEN + 1), true).is_err());
    }

    #[test]
    fn negative_rights_subtract() {
        let rows = [("fred", r("lrswi")), (ANYONE, r("lr")), ("-fred", r("w"))];
        assert_eq!(effective_rights(rows).to_string(), "lrsi");
        assert_eq!(effective_rights::<&str>([]), Rights::NONE);
    }

    #[test]
    fn serde() {
        let e = SharedMailbox {
            account: AccountId::new(),
            mailbox: MailboxId::new(),
            rights: r("lr"),
        };
        let s = serde_json::to_string(&e).expect("serialize");
        assert_eq!(serde_json::from_str::<SharedMailbox>(&s).expect("parse"), e);
    }
}
