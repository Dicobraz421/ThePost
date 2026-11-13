//! Архив ящика (CONTRACTS.md#Archive; SRS 4.7; решение 2026-11-04-exchange-parity «Хранение»; CTR-76).
//!
//! Архив — ветка папок учётной записи: папка с ролью [`SpecialUse::Archive`](crate::meta::SpecialUse) (RFC 6154) и
//! все вложенные в неё. Письма ветки учитываются в квоте архива ([`Policy::archive_quota_bytes`],
//! [`ArchiveStore::archive_usage`]), остальные — в основной квоте ([`Policy::quota_bytes`],
//! [`Quota::usage`](crate::retention::Quota), `AccountStore::quota`): перенос в архив освобождает основную квоту.
//!
//! Автоперенос по возрасту (роль maintenance, `tp-mailstore`, MS-16): [`ArchiveStore::archive_due`] — письма, которым
//! пора в архив, по сроку; ближайший срок — первый элемент `archive_due` с моментом в далёком будущем и `limit = 1`,
//! один таймер без периодического опроса (SRS 2.8). Срок письма — `received_at + Policy::archive_after` по
//! действующей политике учётной записи на момент запроса: изменение политики действует и на полученные раньше.
//! Перенос — `MessageStore::move_to` в папку архива; квоту архива проверяет вызывающий.
//!
//! [`Policy::archive_quota_bytes`]: crate::meta::Policy::archive_quota_bytes
//! [`Policy::quota_bytes`]: crate::meta::Policy::quota_bytes

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::Result;
use crate::ids::{MailboxId, MessageId};
use crate::meta::{AccountScope, QuotaUsage};

/// Письмо, которому пора в архив ([`ArchiveStore::archive_due`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArchiveItem {
    /// Учётная запись.
    pub scope: AccountScope,
    /// Письмо.
    pub message: MessageId,
    /// Папка письма на момент запроса.
    pub mailbox: MailboxId,
    /// Срок переноса: `received_at + Policy::archive_after`.
    pub due: OffsetDateTime,
}

/// Архив ящика. Реализация — `tp-meta`; in-memory — `tp-testkit`.
#[async_trait]
pub trait ArchiveStore: Send + Sync + 'static {
    /// Письма всех организаций, которым пора в архив (системный вызов роли maintenance): вне ветки архива и
    /// корзины, без скрытых, у действующих ящиков ([`AccountState::Active`](crate::meta::AccountState)) с заданным
    /// [`Policy::archive_after`](crate::meta::Policy::archive_after), со сроком не позже `now`; по (`due`,
    /// `message`), не более `limit`.
    async fn archive_due(&self, now: OffsetDateTime, limit: usize) -> Result<Vec<ArchiveItem>>;

    /// Использование квоты архива учётной записи: письма ветки архива без скрытых; лимит —
    /// [`Policy::archive_quota_bytes`](crate::meta::Policy::archive_quota_bytes) по цепочке (не задан — без
    /// ограничения). Ветки архива нет — нули. Нет учётной записи — `Error::NotFound`.
    async fn archive_usage(&self, scope: AccountScope) -> Result<QuotaUsage>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn object_safe() {
        fn _archive(_: Arc<dyn ArchiveStore>) {}
    }
}
