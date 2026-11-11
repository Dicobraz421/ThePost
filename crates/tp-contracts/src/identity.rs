//! Адреса отправителя учётной записи (JMAP Identity, RFC 8621 §6; CTR-30; CONTRACTS.md#Identity и EmailSubmission).
//!
//! Хранилище держит адреса, которые пользователь добавил сам: отображаемое имя, `Reply-To`, `Bcc`, подписи.
//! Адрес входа учётной записи (из `AccountStore`) хранилищу не нужен: его показывает `tp-jmap` как неудаляемый.
//! Право отправлять от адреса (свои адреса, Send As, Send on Behalf) проверяет вызывающий
//! ([`Authorizer::send_right`](crate::auth::Authorizer::send_right)) до [`IdentityStore::put`] и при каждой отправке.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::meta::AccountScope;

/// Наибольшая длина идентификатора адреса отправителя, байт.
pub const MAX_IDENTITY_ID_LEN: usize = 64;

/// Наибольшая длина отображаемого имени, символов.
pub const MAX_IDENTITY_NAME_LEN: usize = 255;

/// Наибольшая длина подписи (текстовой и HTML отдельно), байт.
pub const MAX_SIGNATURE_LEN: usize = 64 * 1024;

/// Наибольшее число адресов в `reply_to` и в `bcc`.
pub const MAX_IDENTITY_ADDRESSES: usize = 100;

/// Адрес с необязательным отображаемым именем (JMAP `EmailAddress`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdentityAddress {
    /// Отображаемое имя.
    pub name: Option<String>,
    /// Адрес `local@domain`.
    pub email: String,
}

/// Адрес отправителя учётной записи (JMAP Identity).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// Идентификатор в учётной записи.
    pub id: String,
    /// Отображаемое имя в `From`; пустое — без имени.
    pub name: String,
    /// Адрес `From`.
    pub email: String,
    /// Адреса `Reply-To` по умолчанию.
    pub reply_to: Vec<IdentityAddress>,
    /// Адреса `Bcc` по умолчанию.
    pub bcc: Vec<IdentityAddress>,
    /// Текстовая подпись.
    pub text_signature: String,
    /// Подпись HTML.
    pub html_signature: String,
}

/// Адрес `local@domain`: одна `@`, обе части непустые, без пробелов и управляющих символов, ≤ 320 байт.
pub fn valid_address(email: &str) -> bool {
    let Some((local, domain)) = email.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && email.len() <= 320
        && !email.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// Строка без управляющих символов и не длиннее `max` символов.
fn plain(s: &str, max: usize) -> bool {
    s.chars().count() <= max && !s.chars().any(char::is_control)
}

impl Identity {
    /// Проверяет поля: идентификатор 1–[`MAX_IDENTITY_ID_LEN`] байт без пробелов и управляющих символов; имена —
    /// до [`MAX_IDENTITY_NAME_LEN`] символов без управляющих; адреса — [`valid_address`], в `reply_to` и `bcc` —
    /// до [`MAX_IDENTITY_ADDRESSES`]; подписи — до [`MAX_SIGNATURE_LEN`] байт. Иначе — `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || self.id.len() > MAX_IDENTITY_ID_LEN
            || self.id.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(Error::invalid(format!(
                "недопустимый идентификатор адреса отправителя: {:?}",
                self.id
            )));
        }
        if !plain(&self.name, MAX_IDENTITY_NAME_LEN) || !valid_address(&self.email) {
            return Err(Error::invalid("недопустимый адрес отправителя"));
        }
        for list in [&self.reply_to, &self.bcc] {
            if list.len() > MAX_IDENTITY_ADDRESSES
                || list.iter().any(|a| {
                    !valid_address(&a.email)
                        || a.name
                            .as_deref()
                            .is_some_and(|n| !plain(n, MAX_IDENTITY_NAME_LEN))
                })
            {
                return Err(Error::invalid("недопустимые адреса Reply-To или Bcc"));
            }
        }
        if self.text_signature.len() > MAX_SIGNATURE_LEN
            || self.html_signature.len() > MAX_SIGNATURE_LEN
        {
            return Err(Error::invalid("подпись слишком длинная"));
        }
        Ok(())
    }
}

/// Хранилище адресов отправителя учётной записи. Реализация — `tp-meta` (карточка meta); in-memory — `tp-testkit`.
/// Удаление учётной записи удаляет её адреса.
#[async_trait]
pub trait IdentityStore: Send + Sync + 'static {
    /// Адреса учётной записи по `id` (побайтно); нет — пусто.
    async fn list(&self, scope: AccountScope) -> Result<Vec<Identity>>;

    /// Адрес по идентификатору.
    async fn get(&self, scope: AccountScope, id: &str) -> Result<Option<Identity>>;

    /// Создаёт или заменяет целиком адрес `identity.id`; недопустимые поля ([`Identity::validate`]) — `Error::Invalid`.
    /// Возвращает новое состояние ([`IdentityStore::state`]).
    async fn put(&self, scope: AccountScope, identity: Identity) -> Result<u64>;

    /// Удаляет адрес; нет — `Error::NotFound`. Возвращает новое состояние.
    async fn delete(&self, scope: AccountScope, id: &str) -> Result<u64>;

    /// Состояние адресов учётной записи (JMAP `state`): растёт при каждом `put` и `delete`, у новой — 0.
    async fn state(&self, scope: AccountScope) -> Result<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn identity() -> Identity {
        Identity {
            id: "work".into(),
            name: "Анна Иванова".into(),
            email: "anna@example.org".into(),
            reply_to: vec![IdentityAddress {
                name: Some("Отдел продаж".into()),
                email: "sales@example.org".into(),
            }],
            bcc: Vec::new(),
            text_signature: "--\nАнна".into(),
            html_signature: "<p>Анна</p>".into(),
        }
    }

    #[test]
    fn object_safe() {
        fn _store(_: Arc<dyn IdentityStore>) {}
    }

    #[test]
    fn addresses() {
        assert!(valid_address("a@example.org"));
        for bad in [
            "",
            "a",
            "@example.org",
            "a@",
            "a@b@c",
            "a b@example.org",
            "a\n@x",
        ] {
            assert!(!valid_address(bad), "{bad:?}");
        }
    }

    #[test]
    fn validate() {
        assert_eq!(identity().validate(), Ok(()));
        let bad = [
            Identity {
                id: String::new(),
                ..identity()
            },
            Identity {
                id: "a b".into(),
                ..identity()
            },
            Identity {
                email: "anna".into(),
                ..identity()
            },
            Identity {
                name: "a\u{7}".into(),
                ..identity()
            },
            Identity {
                bcc: vec![IdentityAddress {
                    name: None,
                    email: "x".into(),
                }],
                ..identity()
            },
            Identity {
                html_signature: "x".repeat(MAX_SIGNATURE_LEN + 1),
                ..identity()
            },
        ];
        for i in bad {
            assert!(i.validate().is_err(), "{i:?}");
        }
    }

    #[test]
    fn serde_roundtrip() {
        let i = identity();
        let s = serde_json::to_string(&i).expect("json");
        assert_eq!(serde_json::from_str::<Identity>(&s).expect("parse"), i);
    }
}
