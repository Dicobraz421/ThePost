//! Группы рассылки при приёме (SRS 4.1, 4.5; CONTRACTS.md#Groups): ограничение круга отправителей, очередь
//! модерации, раскрытие вложенных групп.
//!
//! Адрес получателя, совпавший с адресом группы [`GroupStore`], проверяется до ответа `250` на `RCPT TO`:
//! отправитель вне круга группы — `550 5.7.1`. Внешний отправитель (smtp-in) — `None`, отправитель submission —
//! его учётная запись. Письмо в модерируемую группу не от модератора не раскрывается: после `DATA` оно
//! сохраняется одним объектом BLOB (одна ссылка, протокол записи SRS 3.4) и ставится в очередь модерации
//! ([`GroupStore::hold`]); ссылку снимает тот, кто решает судьбу письма. Иначе группа раскрывается
//! ([`GroupStore::expand`], защита от циклов — в хранилище) в учётные записи организации.
//!
//! Группы каталога без записи в [`GroupStore`] раскрываются по снимку каталога, как до SMTP-16.
//!
//! Решение модератора (`tp_contracts::groups::Moderation`, FIX-01) — `impl Moderation for Delivery`
//! ([`crate::deliver`]): объект письма читается отсюда, ссылку на него снимает успешное решение.

use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use time::OffsetDateTime;
use tp_contracts::blob::{BlobIndex, BlobStore};
use tp_contracts::groups::{DistributionGroup, GroupStore, HeldMessage};
use tp_contracts::{AccountId, BlobKey, GroupId, HeldId, Result, TenantId};

use crate::queue::{release_blob, store_blob};

/// Группы рассылки и хранилище писем очереди модерации.
#[derive(Clone)]
pub struct Groups {
    store: Arc<dyn GroupStore>,
    blobs: Arc<dyn BlobStore>,
    index: Arc<dyn BlobIndex>,
}

impl fmt::Debug for Groups {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Groups").finish_non_exhaustive()
    }
}

/// Письмо для очереди модерации.
pub(crate) struct Hold<'a> {
    pub tenant: TenantId,
    pub group: &'a DistributionGroup,
    pub sender: &'a str,
    pub sender_account: Option<AccountId>,
    pub message_id: Option<String>,
    pub raw: Bytes,
    pub at: OffsetDateTime,
}

impl Groups {
    /// Группы `store`; письма очереди модерации — объектами `blobs` с учётом ссылок `index`.
    pub fn new(
        store: Arc<dyn GroupStore>,
        blobs: Arc<dyn BlobStore>,
        index: Arc<dyn BlobIndex>,
    ) -> Self {
        Self {
            store,
            blobs,
            index,
        }
    }

    /// Группа с адресом рассылки `address`.
    pub(crate) async fn by_address(
        &self,
        tenant: TenantId,
        address: &str,
    ) -> Result<Option<DistributionGroup>> {
        self.store.by_address(tenant, address).await
    }

    /// Группа по идентификатору.
    pub(crate) async fn get(
        &self,
        tenant: TenantId,
        id: GroupId,
    ) -> Result<Option<DistributionGroup>> {
        self.store.get(tenant, id).await
    }

    /// Письмо очереди модерации целиком (как принято).
    pub(crate) async fn read(&self, key: &BlobKey) -> Result<Bytes> {
        crate::deliver::read_blob(self.blobs.as_ref(), key).await
    }

    /// Снимает ссылку письма очереди модерации на объект (после решения модератора).
    pub(crate) async fn release(&self, key: &BlobKey) {
        release_blob(self.index.as_ref(), key).await;
    }

    /// Учётные записи группы с раскрытием вложенных.
    pub(crate) async fn members(
        &self,
        tenant: TenantId,
        group: &DistributionGroup,
    ) -> Result<Vec<AccountId>> {
        self.store.expand(tenant, group.id).await
    }

    /// Может ли отправитель писать в группу.
    pub(crate) async fn allowed(
        &self,
        tenant: TenantId,
        group: &DistributionGroup,
        sender: Option<AccountId>,
    ) -> Result<bool> {
        self.store.sender_allowed(tenant, group, sender).await
    }

    /// Ставит письмо в очередь модерации группы; ошибка постановки снимает ссылку на объект.
    pub(crate) async fn hold(&self, h: Hold<'_>) -> Result<HeldId> {
        let size = h.raw.len() as u64;
        let blob = store_blob(self.blobs.as_ref(), self.index.as_ref(), h.tenant, h.raw).await?;
        let id = HeldId::new();
        let message = HeldMessage {
            id,
            group: Some(h.group.id),
            source: Default::default(),
            moderators: Vec::new(),
            recipients: Vec::new(),
            sender: h.sender.to_owned(),
            sender_account: h.sender_account,
            message_id: h.message_id,
            blob,
            size,
            held_at: h.at,
        };
        if let Err(e) = self.store.hold(h.tenant, message).await {
            release_blob(self.index.as_ref(), &blob).await;
            return Err(e);
        }
        Ok(id)
    }
}
