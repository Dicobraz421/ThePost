//! Корзина и квоты (CONTRACTS.md#Trash, Retention, Quota (Ф1); SRS 4.7, 3.8).
//!
//! Удалённое пользователем попадает в корзину (`MessageStore::to_trash`, `MailboxStore::delete`) и хранится
//! срок из политики ([`Policy::trash_retention`](crate::meta::Policy), [`PolicyResolver`](crate::meta::PolicyResolver);
//! не задан — значение по умолчанию конфигурации хранилища, 30 суток). Срок вычисляет хранилище при переносе
//! в корзину и фиксирует в элементе (`expires_at`): изменение политики действует на элементы, удалённые после него.
//!
//! Истечение (роль maintenance, `tp-mailstore`): [`TrashStore::expired`] → точка принятия решений
//! `DecisionHooks::before_final_delete` → `MessageStore::expunge_final`; отказ хука — [`TrashStore::defer`].
//! `expunge_final` удаляет только письма, которые всё ещё в корзине: восстановленное после `expired` остаётся.
//! Политики хранения и Legal Hold — модуль «Соответствие требованиям» через этот хук (SRS 1.3).

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::ids::{MailboxId, MessageId};
use crate::meta::{AccountScope, Inserted, QuotaUsage};

/// Объект в корзине. Ф1 — письма; события и контакты добавятся с календарями.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub enum TrashObject {
    /// Письмо (корзина сохраняет `MessageId`).
    Message(MessageId),
}

/// Элемент корзины.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrashItem {
    /// Учётная запись.
    pub scope: AccountScope,
    /// Объект.
    pub object: TrashObject,
    /// Исходная папка (могла быть удалена — тогда восстановление во «Входящие»); `FinalDelete::mailbox`.
    pub origin: MailboxId,
    /// Размер, байт.
    pub size: u64,
    /// Момент приёма письма (политики хранения).
    pub received_at: OffsetDateTime,
    /// Момент удаления в корзину.
    pub deleted_at: OffsetDateTime,
    /// Окончательное удаление не раньше этого момента.
    pub expires_at: OffsetDateTime,
}

/// Результат восстановления из корзины.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Restored {
    /// Письмо: прежний `MessageId`, папка и новый UID.
    Message {
        /// Папка, куда восстановлено.
        mailbox: MailboxId,
        /// Идентификатор, UID, `ModSeq`.
        inserted: Inserted,
    },
}

/// Корзина. Реализация — `tp-meta`; in-memory — `tp-testkit`.
#[async_trait]
pub trait TrashStore: Send + Sync + 'static {
    /// Элементы корзины учётной записи, новые первыми, со смещения `offset`, не более `limit`
    /// (Admin API; JMAP видит корзину как папку).
    async fn list(
        &self,
        scope: AccountScope,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<TrashItem>>;

    /// Восстанавливает элемент (для письма — как `MessageStore::restore`). Элемента нет — `Error::NotFound`.
    async fn restore(&self, scope: AccountScope, object: TrashObject) -> Result<Restored>;

    /// Элементы всех организаций с `expires_at <= now`, по возрастанию `expires_at`, не более `limit`
    /// (системный вызов роли maintenance).
    async fn expired(&self, now: OffsetDateTime, limit: usize) -> Result<Vec<TrashItem>>;

    /// Откладывает окончательное удаление до `until` (хук запретил удаление: Legal Hold, политика хранения).
    /// Элемента нет — `Error::NotFound`.
    async fn defer(
        &self,
        scope: AccountScope,
        object: TrashObject,
        until: OffsetDateTime,
    ) -> Result<()>;
}

/// Квота учётной записи: использование (письма, включая корзину) и лимит из политики
/// ([`Policy::quota_bytes`](crate::meta::Policy); не задан — значение по умолчанию конфигурации, `None` — без лимита).
/// Реализация — `tp-meta`; in-memory — `tp-testkit`. Проверяют `ingest`, `copy`, `APPEND`, JMAP `Email/import`.
#[async_trait]
pub trait Quota: Send + Sync + 'static {
    /// Использование и лимит.
    async fn usage(&self, scope: AccountScope) -> Result<QuotaUsage>;

    /// Можно добавить `bytes`, иначе `Error::QuotaExceeded`.
    async fn check(&self, scope: AccountScope, bytes: u64) -> Result<()> {
        if self.usage(scope).await?.allows(bytes) {
            Ok(())
        } else {
            Err(Error::QuotaExceeded)
        }
    }
}

impl QuotaUsage {
    /// После добавления `bytes` занято не больше лимита.
    pub fn allows(&self, bytes: u64) -> bool {
        self.limit_bytes
            .is_none_or(|limit| self.used_bytes.saturating_add(bytes) <= limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{AccountId, TenantId};
    use std::sync::Arc;

    #[test]
    fn object_safe() {
        fn _trash(_: Arc<dyn TrashStore>) {}
        fn _quota(_: Arc<dyn Quota>) {}
    }

    #[test]
    fn quota_allows() {
        let mut u = QuotaUsage {
            used_bytes: 90,
            messages: 3,
            limit_bytes: None,
        };
        assert!(u.allows(u64::MAX));
        u.limit_bytes = Some(100);
        assert!(u.allows(10));
        assert!(!u.allows(11));
        assert!(!u.allows(u64::MAX));
    }

    #[test]
    fn trash_item_serde() {
        let item = TrashItem {
            scope: AccountScope {
                tenant: TenantId::DEFAULT,
                account: AccountId::new(),
            },
            object: TrashObject::Message(MessageId::new()),
            origin: MailboxId::new(),
            size: 10,
            received_at: OffsetDateTime::UNIX_EPOCH,
            deleted_at: OffsetDateTime::UNIX_EPOCH,
            expires_at: OffsetDateTime::UNIX_EPOCH + time::Duration::days(30),
        };
        let s = serde_json::to_string(&item).expect("serialize");
        assert_eq!(serde_json::from_str::<TrashItem>(&s).expect("parse"), item);
    }
}
