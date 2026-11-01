//! Пароли приложений (CONTRACTS.md#Auth; SRS 5.1): хранилище для `tp-auth` (`AppPasswords`, `PasswordAuthenticator`).
//!
//! Значение пароля выдаётся один раз при выпуске и не хранится: в хранилище — только хэш (argon2id, PHC-строка).
//! Выпуск, проверку и предел числа паролей ведёт `tp-auth`; хранилище — список, добавление и отзыв.

use std::fmt;

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::meta::AccountScope;

/// Наибольшая длина идентификатора пароля, байт.
pub const MAX_APP_PASSWORD_ID_LEN: usize = 64;

/// Наибольшая длина подписи пароля, символов.
pub const MAX_APP_PASSWORD_LABEL_LEN: usize = 255;

/// Пароль приложения (без значения). `Debug` не показывает хэш.
#[derive(Clone, PartialEq, Eq)]
pub struct AppPassword {
    /// Идентификатор в учётной записи (для отзыва).
    pub id: String,
    /// Подпись владельца («Телефон», «Thunderbird дома»).
    pub label: String,
    /// Хэш, PHC-строка.
    pub hash: String,
    /// Выпущен.
    pub created_at: OffsetDateTime,
    /// Отозван; отозванный пароль не принимается.
    pub revoked_at: Option<OffsetDateTime>,
}

impl fmt::Debug for AppPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppPassword")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("created_at", &self.created_at)
            .field("revoked_at", &self.revoked_at)
            .finish_non_exhaustive()
    }
}

impl AppPassword {
    /// Проверяет поля: идентификатор непустой, не длиннее [`MAX_APP_PASSWORD_ID_LEN`], без пробелов и управляющих
    /// символов; подпись не длиннее [`MAX_APP_PASSWORD_LABEL_LEN`] символов, без управляющих; хэш непустой.
    /// Иначе — `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || self.id.len() > MAX_APP_PASSWORD_ID_LEN
            || self.id.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(Error::invalid(format!(
                "недопустимый идентификатор пароля: {:?}",
                self.id
            )));
        }
        if self.label.chars().count() > MAX_APP_PASSWORD_LABEL_LEN
            || self.label.chars().any(char::is_control)
        {
            return Err(Error::invalid("недопустимая подпись пароля"));
        }
        if self.hash.is_empty() {
            return Err(Error::invalid("пустой хэш пароля"));
        }
        Ok(())
    }
}

/// Хранилище паролей приложений учётной записи. Реализация — `tp-meta`; in-memory — `tp-testkit`.
#[async_trait]
pub trait AppPasswordStore: Send + Sync + 'static {
    /// Пароли учётной записи, включая отозванные, по `created_at`, затем по `id` (побайтно);
    /// нет учётной записи — пусто.
    async fn list(&self, scope: AccountScope) -> Result<Vec<AppPassword>>;

    /// Добавляет пароль. Недопустимые поля ([`AppPassword::validate`]) — `Error::Invalid`, учётной записи нет —
    /// `Error::NotFound`, идентификатор занят в учётной записи — `Error::Conflict`.
    async fn insert(&self, scope: AccountScope, password: AppPassword) -> Result<()>;

    /// Отмечает пароль отозванным в момент `at`; пароля нет — `Error::NotFound`. Повторный отзыв — не ошибка,
    /// момент первого отзыва сохраняется.
    async fn revoke(&self, scope: AccountScope, id: &str, at: OffsetDateTime) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn password() -> AppPassword {
        AppPassword {
            id: "0123456789abcdef".into(),
            label: "Телефон".into(),
            hash: "$argon2id$v=19$m=19456,t=2,p=1$c2FsdA$aGFzaA".into(),
            created_at: OffsetDateTime::UNIX_EPOCH,
            revoked_at: None,
        }
    }

    #[test]
    fn object_safe() {
        fn _store(_: Arc<dyn AppPasswordStore>) {}
    }

    #[test]
    fn debug_hides_hash() {
        let p = password();
        let shown = format!("{p:?}");
        assert!(!shown.contains(&p.hash), "{shown}");
        assert!(shown.contains("Телефон"), "{shown}");
    }

    #[test]
    fn validate() {
        assert_eq!(password().validate(), Ok(()));
        let label = |l: String| AppPassword {
            label: l,
            ..password()
        };
        assert_eq!(label(String::new()).validate(), Ok(()));
        assert_eq!(
            label("я".repeat(MAX_APP_PASSWORD_LABEL_LEN)).validate(),
            Ok(())
        );
        assert!(
            label("я".repeat(MAX_APP_PASSWORD_LABEL_LEN + 1))
                .validate()
                .is_err()
        );
        assert!(label("a\nb".into()).validate().is_err());
        for id in [
            "",
            "a b",
            "a\u{7}",
            &"x".repeat(MAX_APP_PASSWORD_ID_LEN + 1),
        ] {
            let p = AppPassword {
                id: id.to_owned(),
                ..password()
            };
            assert!(p.validate().is_err(), "{id:?}");
        }
        let p = AppPassword {
            hash: String::new(),
            ..password()
        };
        assert!(p.validate().is_err());
    }
}
