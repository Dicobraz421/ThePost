//! Полнотекстовый поиск Tantivy (индекс на ящик), извлечение текста вложений (SRS 4.6).
//!
//! - [`index`] — схема индекса ящика и менеджер индексов в каталоге;
//! - [`analyzers`] — анализатор текста: слова, регистр, «ё», стеммеры Snowball (ru, en);
//! - [`query`] — исполнитель запросов [`tp_contracts::search::SearchIndex`];
//! - [`indexer`] — роль indexer: подписчик журнала событий, пакетный коммит индекса;
//! - [`persist`] — сегменты индекса в BlobStore и манифест версии, загрузка индекса на другом узле;
//! - [`cache`] — LRU индексов на NVMe узла по размеру, холодная загрузка, метрики;
//! - [`extract`] — текст вложений в изолированном подпроцессе (SYS-04);
//! - [`rebuild`] — перестройка индекса ящика рядом с действующим и атомарная замена манифеста.

pub mod analyzers;
pub mod cache;
pub mod extract;
pub mod index;
pub mod indexer;
pub mod persist;
pub mod query;
pub mod rebuild;

pub use cache::{CacheConfig, CacheStats, IndexCache};
pub use extract::{ExtractConfig, ExtractLimits, Extractor, NotExtracted};
pub use index::{IndexConfig, IndexDoc, IndexManager, IndexOp, MailIndex, SCHEMA_VERSION};
pub use indexer::{DocumentSource, Indexer, IndexerConfig, SUBSCRIBER};
pub use persist::{IndexStore, PersistConfig};
pub use query::{QueryConfig, Search};
pub use rebuild::{RebuildConfig, Rebuilder};

use schemars::JsonSchema;
use serde::Deserialize;

/// Секция `search` конфигурации (поля необязательны, неизвестное — ошибка).
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SearchConfig {
    /// Индексы ящиков на локальном диске.
    pub index: IndexConfig,
    /// Индексатор (роль indexer).
    pub indexer: IndexerConfig,
    /// Выполнение запросов (роль search).
    pub query: QueryConfig,
    /// Сегменты в BlobStore.
    pub persist: PersistConfig,
    /// Кэш индексов на узле (роль search).
    pub cache: CacheConfig,
    /// Извлечение текста вложений (роль indexer).
    pub extract: ExtractConfig,
    /// Перестройка индексов (роль indexer).
    pub rebuild: RebuildConfig,
}

#[cfg(test)]
pub(crate) mod tests {
    use serde::Deserialize;
    use serde::de::value::{Error, MapDeserializer};

    /// Секция конфигурации из пар «поле — строка» (без формата файла).
    pub(crate) fn from_pairs<'de, T: Deserialize<'de>>(
        pairs: &'de [(&'de str, &'de str)],
    ) -> Result<T, Error> {
        T::deserialize(MapDeserializer::<_, Error>::new(pairs.iter().copied()))
    }
}
