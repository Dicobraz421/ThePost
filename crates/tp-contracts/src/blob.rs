//! Хранилище объектов BLOB и их учёт в метаданных (CONTRACTS.md#BlobStore, #BlobIndex; SRS 3.2–3.5).
//!
//! # Протокол записи (SRS 3.4)
//!
//! Обращаться к [`BlobStore`] внутри открытой транзакции БД запрещено: каждый вызов [`BlobIndex`] —
//! отдельная короткая транзакция, загрузка — между ними.
//!
//! 1. Хэш части вычисляется при потоковом разборе письма.
//! 2. [`BlobIndex::acquire`]: если есть запись `live` — счётчик ссылок увеличивается на `refs`
//!    (число получателей) одной транзакцией, ответ [`Acquire::Existing`], загрузка не нужна.
//! 3. Иначе [`Acquire::Absent`]: объект загружается по ключу `hash/gen` ([`BlobStore::put`] идемпотентна),
//!    затем [`BlobIndex::commit_new`] создаёт запись `live`. Если запись в этот момент `deleting`,
//!    поколение увеличивается — [`Commit::Bumped`]: загрузить по новому `gen` и повторить `commit_new`.
//! 4. Метаданные письма фиксируются короткой транзакцией после успешной загрузки
//!    и ссылаются на ключ с поколением из [`Commit::Done`] / [`Acquire::Existing`].
//!
//! # Протокол удаления
//!
//! 1. Удаление пользователем переносит письмо в корзину; ссылки сохраняются.
//! 2. Окончательное удаление — [`BlobIndex::release`]; при обнулении счётчика фиксируется `zero_since`.
//! 3. GC: [`BlobIndex::gc_candidates`] → [`BlobIndex::mark_deleting`] (одна транзакция: `refcnt = 0`
//!    и окно W истекло) → [`BlobStore::delete`] → [`BlobIndex::finalize_delete`] (заменяет запись
//!    надгробием, только если поколение не изменилось).
//!
//! Надгробие хранит поколение удалённого объекта: загрузка, для которой `acquire` выдал это поколение
//! до удаления, получит в `commit_new` [`Commit::Bumped`], а не ссылку на удалённый объект.
//! Надгробие удаляется по истечении срока, большего максимальной длительности загрузки.
//!
//! Поиск «сирот»: [`BlobStore::list`] объектов старше периода ожидания, для которых
//! [`BlobIndex::exists`] ложно, — удалить. Период ожидания больше максимальной длительности загрузки.
//!
//! # Пример
//!
//! ```
//! use bytes::Bytes;
//! use tp_contracts::blob::{Acquire, BlobIndex, BlobStore, Commit};
//! use tp_contracts::{BlobHash, BlobKey, Result, TenantId};
//!
//! /// Сохраняет часть письма для `refs` получателей и возвращает ключ для метаданных.
//! async fn store_part(
//!     index: &dyn BlobIndex,
//!     store: &dyn BlobStore,
//!     tenant: TenantId,
//!     hash: BlobHash,
//!     data: Bytes,
//!     refs: u32,
//! ) -> Result<BlobKey> {
//!     let mut generation = match index.acquire(tenant, &hash, refs).await? {
//!         Acquire::Existing { generation } => return Ok(BlobKey { tenant, hash, generation }),
//!         Acquire::Absent { generation } => generation,
//!     };
//!     loop {
//!         let key = BlobKey { tenant, hash, generation };
//!         store.put(&key, data.clone()).await?; // вне транзакции БД
//!         match index.commit_new(tenant, &hash, generation, data.len() as u64, refs).await? {
//!             Commit::Done { generation } => return Ok(BlobKey { tenant, hash, generation }),
//!             Commit::Bumped { generation: next } => generation = next,
//!         }
//!     }
//! }
//!
//! /// Один проход GC по кандидатам.
//! async fn gc(
//!     index: &dyn BlobIndex,
//!     store: &dyn BlobStore,
//!     older_than: time::OffsetDateTime,
//! ) -> Result<()> {
//!     for key in index.gc_candidates(older_than, 1000).await? {
//!         if index.mark_deleting(&key, older_than).await? {
//!             store.delete(&key).await?;
//!             index.finalize_delete(&key).await?;
//!         }
//!     }
//!     Ok(())
//! }
//! ```

use std::ops::Range;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::Result;
use crate::ids::{BlobHash, BlobKey, Gen, TenantId};

/// Поток байтов объекта.
pub type ByteStream = BoxStream<'static, Result<Bytes>>;

/// Поток записей листинга хранилища.
pub type ObjectStream = BoxStream<'static, Result<ObjectEntry>>;

/// Запись листинга [`BlobStore::list`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectEntry {
    /// Путь объекта: [`BlobKey::path`] или произвольный путь `put_raw`.
    pub path: String,
    /// Размер в байтах.
    pub size: u64,
    /// Момент последней записи.
    pub modified: OffsetDateTime,
}

impl ObjectEntry {
    /// Ключ объекта, если путь — ключ BLOB (а не сегмент индекса и т. п.).
    pub fn key(&self) -> Option<BlobKey> {
        BlobKey::parse_path(&self.path).ok()
    }
}

/// Хранилище неизменяемых объектов (локальный диск / S3, SRS 3.2). Запись идемпотентна.
///
/// Реализации — `tp-blob` (и модули через реестр); in-memory — `tp-testkit`.
/// Сетевые сбои — `Error::Unavailable`.
#[async_trait]
pub trait BlobStore: Send + Sync + 'static {
    /// Записывает объект целиком. Повторная запись того же ключа — не ошибка.
    async fn put(&self, key: &BlobKey, data: Bytes) -> Result<()>;

    /// Записывает объект из потока; объект становится видимым только после полной записи.
    async fn put_stream(
        &self,
        key: &BlobKey,
        data: ByteStream,
        size_hint: Option<u64>,
    ) -> Result<()>;

    /// Читает объект или диапазон байтов. Отсутствие — `Error::NotFound`.
    async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<ByteStream>;

    /// Размер объекта или `None`, если его нет.
    async fn head(&self, key: &BlobKey) -> Result<Option<u64>>;

    /// Удаляет объект; отсутствие — не ошибка.
    async fn delete(&self, key: &BlobKey) -> Result<()>;

    /// Объекты с путём, начинающимся с `prefix`, записанные раньше `older_than`
    /// (поиск «сирот», миграция бэкенда SRS 3.7). Порядок не гарантирован.
    fn list(&self, prefix: &str, older_than: OffsetDateTime) -> ObjectStream;

    /// Записывает объект по произвольному пути (сегменты индексов `idx/{tenant}/{account}/...`).
    async fn put_raw(&self, path: &str, data: Bytes) -> Result<()>;

    /// Читает объект по произвольному пути. Отсутствие — `Error::NotFound`.
    async fn get_raw(&self, path: &str, range: Option<Range<u64>>) -> Result<ByteStream>;

    /// Удаляет объект по произвольному пути; отсутствие — не ошибка.
    async fn delete_raw(&self, path: &str) -> Result<()>;
}

/// Ответ [`BlobIndex::acquire`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Acquire {
    /// Запись `live` есть, ссылки добавлены; загрузка не нужна.
    Existing {
        /// Поколение существующего объекта.
        generation: Gen,
    },
    /// Записи `live` нет: загрузить по этому поколению и вызвать [`BlobIndex::commit_new`].
    Absent {
        /// Поколение для загрузки.
        generation: Gen,
    },
}

/// Ответ [`BlobIndex::commit_new`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Commit {
    /// Запись `live` создана или ссылки добавлены к параллельно созданной.
    /// Метаданные письма ссылаются на это поколение (может отличаться от загруженного —
    /// тогда загруженный объект станет «сиротой»).
    Done {
        /// Поколение объекта, на которое ссылаться.
        generation: Gen,
    },
    /// Запись была `deleting`: загрузить по новому поколению и повторить `commit_new`.
    Bumped {
        /// Новое поколение.
        generation: Gen,
    },
}

/// Учёт объектов BLOB в метаданных: таблица `blobs` (хэш, поколение, счётчик ссылок,
/// состояние `live`/`deleting`, момент обнуления). Реализация — `tp-meta`; in-memory — `tp-testkit`.
///
/// Каждый метод — одна короткая транзакция; к [`BlobStore`] реализация не обращается.
/// Операции GC (`gc_candidates`, `mark_deleting`, `finalize_delete`) работают по всем организациям
/// (роль maintenance), остальные — в контексте `tenant`.
#[async_trait]
pub trait BlobIndex: Send + Sync + 'static {
    /// Шаг 2 записи: добавить `refs` ссылок к записи `live` или выдать поколение для загрузки.
    async fn acquire(&self, tenant: TenantId, hash: &BlobHash, refs: u32) -> Result<Acquire>;

    /// Шаг 3 записи: после загрузки объекта `hash/generation` размером `size` создать запись `live`
    /// с `refs` ссылками.
    async fn commit_new(
        &self,
        tenant: TenantId,
        hash: &BlobHash,
        generation: Gen,
        size: u64,
        refs: u32,
    ) -> Result<Commit>;

    /// Уменьшить счётчик на `refs`; при обнулении зафиксировать `zero_since`.
    /// Уход счётчика ниже нуля — `Error::Conflict`.
    async fn release(
        &self,
        tenant: TenantId,
        hash: &BlobHash,
        generation: Gen,
        refs: u32,
    ) -> Result<()>;

    /// Записи с `refcnt = 0` и `zero_since < older_than` (окно W истекло), не более `limit`.
    async fn gc_candidates(&self, older_than: OffsetDateTime, limit: usize)
    -> Result<Vec<BlobKey>>;

    /// Перевести запись в `deleting`, если `refcnt = 0`, `zero_since < older_than` и поколение совпадает.
    /// `false` — условие не выполнено, объект удалять нельзя.
    async fn mark_deleting(&self, key: &BlobKey, older_than: OffsetDateTime) -> Result<bool>;

    /// Заменить запись `deleting` надгробием, только если поколение не изменилось. `false` — запись уже другая.
    /// Надгробие невидимо для [`BlobIndex::exists`] и GC; `acquire`/`commit_new` выдают после него следующее поколение.
    async fn finalize_delete(&self, key: &BlobKey) -> Result<bool>;

    /// Есть ли запись с совпадающими хэшем и поколением (поиск «сирот»).
    async fn exists(&self, key: &BlobKey) -> Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn object_safe() {
        fn _store(_: Arc<dyn BlobStore>) {}
        fn _index(_: Arc<dyn BlobIndex>) {}
    }

    #[test]
    fn object_entry_key() {
        let key = BlobKey {
            tenant: TenantId::DEFAULT,
            hash: BlobHash([7; 32]),
            generation: Gen(1),
        };
        let entry = |path: String| ObjectEntry {
            path,
            size: 1,
            modified: OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(entry(key.path()).key(), Some(key));
        assert_eq!(entry("idx/t/a/seg".into()).key(), None);
    }
}
