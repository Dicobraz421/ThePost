//! Исполнитель запросов: [`SearchIndex`] по индексам ящиков (CONTRACTS.md#Search, SRS 4.6).
//!
//! [`TextQuery`] переводится в запрос Tantivy: текст условия проходит тот же анализатор, что и индекс
//! (морфология, регистр, «ё»); одно слово — терм, несколько — фраза. `TextField::Any` — любое из текстовых
//! полей, `TextField::Header` — заголовок по имени (пустой текст — заголовок есть). Сортировка — по
//! релевантности (BM25) или по моменту приёма (fast field). Фрагменты — `SnippetGenerator` с `<mark>`.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tantivy::collector::TopDocs;
use tantivy::query::{
    AllQuery, BooleanQuery, EmptyQuery, Occur, PhraseQuery, Query, TermQuery, TermSetQuery,
};
use tantivy::schema::{Field, IndexRecordOption, Value};
use tantivy::snippet::{SnippetGenerator, collapse_overlapped_ranges};
use tantivy::{DateTime, DocAddress, Order, Searcher, TantivyDocument, Term};
use tp_contracts::meta::AccountScope;
use tp_contracts::search::{SearchIndex, SearchSort, Snippet, TextField, TextQuery};
use tp_contracts::{Error, MailboxId, MessageId, Result};

use crate::analyzers;
use crate::index::{Fields, IndexManager, MailIndex, tantivy_err};

/// Выполнение запросов (секция `search.query`; поля необязательны, неизвестное — ошибка).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct QueryConfig {
    /// Длина фрагмента текста письма в [`Snippet::preview`], символов.
    pub preview_chars: usize,
}

impl Default for QueryConfig {
    fn default() -> Self {
        Self { preview_chars: 200 }
    }
}

/// [`SearchIndex`] по индексам [`IndexManager`]. Учётная запись без индекса — пустой результат.
#[derive(Debug, Clone)]
pub struct Search {
    indexes: Arc<IndexManager>,
    cfg: QueryConfig,
}

impl Search {
    /// Исполнитель запросов по индексам `indexes`.
    pub fn new(indexes: Arc<IndexManager>, cfg: QueryConfig) -> Self {
        Self { indexes, cfg }
    }
}

/// Выполняет блокирующую работу с индексом учётной записи вне потоков async.
async fn blocking<T: Send + 'static>(
    indexes: Arc<IndexManager>,
    scope: AccountScope,
    f: impl FnOnce(Option<Arc<MailIndex>>) -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(move || f(indexes.open(scope)?))
        .await
        .map_err(|e| Error::internal(format!("search task: {e}")))?
}

#[async_trait]
impl SearchIndex for Search {
    async fn query(
        &self,
        scope: AccountScope,
        mailboxes: &[MailboxId],
        q: &TextQuery,
        sort: SearchSort,
        limit: usize,
    ) -> Result<Vec<MessageId>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let (mailboxes, q) = (mailboxes.to_vec(), q.clone());
        blocking(self.indexes.clone(), scope, move |index| {
            let Some(index) = index else {
                return Ok(Vec::new());
            };
            run(&index, &mailboxes, &q, sort, limit)
        })
        .await
    }

    async fn snippet(
        &self,
        scope: AccountScope,
        message: MessageId,
        q: &TextQuery,
    ) -> Result<Snippet> {
        let (q, preview_chars) = (q.clone(), self.cfg.preview_chars);
        blocking(self.indexes.clone(), scope, move |index| {
            let index = index.ok_or(Error::NotFound)?;
            snippet(&index, message, &q, preview_chars)
        })
        .await
    }
}

fn run(
    index: &MailIndex,
    mailboxes: &[MailboxId],
    q: &TextQuery,
    sort: SearchSort,
    limit: usize,
) -> Result<Vec<MessageId>> {
    let f = index.fields();
    let mut query = build(f, q, false);
    if !mailboxes.is_empty() {
        let terms = mailboxes
            .iter()
            .map(|m| Term::from_field_text(f.mailbox, &m.to_string()));
        query = Box::new(BooleanQuery::new(vec![
            (Occur::Must, query),
            (Occur::Must, Box::new(TermSetQuery::new(terms))),
        ]));
    }
    let searcher = index.searcher();
    let top = TopDocs::with_limit(limit);
    let order = match sort {
        SearchSort::Relevance => None,
        SearchSort::NewestFirst => Some(Order::Desc),
        SearchSort::OldestFirst => Some(Order::Asc),
    };
    let found: Vec<DocAddress> = match order {
        None => searcher
            .search(&query, &top.order_by_score())
            .map_err(tantivy_err)?
            .into_iter()
            .map(|(_, a)| a)
            .collect(),
        Some(order) => searcher
            .search(
                &query,
                &top.order_by_fast_field::<DateTime>("received_at", order),
            )
            .map_err(tantivy_err)?
            .into_iter()
            .map(|(_, a)| a)
            .collect(),
    };
    found
        .into_iter()
        .map(|a| message_id(&searcher, f.id, a))
        .collect()
}

fn message_id(searcher: &Searcher, id: Field, address: DocAddress) -> Result<MessageId> {
    let doc: TantivyDocument = searcher.doc(address).map_err(tantivy_err)?;
    doc.get_first(id)
        .and_then(|v| v.as_str())
        .ok_or_else(|| Error::internal("search index: document without id"))?
        .parse()
}

/// Запрос Tantivy для [`TextQuery`]. `positive` — только условия вне `Not` (термы для подсветки).
fn build(f: &Fields, q: &TextQuery, positive: bool) -> Box<dyn Query> {
    match q {
        TextQuery::Term { field, text } => term(f, field, text),
        TextQuery::And(qs) if qs.is_empty() => Box::new(AllQuery),
        TextQuery::And(qs) => all(qs.iter().map(|q| (Occur::Must, build(f, q, positive)))),
        TextQuery::Or(qs) if qs.is_empty() => Box::new(EmptyQuery),
        TextQuery::Or(qs) => all(qs.iter().map(|q| (Occur::Should, build(f, q, positive)))),
        TextQuery::Not(_) if positive => Box::new(EmptyQuery),
        TextQuery::Not(q) => all([
            (Occur::Must, Box::new(AllQuery) as Box<dyn Query>),
            (Occur::MustNot, build(f, q, positive)),
        ]),
    }
}

fn all(clauses: impl IntoIterator<Item = (Occur, Box<dyn Query>)>) -> Box<dyn Query> {
    Box::new(BooleanQuery::new(clauses.into_iter().collect()))
}

/// Условие «поле содержит текст»: слова текста после анализатора — терм или фраза в каждом поле.
fn term(f: &Fields, field: &TextField, text: &str) -> Box<dyn Query> {
    let words = analyzers::tokens(text);
    if let TextField::Header(name) = field {
        let name = name.to_lowercase();
        if text.trim().is_empty() {
            return Box::new(TermQuery::new(
                Term::from_field_text(f.header_names, &name),
                IndexRecordOption::Basic,
            ));
        }
        // Точка в имени заголовка — не разделитель пути JSON.
        let path = name.replace('.', "\\.");
        let terms = words.iter().map(|w| {
            let mut t = Term::from_field_json_path(f.header, &path, false);
            t.append_type_and_str(w);
            t
        });
        return words_query(terms.collect());
    }
    if text.trim().is_empty() {
        return Box::new(AllQuery);
    }
    let fields: &[Field] = match field {
        TextField::Any => &[
            f.subject,
            f.from,
            f.to,
            f.cc,
            f.bcc,
            f.body,
            f.attachments,
            f.headers,
        ],
        TextField::Body => &[f.body],
        TextField::Subject => &[f.subject],
        TextField::From => &[f.from],
        TextField::To => &[f.to],
        TextField::Cc => &[f.cc],
        TextField::Bcc => &[f.bcc],
        TextField::Attachment => &[f.attachments],
        TextField::Header(_) => &[],
    };
    all(fields.iter().map(|&field| {
        let terms = words
            .iter()
            .map(|w| Term::from_field_text(field, w))
            .collect();
        (Occur::Should, words_query(terms))
    }))
}

/// Одно слово — терм, несколько — фраза, ни одного (только знаки) — ничего.
fn words_query(mut terms: Vec<Term>) -> Box<dyn Query> {
    match terms.len() {
        0 => Box::new(EmptyQuery),
        1 => Box::new(TermQuery::new(
            terms.remove(0),
            IndexRecordOption::WithFreqs,
        )),
        _ => Box::new(PhraseQuery::new(terms)),
    }
}

fn snippet(
    index: &MailIndex,
    message: MessageId,
    q: &TextQuery,
    preview_chars: usize,
) -> Result<Snippet> {
    let f = index.fields();
    let searcher = index.searcher();
    let by_id = TermQuery::new(index.id_term(message), IndexRecordOption::Basic);
    let (_, address) = *searcher
        .search(&by_id, &TopDocs::with_limit(1).order_by_score())
        .map_err(tantivy_err)?
        .first()
        .ok_or(Error::NotFound)?;
    let doc: TantivyDocument = searcher.doc(address).map_err(tantivy_err)?;
    let text = |field: Field| {
        doc.get_first(field)
            .and_then(|v| v.as_str())
            .unwrap_or_default()
    };
    let query = build(f, q, true);
    let generator = |field: Field, max_chars: usize| -> Result<SnippetGenerator> {
        let mut g = SnippetGenerator::create(&searcher, &*query, field).map_err(tantivy_err)?;
        g.set_max_num_chars(max_chars.max(1));
        Ok(g)
    };
    // Тема — целиком, текст — фрагмент вокруг лучших совпадений.
    let subject = text(f.subject);
    let s = generator(f.subject, subject.len())?.snippet(subject);
    let subject = (!s.highlighted().is_empty()).then(|| mark_all(subject, &s));
    let mut s = generator(f.body, preview_chars)?.snippet(text(f.body));
    let preview = (!s.highlighted().is_empty()).then(|| {
        s.set_snippet_prefix_postfix(MARK, MARK_END);
        s.to_html()
    });
    Ok(Snippet { subject, preview })
}

const MARK: &str = "<mark>";
const MARK_END: &str = "</mark>";

/// Весь `text` с `<mark>` вокруг совпадений фрагмента `s` (построенного по `text`), остальное экранировано.
fn mark_all(text: &str, s: &tantivy::snippet::Snippet) -> String {
    let base = s.fragment_range().start;
    let mut out = String::with_capacity(text.len() + 16);
    let mut pos = 0;
    for r in collapse_overlapped_ranges(s.highlighted()) {
        let (start, end) = (base + r.start, base + r.end);
        escape(text.get(pos..start).unwrap_or_default(), &mut out);
        out.push_str(MARK);
        escape(text.get(start..end).unwrap_or_default(), &mut out);
        out.push_str(MARK_END);
        pos = end;
    }
    escape(text.get(pos..).unwrap_or_default(), &mut out);
    out
}

/// Экранирование HTML (как в `Snippet::to_html`).
fn escape(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#x27;"),
            c => out.push(c),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::{doc, manager, scope};
    use crate::{IndexDoc, IndexOp};
    use std::collections::{BTreeSet, HashMap};
    use time::OffsetDateTime;
    use tp_testkit::{SearchDoc, corpus};

    fn any(text: &str) -> TextQuery {
        TextQuery::term(TextField::Any, text)
    }

    fn body(text: &str) -> TextQuery {
        TextQuery::term(TextField::Body, text)
    }

    /// Корпус testkit в индексе: письма по порядку файлов (минута на письмо), чётные — во «Входящих».
    struct Corpus {
        search: Search,
        scope: AccountScope,
        inbox: MailboxId,
        names: HashMap<MessageId, String>,
        docs: Vec<SearchDoc>,
        _dir: tempfile::TempDir,
    }

    impl Corpus {
        async fn new() -> Corpus {
            let mails = tokio::task::spawn_blocking(corpus)
                .await
                .expect("join")
                .expect("corpus");
            let dir = tempfile::tempdir().expect("tempdir");
            let indexes = Arc::new(manager(&dir));
            let (scope, inbox, archive) = (scope(), MailboxId::new(), MailboxId::new());
            let start = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
            let mut names = HashMap::new();
            let mut docs = Vec::new();
            for (i, m) in mails.iter().filter(|m| !m.is_broken()).enumerate() {
                let mailbox = if i % 2 == 0 { inbox } else { archive };
                let at = start + time::Duration::minutes(i as i64);
                let d = SearchDoc::parse(MessageId::new(), mailbox, at, &m.raw).expect("parse");
                names.insert(d.message, m.name.clone());
                docs.push(d);
            }
            let ops: Vec<IndexOp> = docs
                .iter()
                .map(|d| IndexOp::Upsert(Box::new(doc(d.clone()))))
                .collect();
            let idx = indexes.clone();
            tokio::task::spawn_blocking(move || idx.open_or_create(scope)?.apply(&ops))
                .await
                .expect("join")
                .expect("apply");
            Corpus {
                search: Search::new(indexes, QueryConfig::default()),
                scope,
                inbox,
                names,
                docs,
                _dir: dir,
            }
        }

        async fn ids(
            &self,
            mailboxes: &[MailboxId],
            q: &TextQuery,
            sort: SearchSort,
        ) -> Vec<MessageId> {
            self.search
                .query(self.scope, mailboxes, q, sort, 1000)
                .await
                .expect("query")
        }

        /// Имена писем корпуса, найденных запросом.
        async fn find(&self, q: &TextQuery) -> BTreeSet<String> {
            self.ids(&[], q, SearchSort::NewestFirst)
                .await
                .iter()
                .map(|id| self.names[id].clone())
                .collect()
        }

        /// Имена писем, чей текст (в нижнем регистре) содержит подстроку.
        fn body_contains(&self, needle: &str) -> BTreeSet<String> {
            self.docs
                .iter()
                .filter(|d| d.body.to_lowercase().contains(needle))
                .map(|d| self.names[&d.message].clone())
                .collect()
        }

        fn id(&self, name: &str) -> MessageId {
            *self
                .names
                .iter()
                .find(|(_, n)| *n == name)
                .map(|(id, _)| id)
                .expect("name")
        }
    }

    fn set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|n| (*n).to_owned()).collect()
    }

    #[tokio::test]
    async fn corpus_relevance() {
        let c = Corpus::new().await;
        // Морфология: «письмами» — все письма со словом «письмо» в тексте, и только с основой «письм».
        let found = c.find(&body("письмами")).await;
        assert!(c.body_contains("письмо").is_subset(&found), "{found:?}");
        assert!(found.is_subset(&c.body_contains("письм")), "{found:?}");
        assert!(found.contains("03-plain-utf8-8bit") && found.contains("19-alternative-cyrillic"));
        // «ё» и «е» равнозначны.
        assert_eq!(c.find(&body("еще")).await, c.body_contains("ещё"));
        // Фраза: слова подряд, с учётом форм.
        let phrase = c.find(&body("quarterly reports")).await;
        assert!(phrase.contains("20-mixed-pdf"), "{phrase:?}");
        assert!(c.find(&body("reports quarterly")).await.is_empty());
        // Поля: тема, адрес, вложение, заголовок.
        assert_eq!(
            c.find(&TextQuery::term(TextField::Subject, "report")).await,
            set(&["20-mixed-pdf"])
        );
        assert_eq!(
            c.find(&TextQuery::term(TextField::From, "ivan@example.ru"))
                .await,
            set(&["10-raw-8bit-headers", "16-encoded-addresses"])
        );
        assert!(
            c.find(&TextQuery::term(TextField::Attachment, "report.pdf"))
                .await
                .contains("20-mixed-pdf")
        );
        assert_eq!(
            c.find(&TextQuery::term(
                TextField::Header("Message-ID".into()),
                "corpus-001@example.com"
            ))
            .await,
            set(&["01-plain-ascii"])
        );
        let no_id = c
            .find(&TextQuery::Not(Box::new(TextQuery::term(
                TextField::Header("message-id".into()),
                "",
            ))))
            .await;
        assert!(no_id.contains("10-raw-8bit-headers") && !no_id.contains("01-plain-ascii"));
        // Any — и тема, и тело.
        let fox = c.find(&any("fox")).await;
        assert!(fox.contains("01-plain-ascii") && fox.contains("22-mixed-xlsx"));
        assert!(
            c.find(&any("планерка"))
                .await
                .contains("30-calendar-invite")
        );

        // Логика: And, Or, Not, пустые списки.
        let and = c
            .find(&TextQuery::And(vec![
                any("fox"),
                TextQuery::Not(Box::new(TextQuery::term(TextField::Subject, "report"))),
            ]))
            .await;
        assert!(and.contains("01-plain-ascii") && !and.contains("20-mixed-pdf"));
        let or = c
            .find(&TextQuery::Or(vec![
                TextQuery::term(TextField::Subject, "budget"),
                TextQuery::term(TextField::Subject, "meeting"),
            ]))
            .await;
        assert_eq!(or, set(&["22-mixed-xlsx", "30-calendar-invite"]));
        assert_eq!(c.find(&TextQuery::And(vec![])).await.len(), c.docs.len());
        assert!(c.find(&TextQuery::Or(vec![])).await.is_empty());
        assert!(c.find(&any("!!!")).await.is_empty());

        // Сортировка по дате, лимит, папки.
        let newest = c.ids(&[], &any("fox"), SearchSort::NewestFirst).await;
        let mut oldest = c.ids(&[], &any("fox"), SearchSort::OldestFirst).await;
        oldest.reverse();
        assert_eq!(newest, oldest);
        let at: HashMap<_, _> = c.docs.iter().map(|d| (d.message, d.received_at)).collect();
        assert!(newest.windows(2).all(|w| at[&w[0]] > at[&w[1]]));
        let top = c
            .search
            .query(c.scope, &[], &any("fox"), SearchSort::NewestFirst, 2)
            .await
            .expect("limit");
        assert_eq!(top, newest[..2]);
        assert!(
            c.search
                .query(c.scope, &[], &any("fox"), SearchSort::NewestFirst, 0)
                .await
                .expect("zero")
                .is_empty()
        );
        let inbox = c
            .ids(&[c.inbox], &any("fox"), SearchSort::NewestFirst)
            .await;
        assert!(!inbox.is_empty() && inbox.len() < newest.len());
        assert!(inbox.iter().all(|id| {
            c.docs
                .iter()
                .any(|d| d.message == *id && d.mailbox == c.inbox)
        }));

        // Учётная запись без индекса.
        assert!(
            c.search
                .query(scope(), &[], &any("fox"), SearchSort::Relevance, 10)
                .await
                .expect("empty")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn relevance_order() {
        let dir = tempfile::tempdir().expect("tempdir");
        let indexes = Arc::new(manager(&dir));
        let scope = scope();
        let mailbox = MailboxId::new();
        let mk = |subject: &str, body: &str| IndexDoc {
            message: MessageId::new(),
            mailbox,
            received_at: OffsetDateTime::UNIX_EPOCH,
            subject: subject.to_owned(),
            from: String::new(),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            body: body.to_owned(),
            attachments: String::new(),
            headers: Vec::new(),
        };
        let filler = "Обсудили планы отдела, сроки поставок и график отпусков на следующий месяц. ";
        let docs = [
            mk(
                "Q&A: квартальный <отчёт>",
                "Отчёт готов, отчёты по филиалам приложены.",
            ),
            mk("Планы", &format!("{}Отчёт будет позже.", filler.repeat(5))),
            mk("Отпуск", filler),
        ];
        let ops: Vec<_> = docs
            .iter()
            .cloned()
            .map(|d| IndexOp::Upsert(Box::new(d)))
            .collect();
        let idx = indexes.clone();
        tokio::task::spawn_blocking(move || idx.open_or_create(scope)?.apply(&ops))
            .await
            .expect("join")
            .expect("apply");
        let search = Search::new(indexes, QueryConfig::default());
        let hits = search
            .query(scope, &[], &any("отчеты"), SearchSort::Relevance, 10)
            .await
            .expect("query");
        assert_eq!(hits, [docs[0].message, docs[1].message]);
        // Экранирование HTML вне подсветки.
        let s = search
            .snippet(scope, docs[0].message, &any("отчеты"))
            .await
            .expect("snippet");
        let subject = s.subject.expect("subject");
        assert!(
            subject.starts_with("Q&amp;A: квартальный &lt;<mark>отчёт</mark>"),
            "{subject}"
        );
        assert!(
            s.preview
                .expect("preview")
                .starts_with("<mark>Отчёт</mark> готов, <mark>отчёты</mark>")
        );
    }

    #[tokio::test]
    async fn snippets() {
        let c = Corpus::new().await;
        let id = c.id("03-plain-utf8-8bit");
        let s = c
            .search
            .snippet(c.scope, id, &body("письма"))
            .await
            .expect("snippet");
        assert_eq!(s.subject, None);
        let preview = s.preview.expect("preview");
        assert!(preview.contains("<mark>письмо</mark>"), "{preview}");
        assert!(preview.chars().count() <= QueryConfig::default().preview_chars + 20);

        let s = c
            .search
            .snippet(
                c.scope,
                id,
                &TextQuery::Or(vec![
                    any("кириллица"),
                    TextQuery::Not(Box::new(body("булок"))),
                ]),
            )
            .await
            .expect("snippet");
        assert_eq!(s.subject.as_deref(), Some("<mark>Кириллица</mark> UTF-8"));
        let preview = s.preview.expect("preview");
        assert!(preview.contains("<mark>кириллицы</mark>") && !preview.contains("<mark>булок"));

        // Подсветка темы; совпадений в тексте нет.
        let html = c.id("01-plain-ascii");
        let s = c
            .search
            .snippet(c.scope, html, &TextQuery::term(TextField::Subject, "ascii"))
            .await
            .expect("snippet");
        assert_eq!(s.subject.as_deref(), Some("Plain <mark>ASCII</mark>"));
        assert_eq!(s.preview, None);

        assert!(matches!(
            c.search
                .snippet(c.scope, MessageId::new(), &any("fox"))
                .await,
            Err(Error::NotFound)
        ));
        assert!(matches!(
            c.search.snippet(scope(), id, &any("fox")).await,
            Err(Error::NotFound)
        ));
    }

    #[test]
    fn config_defaults_and_unknown() {
        assert_eq!(
            crate::tests::from_pairs::<QueryConfig>(&[]).expect("defaults"),
            QueryConfig::default()
        );
        assert!(crate::tests::from_pairs::<QueryConfig>(&[("preview", "1")]).is_err());
    }
}
