//! Локальный одноразовый токен Admin API (CONTRACTS.md#Auth, решение владельца #193, CTR-41).
//!
//! Первая учётная запись SMB заводится без провайдера OIDC: `thepost admin init` на узле (локальный сокет, ADM-11, APP-19)
//! выпускает токен с коротким сроком. Admin API принимает его в `Authorization: Bearer` наравне с токеном OIDC: значение
//! начинается с [`LOCAL_ADMIN_TOKEN_PREFIX`] (токен OIDC — JWT — так не начинается).
//!
//! Значение токена выдаётся один раз и не хранится: в хранилище — только хэш (hex SHA-256 значения, [`LocalAdminToken::hash`]).
//! Токен однократный: первая успешная проверка ([`LocalAdminTokens::consume`]) его погашает. Генерацию значения, хэш и
//! проверку ведёт `tp-admin`; хранилище — выпуск, погашение, отзыв, список и очистка.

use std::fmt;

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::ids::{AccountId, TenantId};

/// Префикс значения локального токена в `Authorization: Bearer`.
pub const LOCAL_ADMIN_TOKEN_PREFIX: &str = "tpa_";

/// Наибольшая длина идентификатора токена, байт.
pub const MAX_LOCAL_ADMIN_TOKEN_ID_LEN: usize = 64;

/// Значение Bearer — локальный токен Admin API (по префиксу), а не токен OIDC.
pub fn is_local_admin_token(bearer: &str) -> bool {
    bearer.starts_with(LOCAL_ADMIN_TOKEN_PREFIX)
}

/// Хэш — 64 символа hex в нижнем регистре (SHA-256).
fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Состояние токена в момент `now` ([`LocalAdminToken::state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LocalTokenState {
    /// Принимается.
    Active,
    /// Уже погашен.
    Used,
    /// Отозван.
    Revoked,
    /// Срок истёк.
    Expired,
}

/// Локальный токен Admin API (без значения). `Debug` не показывает хэш.
#[derive(Clone, PartialEq, Eq)]
pub struct LocalAdminToken {
    /// Идентификатор (для отзыва и аудита).
    pub id: String,
    /// Организация администратора.
    pub tenant: TenantId,
    /// Учётная запись администратора, от имени которой действует токен.
    pub account: AccountId,
    /// Хэш значения: hex SHA-256 в нижнем регистре.
    pub hash: String,
    /// Выпущен.
    pub created_at: OffsetDateTime,
    /// Действует до (не включительно); срок — настройка выпуска (`admin.init.token_ttl`).
    pub expires_at: OffsetDateTime,
    /// Погашен (первая успешная проверка).
    pub used_at: Option<OffsetDateTime>,
    /// Отозван.
    pub revoked_at: Option<OffsetDateTime>,
}

impl fmt::Debug for LocalAdminToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LocalAdminToken")
            .field("id", &self.id)
            .field("tenant", &self.tenant)
            .field("account", &self.account)
            .field("created_at", &self.created_at)
            .field("expires_at", &self.expires_at)
            .field("used_at", &self.used_at)
            .field("revoked_at", &self.revoked_at)
            .finish_non_exhaustive()
    }
}

impl LocalAdminToken {
    /// Проверяет поля: идентификатор непустой, не длиннее [`MAX_LOCAL_ADMIN_TOKEN_ID_LEN`], без пробелов и управляющих
    /// символов; хэш — 64 символа hex в нижнем регистре; `expires_at` позже `created_at`. Иначе — `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if self.id.is_empty()
            || self.id.len() > MAX_LOCAL_ADMIN_TOKEN_ID_LEN
            || self.id.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            return Err(Error::invalid(format!(
                "недопустимый идентификатор токена: {:?}",
                self.id
            )));
        }
        if !is_sha256_hex(&self.hash) {
            return Err(Error::invalid("хэш токена — не hex SHA-256"));
        }
        if self.expires_at <= self.created_at {
            return Err(Error::invalid("срок токена не позже выпуска"));
        }
        Ok(())
    }

    /// Состояние в момент `now`: отзыв важнее погашения, погашение — истечения.
    pub fn state(&self, now: OffsetDateTime) -> LocalTokenState {
        if self.revoked_at.is_some() {
            LocalTokenState::Revoked
        } else if self.used_at.is_some() {
            LocalTokenState::Used
        } else if now >= self.expires_at {
            LocalTokenState::Expired
        } else {
            LocalTokenState::Active
        }
    }
}

/// Хранилище локальных токенов Admin API (основная БД). Реализация — `tp-meta`; in-memory — `tp-testkit`.
#[async_trait]
pub trait LocalAdminTokens: Send + Sync + 'static {
    /// Сохраняет выпущенный токен. Недопустимые поля ([`LocalAdminToken::validate`]), `used_at` или `revoked_at`
    /// заданы — `Error::Invalid`; идентификатор или хэш заняты — `Error::Conflict`.
    async fn issue(&self, token: LocalAdminToken) -> Result<()>;

    /// Погашает токен с хэшем `hash` в момент `now` и возвращает его (с `used_at = Some(now)`). Атомарно: из
    /// параллельных вызовов успешен ровно один. Нет токена, погашен, отозван, срок истёк (`now ≥ expires_at`) —
    /// `Error::Forbidden` (причину не раскрывать вызывающему клиенту).
    async fn consume(&self, hash: &str, now: OffsetDateTime) -> Result<LocalAdminToken>;

    /// Отзывает токен в момент `at`; нет — `Error::NotFound`. Повторный отзыв — не ошибка, момент первого сохраняется;
    /// погашенный токен тоже отзывается (для аудита).
    async fn revoke(&self, id: &str, at: OffsetDateTime) -> Result<()>;

    /// Токены организации, включая погашенные, отозванные и истёкшие, по `created_at`, затем по `id`.
    async fn list(&self, tenant: TenantId) -> Result<Vec<LocalAdminToken>>;

    /// Удаляет токены с `expires_at < before` (в любом состоянии); возвращает число удалённых.
    async fn prune(&self, before: OffsetDateTime) -> Result<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn token() -> LocalAdminToken {
        LocalAdminToken {
            id: "init-1".into(),
            tenant: TenantId::DEFAULT,
            account: AccountId::new(),
            hash: "a".repeat(64),
            created_at: OffsetDateTime::UNIX_EPOCH,
            expires_at: OffsetDateTime::UNIX_EPOCH + Duration::from_secs(600),
            used_at: None,
            revoked_at: None,
        }
    }

    #[test]
    fn object_safe() {
        fn _store(_: Arc<dyn LocalAdminTokens>) {}
    }

    #[test]
    fn prefix() {
        assert!(is_local_admin_token("tpa_0123"));
        assert!(!is_local_admin_token("eyJhbGciOiJSUzI1NiJ9.e30.sig"));
    }

    #[test]
    fn debug_hides_hash() {
        let t = token();
        let shown = format!("{t:?}");
        assert!(!shown.contains(&t.hash), "{shown}");
        assert!(shown.contains("init-1"), "{shown}");
    }

    #[test]
    fn validate() {
        assert_eq!(token().validate(), Ok(()));
        for id in [
            "",
            "a b",
            "a\u{7}",
            &"x".repeat(MAX_LOCAL_ADMIN_TOKEN_ID_LEN + 1),
        ] {
            let t = LocalAdminToken {
                id: id.to_owned(),
                ..token()
            };
            assert!(t.validate().is_err(), "{id:?}");
        }
        for hash in [
            String::new(),
            "A".repeat(64),
            "a".repeat(63),
            "g".repeat(64),
        ] {
            let t = LocalAdminToken { hash, ..token() };
            assert!(t.validate().is_err());
        }
        let t = LocalAdminToken {
            expires_at: OffsetDateTime::UNIX_EPOCH,
            ..token()
        };
        assert!(t.validate().is_err());
    }

    #[test]
    fn state() {
        let t = token();
        let before = t.expires_at - Duration::from_secs(1);
        assert_eq!(t.state(before), LocalTokenState::Active);
        assert_eq!(t.state(t.expires_at), LocalTokenState::Expired);
        let used = LocalAdminToken {
            used_at: Some(before),
            ..token()
        };
        assert_eq!(used.state(before), LocalTokenState::Used);
        let revoked = LocalAdminToken {
            revoked_at: Some(before),
            ..used
        };
        assert_eq!(revoked.state(before), LocalTokenState::Revoked);
    }
}
