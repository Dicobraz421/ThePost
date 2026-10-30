//! Полнотекстовый поиск (CONTRACTS.md#Search; SRS 4.6).
//!
//! Индекс учётной записи — набор неизменяемых сегментов в BLOB (`BlobStore::put_raw`, пути
//! `idx/{tenant}/{account}/...`) и манифест в метаданных ([`IndexManifestStore`]). Индексатор (роль indexer,
//! `tp-search`) — подписчик журнала событий ядра; ящик индексирует один узел под лизой
//! `indexer/{tenant}/{account}`. Запросы выполняет роль search через [`SearchIndex`]; модуль может
//! подменить реализацию (SRS 2.5).
//!
//! Нетекстовые условия (флаги, даты, размер, папки) фильтрует вызывающий по метаданным;
//! [`TextQuery`] — только текстовые.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::ids::{MailboxId, MessageId};
use crate::meta::AccountScope;

/// Поле письма для текстового условия.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TextField {
    /// Заголовки и текст, включая имена и текст вложений (IMAP `TEXT`, JMAP `text`).
    Any,
    /// Текст письма (IMAP `BODY`, JMAP `body`).
    Body,
    /// Тема.
    Subject,
    /// `From`.
    From,
    /// `To`.
    To,
    /// `Cc`.
    Cc,
    /// `Bcc`.
    Bcc,
    /// Имена и текст вложений.
    Attachment,
    /// Произвольный заголовок по имени (без учёта регистра); пустой текст — заголовок есть.
    Header(String),
}

/// Текстовый запрос: дерево условий.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TextQuery {
    /// Поле содержит текст (по словам с учётом морфологии, без учёта регистра).
    Term {
        /// Поле.
        field: TextField,
        /// Искомый текст.
        text: String,
    },
    /// Все условия; пустой список — любое письмо.
    And(Vec<TextQuery>),
    /// Хотя бы одно условие; пустой список — ни одно письмо.
    Or(Vec<TextQuery>),
    /// Отрицание.
    Not(Box<TextQuery>),
}

impl TextQuery {
    /// Условие на поле.
    pub fn term(field: TextField, text: impl Into<String>) -> TextQuery {
        TextQuery::Term {
            field,
            text: text.into(),
        }
    }
}

/// Порядок результатов.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SearchSort {
    /// По убыванию релевантности.
    Relevance,
    /// Сначала новые (`receivedAt` по убыванию).
    #[default]
    NewestFirst,
    /// Сначала старые.
    OldestFirst,
}

/// Фрагменты с подсветкой совпадений (JMAP SearchSnippet): совпадения обёрнуты в `<mark>`,
/// остальной текст экранирован для HTML.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Snippet {
    /// Тема с подсветкой; `None` — совпадений в теме нет.
    pub subject: Option<String>,
    /// Фрагмент текста с подсветкой; `None` — совпадений в тексте нет.
    pub preview: Option<String>,
}

/// Выполнение поисковых запросов по индексу учётной записи.
#[async_trait]
pub trait SearchIndex: Send + Sync + 'static {
    /// Письма учётной записи, удовлетворяющие `q`, в папках `mailboxes` (пусто — во всех), не более `limit`.
    /// Индекс может отставать от метаданных: письма, которых уже нет, вызывающий отбрасывает.
    async fn query(
        &self,
        scope: AccountScope,
        mailboxes: &[MailboxId],
        q: &TextQuery,
        sort: SearchSort,
        limit: usize,
    ) -> Result<Vec<MessageId>>;

    /// Фрагменты письма для запроса; письма нет в индексе — `Error::NotFound`.
    async fn snippet(
        &self,
        scope: AccountScope,
        message: MessageId,
        q: &TextQuery,
    ) -> Result<Snippet>;
}

/// Сегмент индекса в BLOB.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexSegment {
    /// Путь `BlobStore::put_raw` (`idx/{tenant}/{account}/...`).
    pub path: String,
    /// Размер, байт.
    pub size: u64,
}

/// Манифест индекса учётной записи: действующий набор сегментов.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct IndexManifest {
    /// Версия: растёт при каждой записи; `0` — индекса нет.
    pub version: u64,
    /// Сегменты.
    pub segments: Vec<IndexSegment>,
}

/// Манифесты индексов в метаданных. Реализация — `tp-meta`; in-memory — `tp-testkit`.
///
/// Сегменты загружаются в BLOB до `put_manifest`; сегменты, выпавшие из манифеста, удаляет индексатор
/// после записи нового манифеста (с задержкой на завершение читающих запросов).
#[async_trait]
pub trait IndexManifestStore: Send + Sync + 'static {
    /// Текущий манифест; индекса нет — версия `0` и пустой набор.
    async fn get_manifest(&self, scope: AccountScope) -> Result<IndexManifest>;

    /// Записывает набор сегментов, если текущая версия равна `expected` (иначе `Error::Conflict`).
    /// Возвращает новую версию.
    async fn put_manifest(
        &self,
        scope: AccountScope,
        expected: u64,
        segments: Vec<IndexSegment>,
    ) -> Result<u64>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::_private::serde_json;
    use std::sync::Arc;

    #[test]
    fn object_safe() {
        fn _index(_: Arc<dyn SearchIndex>) {}
        fn _manifest(_: Arc<dyn IndexManifestStore>) {}
    }

    #[test]
    fn text_query_serde() {
        let q = TextQuery::And(vec![
            TextQuery::term(TextField::From, "ivanov"),
            TextQuery::Not(Box::new(TextQuery::term(
                TextField::Header("List-Id".into()),
                "",
            ))),
        ]);
        let s = serde_json::to_string(&q).expect("serialize");
        assert_eq!(serde_json::from_str::<TextQuery>(&s).expect("parse"), q);
    }
}
