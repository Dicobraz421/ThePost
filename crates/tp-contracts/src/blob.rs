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
//! # Сжатие (SRS 3.2)
//!
//! Писатель сжимает объект zstd перед [`BlobStore::put`], если это уменьшает размер; уже сжатые форматы
//! (архивы, изображения, видео) не сжимает. Хэш и `size` в учёте — от исходного содержимого, признак сжатия
//! ([`Compression`]) — в записи учёта: [`BlobIndex::commit_new_with`] при записи, [`BlobIndex::compression`]
//! при чтении. Сжимать можно, только если [`BlobIndex::supports_compression`]. Новое поколение после
//! [`Commit::Bumped`] записывается с тем же признаком.
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
//! Срок следующего прохода — [`BlobIndex::next_gc_due`] (`MIN(zero_since) + W`): один таймер, без
//! периодических проходов.
//!
//! # Загрузки (JMAP `uploadUrl`)
//!
//! Загруженный файл — объект без ссылок, ждущий GC по окну W или учёта в письме. Владелец загрузки — в учёте:
//! [`BlobIndex::mark_upload`] после загрузки, [`BlobIndex::upload_owned`] (и [`BlobIndex::exists`]) при обращении
//! по id, [`BlobIndex::prune_uploads`] — очистка истёкших отметок в проходе GC.
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

use std::fmt;
use std::ops::Range;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::BoxStream;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::ids::{BlobHash, BlobKey, Gen, TenantId};
use crate::meta::AccountScope;

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
    /// Сегмент неизменяем: путь записывается один раз, перезапись существующего пути запрещена
    /// (новая версия — новый путь); поэтому кэш BLOB кэширует `*_raw` без инвалидации.
    async fn put_raw(&self, path: &str, data: Bytes) -> Result<()>;

    /// Читает объект по произвольному пути. Отсутствие — `Error::NotFound`.
    async fn get_raw(&self, path: &str, range: Option<Range<u64>>) -> Result<ByteStream>;

    /// Удаляет объект по произвольному пути; отсутствие — не ошибка.
    async fn delete_raw(&self, path: &str) -> Result<()>;
}

/// Сжатие объекта BLOB (SRS 3.2) — признак в записи учёта. Хэш и размер в учёте — от исходного содержимого.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    /// Объект хранится как есть.
    #[default]
    None,
    /// Объект — кадр zstd исходного содержимого.
    Zstd,
}

impl Compression {
    /// Все значения в порядке объявления.
    pub const ALL: &'static [Compression] = &[Compression::None, Compression::Zstd];

    /// Строка хранения (`none`, `zstd`) — та же, что в JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Compression::None => "none",
            Compression::Zstd => "zstd",
        }
    }
}

impl fmt::Display for Compression {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Compression {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        Compression::ALL
            .iter()
            .copied()
            .find(|c| c.as_str() == s)
            .ok_or_else(|| Error::invalid(format!("unknown compression: {s}")))
    }
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
    /// с `refs` ссылками. Объект не сжат — то же, что [`BlobIndex::commit_new_with`] с [`Compression::None`].
    async fn commit_new(
        &self,
        tenant: TenantId,
        hash: &BlobHash,
        generation: Gen,
        size: u64,
        refs: u32,
    ) -> Result<Commit>;

    /// Учёт хранит признак сжатия — писатель может сжимать объекты. По умолчанию — нет.
    fn supports_compression(&self) -> bool {
        false
    }

    /// Шаг 3 записи для объекта, записанного со сжатием `compression`; `size` — размер исходного содержимого.
    /// Создаваемая запись получает признак; при [`Commit::Done`] с поколением параллельно созданной записи
    /// признак — её (читать [`BlobIndex::compression`] по ключу из ответа).
    ///
    /// По умолчанию: [`Compression::None`] — [`BlobIndex::commit_new`], иначе `Error::Internal`
    /// (учёт без [`BlobIndex::supports_compression`]).
    async fn commit_new_with(
        &self,
        tenant: TenantId,
        hash: &BlobHash,
        generation: Gen,
        size: u64,
        refs: u32,
        compression: Compression,
    ) -> Result<Commit> {
        match compression {
            Compression::None => self.commit_new(tenant, hash, generation, size, refs).await,
            other => Err(Error::internal(format!(
                "blob index does not store compression {other}"
            ))),
        }
    }

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

    /// Признак сжатия объекта для чтения; `None` — записи с этим поколением нет (как [`BlobIndex::exists`]).
    /// По умолчанию — [`Compression::None`] для существующей записи.
    async fn compression(&self, key: &BlobKey) -> Result<Option<Compression>> {
        Ok(self.exists(key).await?.then_some(Compression::None))
    }

    /// Ближайший срок GC: `MIN(zero_since) + window` среди записей, которые выдал бы
    /// [`BlobIndex::gc_candidates`] (`refcnt = 0`, не надгробие; все организации). `None` — таких записей нет.
    /// Срок может быть в прошлом — есть просроченные кандидаты. Таймер GC взводится на него (CTR-38).
    ///
    /// По умолчанию — деление пополам по `gc_candidates(.., 1)` с точностью до секунды: срок не раньше точного
    /// и не позже его на секунду (записи с `zero_since` позже `now + 1 сутки` не видны). Реализации — запрос
    /// `MIN(zero_since)` по индексу.
    async fn next_gc_due(&self, window: Duration) -> Result<Option<OffsetDateTime>> {
        let window = time::Duration::try_from(window).unwrap_or(time::Duration::MAX);
        let mut lo = OffsetDateTime::UNIX_EPOCH;
        let mut hi = OffsetDateTime::now_utc().saturating_add(time::Duration::DAY);
        if self.gc_candidates(hi, 1).await?.is_empty() {
            return Ok(None);
        }
        if !self.gc_candidates(lo, 1).await?.is_empty() {
            hi = lo;
        }
        // Нет записей с `zero_since < lo`, есть с `zero_since < hi`.
        while hi - lo > time::Duration::SECOND {
            let mid = lo + (hi - lo) / 2;
            if self.gc_candidates(mid, 1).await?.is_empty() {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        Ok(Some(hi.saturating_add(window)))
    }

    /// Отмечает загрузку объекта `key` учётной записью `scope` до `until` (JMAP `uploadUrl`; CTR-38): объект
    /// дедуплицируется в организации, поэтому загруженный файл без ссылок доступен только загрузившей учётной
    /// записи. Повторная отметка продлевает срок до большего. Учёт общий для узлов и переживает перезапуск.
    /// `key.tenant` не совпадает с `scope.tenant` — `Error::Invalid`.
    ///
    /// По умолчанию — `Error::Internal` (учёт не хранит владельцев загрузок).
    async fn mark_upload(
        &self,
        scope: AccountScope,
        key: &BlobKey,
        until: OffsetDateTime,
    ) -> Result<()> {
        let _ = (scope, key, until);
        Err(Error::internal("blob index does not store upload owners"))
    }

    /// Загружала ли учётная запись `scope` объект `key` и действует ли отметка (`until > now`).
    /// Существование записи учёта не проверяет — это [`BlobIndex::exists`]. По умолчанию — `false`.
    async fn upload_owned(
        &self,
        scope: AccountScope,
        key: &BlobKey,
        now: OffsetDateTime,
    ) -> Result<bool> {
        let _ = (scope, key, now);
        Ok(false)
    }

    /// Удаляет отметки загрузок с `until <= now` (все организации), не более `limit`; ответ — число удалённых.
    /// Вызывает GC в каждом проходе (отметка живёт не дольше окна W). По умолчанию — `0`.
    async fn prune_uploads(&self, now: OffsetDateTime, limit: usize) -> Result<usize> {
        let _ = (now, limit);
        Ok(0)
    }
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
    fn compression_strings() {
        for c in Compression::ALL {
            assert_eq!(c.as_str().parse::<Compression>(), Ok(*c));
            assert_eq!(serde_json::to_string(c).expect("ser"), format!("\"{c}\""));
        }
        assert_eq!(Compression::default(), Compression::None);
        assert!("gzip".parse::<Compression>().is_err());
    }

    /// Учёт без признака сжатия: методы по умолчанию.
    struct Plain;

    #[async_trait]
    impl BlobIndex for Plain {
        async fn acquire(&self, _: TenantId, _: &BlobHash, _: u32) -> Result<Acquire> {
            Ok(Acquire::Absent { generation: Gen(1) })
        }
        async fn commit_new(
            &self,
            _: TenantId,
            _: &BlobHash,
            generation: Gen,
            _: u64,
            _: u32,
        ) -> Result<Commit> {
            Ok(Commit::Done { generation })
        }
        async fn release(&self, _: TenantId, _: &BlobHash, _: Gen, _: u32) -> Result<()> {
            Ok(())
        }
        async fn gc_candidates(&self, _: OffsetDateTime, _: usize) -> Result<Vec<BlobKey>> {
            Ok(Vec::new())
        }
        async fn mark_deleting(&self, _: &BlobKey, _: OffsetDateTime) -> Result<bool> {
            Ok(false)
        }
        async fn finalize_delete(&self, _: &BlobKey) -> Result<bool> {
            Ok(false)
        }
        async fn exists(&self, key: &BlobKey) -> Result<bool> {
            Ok(key.generation == Gen(1))
        }
    }

    #[test]
    fn compression_defaults() {
        futures::executor::block_on(async {
            let (t, h) = (TenantId::DEFAULT, BlobHash([1; 32]));
            assert!(!Plain.supports_compression());
            assert_eq!(
                Plain
                    .commit_new_with(t, &h, Gen(1), 10, 1, Compression::None)
                    .await,
                Ok(Commit::Done { generation: Gen(1) })
            );
            assert!(matches!(
                Plain
                    .commit_new_with(t, &h, Gen(1), 10, 1, Compression::Zstd)
                    .await,
                Err(Error::Internal(_))
            ));
            let key = |g| BlobKey {
                tenant: t,
                hash: h,
                generation: Gen(g),
            };
            assert_eq!(
                Plain.compression(&key(1)).await,
                Ok(Some(Compression::None))
            );
            assert_eq!(Plain.compression(&key(2)).await, Ok(None));
        });
    }

    /// Учёт с записями без ссылок, обнулёнными в моменты `zeros`: остальное — методы по умолчанию.
    struct Zeros(Vec<OffsetDateTime>);

    #[async_trait]
    impl BlobIndex for Zeros {
        async fn acquire(&self, _: TenantId, _: &BlobHash, _: u32) -> Result<Acquire> {
            Err(Error::NotFound)
        }
        async fn commit_new(
            &self,
            _: TenantId,
            _: &BlobHash,
            _: Gen,
            _: u64,
            _: u32,
        ) -> Result<Commit> {
            Err(Error::NotFound)
        }
        async fn release(&self, _: TenantId, _: &BlobHash, _: Gen, _: u32) -> Result<()> {
            Ok(())
        }
        async fn gc_candidates(
            &self,
            older_than: OffsetDateTime,
            limit: usize,
        ) -> Result<Vec<BlobKey>> {
            let key = BlobKey {
                tenant: TenantId::DEFAULT,
                hash: BlobHash([1; 32]),
                generation: Gen(1),
            };
            let n = self.0.iter().filter(|z| **z < older_than).count();
            Ok(vec![key; n.min(limit)])
        }
        async fn mark_deleting(&self, _: &BlobKey, _: OffsetDateTime) -> Result<bool> {
            Ok(false)
        }
        async fn finalize_delete(&self, _: &BlobKey) -> Result<bool> {
            Ok(false)
        }
        async fn exists(&self, _: &BlobKey) -> Result<bool> {
            Ok(true)
        }
    }

    #[test]
    fn next_gc_due_default() {
        futures::executor::block_on(async {
            let w = std::time::Duration::from_secs(3600);
            assert_eq!(Zeros(Vec::new()).next_gc_due(w).await, Ok(None));
            let now = OffsetDateTime::now_utc();
            let z = now - time::Duration::days(3);
            let due = Zeros(vec![now, z])
                .next_gc_due(w)
                .await
                .expect("due")
                .expect("some");
            let exact = z + time::Duration::HOUR;
            assert!(
                due >= exact && due - exact <= time::Duration::SECOND,
                "{due} vs {exact}"
            );
            let epoch = OffsetDateTime::UNIX_EPOCH - time::Duration::DAY;
            assert_eq!(
                Zeros(vec![epoch]).next_gc_due(w).await,
                Ok(Some(OffsetDateTime::UNIX_EPOCH + time::Duration::HOUR))
            );
        });
    }

    #[test]
    fn upload_defaults() {
        futures::executor::block_on(async {
            let scope = AccountScope {
                tenant: TenantId::DEFAULT,
                account: crate::ids::AccountId::new(),
            };
            let key = BlobKey {
                tenant: TenantId::DEFAULT,
                hash: BlobHash([1; 32]),
                generation: Gen(1),
            };
            let now = OffsetDateTime::now_utc();
            assert!(matches!(
                Plain.mark_upload(scope, &key, now).await,
                Err(Error::Internal(_))
            ));
            assert_eq!(Plain.upload_owned(scope, &key, now).await, Ok(false));
            assert_eq!(Plain.prune_uploads(now, 10).await, Ok(0));
        });
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
