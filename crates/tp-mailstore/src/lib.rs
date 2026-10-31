//! Почтовое хранилище: приём письма (MIME-части, дедупликация), выдача, флаги, корзина, журнал изменений (SRS 3.3, 4.7).
//!
//! [`Store`] реализует контракт [`MailStore`] (CONTRACTS.md#MailStore) поверх [`BlobStore`], [`BlobIndex`],
//! [`MessageStore`] и [`ChangeLog`]: письмо хранится как объект-скелет и объекты частей ([`split`]),
//! части дедуплицируются в пределах организации, объекты загружаются вне транзакций БД (SRS 3.4).
//! Изменяющие операции после фиксации публикуют `MailboxChanged` в [`EventBus`]; ошибка публикации
//! не отменяет операцию (шина at-most-once, клиенты сверяются по `ModSeq`). Корзина и истечение её срока — [`trash`].

use std::collections::BTreeSet;
use std::fmt;
use std::ops::Range;
use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tp_contracts::blob::{BlobIndex, BlobStore, ByteStream};
use tp_contracts::bus::{Event, EventBus, EventKind};
use tp_contracts::ext::DecisionHooks;
use tp_contracts::mailstore::{IngestRequest, Ingested, MailStore};
use tp_contracts::meta::{AccountScope, ChangeLog, FlagOp, FlagsUpdate, Inserted, MessageStore};
use tp_contracts::retention::{Quota, TrashStore};
use tp_contracts::{MailboxId, MessageId, ModSeq, Result};

mod fetch;
mod ingest;
pub mod meta;
mod ops;
pub mod quota;
pub mod split;
pub mod trash;

pub use trash::{TrashConfig, TrashPass};

/// Конфигурация хранилища (секция config.yaml; поля необязательны, неизвестное — ошибка).
///
/// ```yaml
/// trash:
///   interval: 1h
///   batch: 500
///   defer: 1d
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Истечение срока корзины.
    pub trash: TrashConfig,
}

/// Зависимости [`Store`]: реализации контрактов, выбранные приложением.
#[derive(Clone)]
pub struct Deps {
    /// Объекты BLOB.
    pub blobs: Arc<dyn BlobStore>,
    /// Учёт объектов BLOB.
    pub index: Arc<dyn BlobIndex>,
    /// Письма.
    pub messages: Arc<dyn MessageStore>,
    /// Журнал изменений: папки, затронутые операцией, — для уведомлений.
    pub changes: Arc<dyn ChangeLog>,
    /// Корзина: элементы с истёкшим сроком.
    pub trash: Arc<dyn TrashStore>,
    /// Квоты учётных записей.
    pub quota: Arc<dyn Quota>,
    /// Шина уведомлений.
    pub bus: Arc<dyn EventBus>,
    /// Точки принятия решений модулей.
    pub hooks: DecisionHooks,
}

impl fmt::Debug for Deps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deps")
            .field("hooks", &self.hooks)
            .finish_non_exhaustive()
    }
}

/// Почтовое хранилище (реализация [`MailStore`]).
#[derive(Debug, Clone)]
pub struct Store {
    d: Deps,
    cfg: Config,
}

impl Store {
    /// Хранилище поверх зависимостей с конфигурацией по умолчанию.
    pub fn new(deps: Deps) -> Self {
        Self::with_config(deps, Config::default())
    }

    /// Хранилище поверх зависимостей с конфигурацией.
    pub fn with_config(deps: Deps, cfg: Config) -> Self {
        Self { d: deps, cfg }
    }

    /// Публикует `MailboxChanged` для папок учётной записи (каждой — один раз).
    async fn notify(
        &self,
        scope: AccountScope,
        mailboxes: impl IntoIterator<Item = MailboxId>,
        modseq: ModSeq,
    ) {
        let unique: BTreeSet<MailboxId> = mailboxes.into_iter().collect();
        for mailbox in unique {
            let event = Event {
                tenant: scope.tenant,
                account: Some(scope.account),
                kind: EventKind::MailboxChanged { mailbox, modseq },
            };
            if let Err(e) = self.d.bus.publish(event).await {
                tracing::warn!(error = %e, "mailstore: уведомление не опубликовано");
            }
        }
    }
}

#[async_trait]
impl MailStore for Store {
    async fn ingest(&self, req: IngestRequest) -> Result<Ingested> {
        self.ingest_impl(req).await
    }

    async fn fetch_raw(
        &self,
        scope: AccountScope,
        message: MessageId,
        range: Option<Range<u64>>,
    ) -> Result<ByteStream> {
        self.fetch_raw_impl(scope, message, range).await
    }

    async fn fetch_part(
        &self,
        scope: AccountScope,
        message: MessageId,
        part_path: &str,
    ) -> Result<ByteStream> {
        self.fetch_part_impl(scope, message, part_path).await
    }

    async fn set_flags(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        op: FlagOp,
        unchanged_since: Option<ModSeq>,
    ) -> Result<FlagsUpdate> {
        self.set_flags_impl(scope, ids, op, unchanged_since).await
    }

    async fn copy(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        to: MailboxId,
    ) -> Result<Vec<Inserted>> {
        self.copy_impl(scope, ids, to).await
    }

    async fn move_to(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        to: MailboxId,
    ) -> Result<Vec<Inserted>> {
        self.move_impl(scope, ids, to).await
    }

    async fn delete_to_trash(&self, scope: AccountScope, ids: &[MessageId]) -> Result<ModSeq> {
        self.trash_impl(scope, ids).await
    }

    async fn restore(&self, scope: AccountScope, ids: &[MessageId]) -> Result<Vec<Inserted>> {
        self.restore_impl(scope, ids).await
    }
}

/// Окружение тестов: in-memory реализации testkit с общим учётом объектов.
#[cfg(test)]
pub(crate) mod testenv {
    use super::*;
    use tp_testkit::{Clock, MemBlobIndex, MemBlobStore, MemEventBus, MemMetadata, SystemClock};

    use crate::quota::fake::TestQuota;
    use crate::trash::fake::TestTrash;

    pub struct Env {
        pub blobs: MemBlobStore,
        pub index: MemBlobIndex,
        pub meta: MemMetadata,
        pub bus: MemEventBus,
        pub trash: TestTrash,
        pub quota: TestQuota,
        pub store: Store,
    }

    impl Env {
        pub fn new() -> Self {
            Self::with_hooks(DecisionHooks::default())
        }

        pub fn with_hooks(hooks: DecisionHooks) -> Self {
            let clock: Arc<dyn Clock> = Arc::new(SystemClock);
            let blobs = MemBlobStore::new(clock.clone()).with_chunk_size(4096);
            let index = MemBlobIndex::new(clock.clone());
            let meta = MemMetadata::new(clock).with_blob_index(index.clone());
            let bus = MemEventBus::default();
            let trash = TestTrash::new(meta.clone());
            let quota = TestQuota::new(meta.clone());
            let store = Store::new(Deps {
                blobs: Arc::new(blobs.clone()),
                index: Arc::new(index.clone()),
                messages: Arc::new(meta.clone()),
                changes: Arc::new(meta.clone()),
                trash: Arc::new(trash.clone()),
                quota: Arc::new(quota.clone()),
                bus: Arc::new(bus.clone()),
                hooks,
            });
            Self {
                blobs,
                index,
                meta,
                bus,
                trash,
                quota,
                store,
            }
        }
    }

    /// Поток байтов из буфера кусками по `chunk`.
    pub fn stream(raw: &[u8], chunk: usize) -> ByteStream {
        let chunks: Vec<Result<bytes::Bytes>> = raw
            .chunks(chunk.max(1))
            .map(|c| Ok(bytes::Bytes::copy_from_slice(c)))
            .collect();
        Box::pin(futures::stream::iter(chunks))
    }
}
