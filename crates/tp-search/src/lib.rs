//! Полнотекстовый поиск Tantivy (индекс на ящик), извлечение текста вложений (SRS 4.6).
//!
//! - [`index`] — схема индекса ящика и менеджер индексов в каталоге;
//! - [`analyzers`] — анализатор текста: слова, регистр, «ё», стеммеры Snowball (ru, en);

pub mod analyzers;
pub mod index;

pub use index::{IndexConfig, IndexDoc, IndexManager, IndexOp, MailIndex, SCHEMA_VERSION};

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
