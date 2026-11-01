//! Схема индекса ящика и менеджер индексов (SRS 4.6).
//!
//! Единица индекса — учётная запись: каталог `{dir}/{tenant}/{account}` с индексом Tantivy. Индексируется
//! неизменяемое содержимое письма (тема, адреса, тело, текст вложений, заголовки); момент приёма — fast field
//! для сортировки. Флаги не индексируются, папка — только для фильтра запроса.
//!
//! Методы [`MailIndex`] и [`IndexManager`] блокирующие (файловый ввод-вывод): из async-кода — через
//! `spawn_blocking`. Изменяет индекс один индексатор (лиза раздела журнала), внутри процесса записи
//! индекса упорядочены.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use schemars::JsonSchema;
use serde::Deserialize;
use tantivy::directory::MmapDirectory;
use tantivy::schema::{
    DateOptions, Field, IndexRecordOption, JsonObjectOptions, OwnedValue, STORED, STRING, Schema,
    TextFieldIndexing, TextOptions,
};
use tantivy::{
    DateTime, Index, IndexReader, IndexWriter, ReloadPolicy, Searcher, TantivyDocument, Term,
};
use time::OffsetDateTime;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};
use tp_contracts::meta::AccountScope;
use tp_contracts::search::IndexManifest;
use tp_contracts::{AccountId, Error, MailboxId, MessageId, Result, TenantId};

use crate::analyzers::{self, TOKENIZER};

/// Версия схемы индекса (манифест индекса, CONTRACTS.md#Search): индекс со схемой старше перестраивается.
pub const SCHEMA_VERSION: u32 = 1;

/// Наименьший буфер записи Tantivy на поток, байт.
const MIN_WRITER_MEMORY: usize = 15 << 20;

/// Индексы ящиков на локальном диске (секция `search.index`; поля необязательны, неизвестное — ошибка).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct IndexConfig {
    /// Каталог индексов ящиков (локальный NVMe). Требует перезапуска.
    pub dir: PathBuf,
    /// Буфер записи индекса ящика на пакет, байт (меньше 15 МиБ — 15 МиБ, требование Tantivy).
    pub writer_memory_bytes: usize,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            dir: PathBuf::from("/var/lib/thepost/search"),
            writer_memory_bytes: 32 << 20,
        }
    }
}

/// Письмо для индекса: тексты полей (декодированные), папка и момент приёма.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexDoc {
    /// Письмо.
    pub message: MessageId,
    /// Папка.
    pub mailbox: MailboxId,
    /// Момент приёма (сортировка результатов).
    pub received_at: OffsetDateTime,
    /// Тема.
    pub subject: String,
    /// `From`.
    pub from: String,
    /// `To`.
    pub to: String,
    /// `Cc`.
    pub cc: String,
    /// `Bcc`.
    pub bcc: String,
    /// Текст письма.
    pub body: String,
    /// Имена и текст вложений.
    pub attachments: String,
    /// Заголовки: имя и значение.
    pub headers: Vec<(String, String)>,
}

/// Изменение индекса.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexOp {
    /// Добавить письмо или заменить его прежний документ.
    Upsert(Box<IndexDoc>),
    /// Удалить письмо (отсутствие — не ошибка).
    Delete(MessageId),
}

/// Поля схемы.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Fields {
    pub(crate) id: Field,
    pub(crate) mailbox: Field,
    pub(crate) received_at: Field,
    pub(crate) subject: Field,
    pub(crate) from: Field,
    pub(crate) to: Field,
    pub(crate) cc: Field,
    pub(crate) bcc: Field,
    pub(crate) body: Field,
    pub(crate) attachments: Field,
    /// Значения всех заголовков (поиск по любому полю).
    pub(crate) headers: Field,
    /// Заголовки по имени (нижний регистр): JSON-объект «имя → значения».
    pub(crate) header: Field,
    /// Имена заголовков письма (нижний регистр): условие «заголовок есть».
    pub(crate) header_names: Field,
}

/// Схема индекса ящика.
pub(crate) fn schema() -> (Schema, Fields) {
    let indexing = TextFieldIndexing::default()
        .set_tokenizer(TOKENIZER)
        .set_index_option(IndexRecordOption::WithFreqsAndPositions);
    let text = TextOptions::default().set_indexing_options(indexing.clone());
    let stored = text.clone().set_stored();
    let mut b = Schema::builder();
    let fields = Fields {
        id: b.add_text_field("id", STRING | STORED),
        mailbox: b.add_text_field("mailbox", STRING),
        received_at: b.add_date_field("received_at", DateOptions::default().set_fast()),
        subject: b.add_text_field("subject", stored.clone()),
        from: b.add_text_field("from", text.clone()),
        to: b.add_text_field("to", text.clone()),
        cc: b.add_text_field("cc", text.clone()),
        bcc: b.add_text_field("bcc", text.clone()),
        body: b.add_text_field("body", stored),
        attachments: b.add_text_field("attachments", text.clone()),
        headers: b.add_text_field("headers", text),
        header: b.add_json_field(
            "header",
            JsonObjectOptions::default().set_indexing_options(indexing),
        ),
        header_names: b.add_text_field("header_names", STRING),
    };
    (b.build(), fields)
}

/// Ошибка Tantivy на границе крейта.
pub(crate) fn tantivy_err(e: tantivy::TantivyError) -> Error {
    Error::internal(format!("search index: {e}"))
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Блокирующая работа (файлы индекса) вне потоков async.
pub(crate) async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| Error::internal(format!("search task: {e}")))?
}

/// Имя файла сегмента Tantivy: `{uuid без дефисов}.{расширение}`.
pub(crate) fn is_segment_file(name: &str) -> bool {
    name.len() > 33
        && name.as_bytes()[32] == b'.'
        && name.as_bytes()[..32].iter().all(u8::is_ascii_hexdigit)
}

/// Зафиксированное состояние индекса: файлы сегментов (без `meta.json`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexFiles {
    /// Opstamp последнего коммита.
    pub(crate) opstamp: u64,
    /// Файл сегмента и число документов (у первого по имени файла сегмента, у остальных 0).
    pub(crate) files: Vec<(String, u64)>,
}

/// Индекс одной учётной записи.
pub struct MailIndex {
    index: Index,
    reader: IndexReader,
    fields: Fields,
    writer_memory: usize,
    /// Записи индекса в процессе — по одной (блокировка каталога Tantivy).
    write: Mutex<()>,
    /// Каталог индекса.
    path: PathBuf,
    /// Манифест, которому соответствуют локальные файлы (`version == 0` — индекс не сохранялся).
    synced: Mutex<IndexManifest>,
}

impl fmt::Debug for MailIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailIndex")
            .field("index", &self.index)
            .finish_non_exhaustive()
    }
}

impl MailIndex {
    fn open(path: &Path, writer_memory: usize) -> Result<MailIndex> {
        let dir = MmapDirectory::open(path).map_err(|e| Error::internal(format!("{e}")))?;
        let (schema, fields) = schema();
        let index = Index::open_or_create(dir, schema).map_err(tantivy_err)?;
        analyzers::register(&index);
        let reader = index
            .reader_builder()
            .reload_policy(ReloadPolicy::Manual)
            .try_into()
            .map_err(tantivy_err)?;
        Ok(MailIndex {
            index,
            reader,
            fields,
            writer_memory: writer_memory.max(MIN_WRITER_MEMORY),
            write: Mutex::new(()),
            path: path.to_path_buf(),
            synced: Mutex::new(IndexManifest::default()),
        })
    }

    /// Каталог индекса.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Манифест, которому соответствуют локальные файлы.
    pub fn synced(&self) -> IndexManifest {
        lock(&self.synced).clone()
    }

    pub(crate) fn set_synced(&self, manifest: IndexManifest) {
        *lock(&self.synced) = manifest;
    }

    /// Перечитывает `meta.json` (после загрузки файлов из BlobStore). Блокирующий.
    pub(crate) fn reload(&self) -> Result<()> {
        self.reader.reload().map_err(tantivy_err)
    }

    /// Файлы последнего коммита. Блокирующий.
    pub(crate) fn files(&self) -> Result<IndexFiles> {
        let _write = lock(&self.write);
        let meta = self.index.load_metas().map_err(tantivy_err)?;
        let docs: HashMap<String, u64> = meta
            .segments
            .iter()
            .map(|s| (s.id().uuid_string(), u64::from(s.num_docs())))
            .collect();
        let mut names: Vec<String> = meta
            .list_segment_files()
            .into_iter()
            .filter_map(|p| p.to_str().map(str::to_owned))
            .filter(|name| self.path.join(name).is_file())
            .collect();
        names.sort();
        let mut seen = HashSet::new();
        let files = names
            .into_iter()
            .map(|name| {
                let uuid = name.get(..32).unwrap_or_default().to_owned();
                let n = if seen.insert(uuid.clone()) {
                    docs.get(&uuid).copied().unwrap_or(0)
                } else {
                    0
                };
                (name, n)
            })
            .collect();
        Ok(IndexFiles {
            opstamp: meta.opstamp,
            files,
        })
    }

    /// Удаляет из каталога файлы сегментов не из `keep` (выпавшие после слияний и загрузок). Блокирующий.
    pub(crate) fn prune(&self, keep: &HashSet<String>) -> Result<()> {
        let _write = lock(&self.write);
        prune_dir(&self.path, keep)
    }

    pub(crate) fn fields(&self) -> &Fields {
        &self.fields
    }

    /// Снимок индекса на момент последнего [`MailIndex::apply`].
    pub fn searcher(&self) -> Searcher {
        self.reader.searcher()
    }

    /// Число писем в индексе.
    pub fn docs(&self) -> u64 {
        self.searcher().num_docs()
    }

    /// Применяет изменения одним коммитом; после возврата они видны запросам. Блокирующий.
    pub fn apply(&self, ops: &[IndexOp]) -> Result<()> {
        if ops.is_empty() {
            return Ok(());
        }
        let _write = lock(&self.write);
        let mut writer: IndexWriter = self
            .index
            .writer_with_num_threads(1, self.writer_memory)
            .map_err(tantivy_err)?;
        for op in ops {
            match op {
                IndexOp::Upsert(doc) => {
                    writer.delete_term(self.id_term(doc.message));
                    writer
                        .add_document(self.document(doc))
                        .map_err(tantivy_err)?;
                }
                IndexOp::Delete(message) => {
                    writer.delete_term(self.id_term(*message));
                }
            }
        }
        writer.commit().map_err(tantivy_err)?;
        // Слияния — до возврата: набор файлов коммита не меняется, пока его выгружают в BlobStore.
        writer.wait_merging_threads().map_err(tantivy_err)?;
        self.reader.reload().map_err(tantivy_err)
    }

    pub(crate) fn id_term(&self, message: MessageId) -> Term {
        Term::from_field_text(self.fields.id, &message.to_string())
    }

    fn document(&self, d: &IndexDoc) -> TantivyDocument {
        let f = &self.fields;
        let mut doc = TantivyDocument::default();
        doc.add_text(f.id, d.message.to_string());
        doc.add_text(f.mailbox, d.mailbox.to_string());
        doc.add_date(f.received_at, DateTime::from_utc(d.received_at));
        for (field, text) in [
            (f.subject, &d.subject),
            (f.from, &d.from),
            (f.to, &d.to),
            (f.cc, &d.cc),
            (f.bcc, &d.bcc),
            (f.body, &d.body),
            (f.attachments, &d.attachments),
        ] {
            doc.add_text(field, text);
        }
        let mut by_name: BTreeMap<String, Vec<OwnedValue>> = BTreeMap::new();
        for (name, value) in &d.headers {
            doc.add_text(f.headers, value);
            by_name
                .entry(name.to_lowercase())
                .or_default()
                .push(OwnedValue::Str(value.clone()));
        }
        for name in by_name.keys().collect::<BTreeSet<_>>() {
            doc.add_text(f.header_names, name);
        }
        let object = by_name
            .into_iter()
            .map(|(name, values)| (name, OwnedValue::Array(values)))
            .collect();
        doc.add_object(f.header, object);
        doc
    }
}

/// Удаляет из каталога `dir` файлы сегментов не из `keep`. Блокирующий.
pub(crate) fn prune_dir(dir: &Path, keep: &HashSet<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if is_segment_file(name) && !keep.contains(name) {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}

type Key = (TenantId, AccountId);

/// Удаляет каталог; его отсутствие — не ошибка. Блокирующий.
fn remove_dir(path: &Path) -> Result<()> {
    match std::fs::remove_dir_all(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// Индексы учётных записей в каталоге [`IndexConfig::dir`]; открытые держит в памяти.
#[derive(Debug)]
pub struct IndexManager {
    cfg: IndexConfig,
    open: Mutex<HashMap<Key, Arc<MailIndex>>>,
    /// Последовательность изменений индекса учётной записи: запись и выгрузка, загрузка, вытеснение, замена.
    gates: Mutex<HashMap<Key, Arc<AsyncMutex<()>>>>,
    /// Перестраиваемые индексы: строятся рядом с действующими и получают те же изменения индексатора.
    shadows: Mutex<HashMap<Key, Arc<MailIndex>>>,
}

impl IndexManager {
    /// Менеджер индексов по конфигурации.
    pub fn new(cfg: IndexConfig) -> Self {
        Self {
            cfg,
            open: Mutex::new(HashMap::new()),
            gates: Mutex::new(HashMap::new()),
            shadows: Mutex::new(HashMap::new()),
        }
    }

    /// Каталог перестраиваемого индекса учётной записи: рядом с действующим.
    pub fn shadow_path(&self, scope: AccountScope) -> PathBuf {
        self.cfg
            .dir
            .join(scope.tenant.to_string())
            .join(format!("{}.rebuild", scope.account))
    }

    /// Перестраиваемый индекс учётной записи, если перестройка идёт.
    pub fn shadow(&self, scope: AccountScope) -> Option<Arc<MailIndex>> {
        lock(&self.shadows)
            .get(&(scope.tenant, scope.account))
            .cloned()
    }

    /// Начинает перестройку: пустой индекс в [`IndexManager::shadow_path`] (прежний каталог удаляется).
    /// Блокирующий.
    pub(crate) fn begin_shadow(&self, scope: AccountScope) -> Result<Arc<MailIndex>> {
        let path = self.shadow_path(scope);
        remove_dir(&path)?;
        std::fs::create_dir_all(&path)?;
        let index = Arc::new(MailIndex::open(&path, self.cfg.writer_memory_bytes)?);
        lock(&self.shadows).insert((scope.tenant, scope.account), index.clone());
        Ok(index)
    }

    /// Отменяет перестройку: перестраиваемый индекс и его каталог удаляются. Блокирующий.
    pub(crate) fn drop_shadow(&self, scope: AccountScope) -> Result<()> {
        lock(&self.shadows).remove(&(scope.tenant, scope.account));
        remove_dir(&self.shadow_path(scope))
    }

    /// Заменяет действующий индекс перестроенным: каталог перестройки становится каталогом индекса.
    /// Вызывать под [`IndexManager::lock`]. Блокирующий.
    pub(crate) fn promote_shadow(&self, scope: AccountScope) -> Result<Arc<MailIndex>> {
        let key = (scope.tenant, scope.account);
        let shadow = lock(&self.shadows)
            .remove(&key)
            .ok_or_else(|| Error::internal("search index is not being rebuilt"))?;
        let synced = shadow.synced();
        drop(shadow);
        let mut open = lock(&self.open);
        open.remove(&key);
        let path = self.path(scope);
        remove_dir(&path)?;
        std::fs::rename(self.shadow_path(scope), &path)?;
        let index = Arc::new(MailIndex::open(&path, self.cfg.writer_memory_bytes)?);
        index.set_synced(synced);
        open.insert(key, index.clone());
        Ok(index)
    }

    /// Захватывает индекс учётной записи для изменения (запись с выгрузкой, загрузка, вытеснение,
    /// замена перестроенным): такие операции над одним индексом выполняются по одной.
    pub async fn lock(&self, scope: AccountScope) -> OwnedMutexGuard<()> {
        let gate = lock(&self.gates)
            .entry((scope.tenant, scope.account))
            .or_default()
            .clone();
        gate.lock_owned().await
    }

    /// Каталог индекса учётной записи.
    pub fn path(&self, scope: AccountScope) -> PathBuf {
        self.cfg
            .dir
            .join(scope.tenant.to_string())
            .join(scope.account.to_string())
    }

    /// Открытый в памяти индекс учётной записи (без обращения к диску).
    pub fn cached(&self, scope: AccountScope) -> Option<Arc<MailIndex>> {
        lock(&self.open)
            .get(&(scope.tenant, scope.account))
            .cloned()
    }

    /// Забывает открытый индекс (файлы остаются): следующее открытие перечитает каталог.
    pub fn forget(&self, scope: AccountScope) -> Option<Arc<MailIndex>> {
        lock(&self.open).remove(&(scope.tenant, scope.account))
    }

    /// Закрывает индекс и удаляет его каталог (вытеснение, замена). Запросы, уже получившие индекс,
    /// дорабатывают по открытым файлам. Блокирующий.
    pub fn evict(&self, scope: AccountScope) -> Result<()> {
        self.forget(scope);
        remove_dir(&self.path(scope))
    }

    /// Индекс учётной записи; нет — создаётся пустой. Блокирующий.
    pub fn open_or_create(&self, scope: AccountScope) -> Result<Arc<MailIndex>> {
        self.get(scope, true)?
            .ok_or_else(|| Error::internal("search index was not created"))
    }

    /// Индекс учётной записи; нет — `None` (письма ещё не индексировались). Блокирующий.
    pub fn open(&self, scope: AccountScope) -> Result<Option<Arc<MailIndex>>> {
        self.get(scope, false)
    }

    fn get(&self, scope: AccountScope, create: bool) -> Result<Option<Arc<MailIndex>>> {
        let mut open = lock(&self.open);
        if let Some(index) = open.get(&(scope.tenant, scope.account)) {
            return Ok(Some(index.clone()));
        }
        let path = self.path(scope);
        if !create && !path.join("meta.json").exists() {
            return Ok(None);
        }
        std::fs::create_dir_all(&path)?;
        let index = Arc::new(MailIndex::open(&path, self.cfg.writer_memory_bytes)?);
        open.insert((scope.tenant, scope.account), index.clone());
        Ok(Some(index))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tantivy::collector::Count;
    use tantivy::query::TermQuery;
    use tp_testkit::{LoadModel, SearchDoc, SyntheticMail};

    pub(crate) fn scope() -> AccountScope {
        AccountScope {
            tenant: TenantId::new(),
            account: AccountId::new(),
        }
    }

    pub(crate) fn manager(dir: &tempfile::TempDir) -> IndexManager {
        IndexManager::new(IndexConfig {
            dir: dir.path().to_path_buf(),
            ..IndexConfig::default()
        })
    }

    /// Документ индекса из разобранного письма testkit.
    pub(crate) fn doc(d: SearchDoc) -> IndexDoc {
        IndexDoc {
            message: d.message,
            mailbox: d.mailbox,
            received_at: d.received_at.unwrap_or(OffsetDateTime::UNIX_EPOCH),
            subject: d.subject,
            from: d.from,
            to: d.to,
            cc: d.cc,
            bcc: d.bcc,
            body: d.body,
            attachments: d.attachments,
            headers: d.headers,
        }
    }

    /// Число писем с термом: для темы — после анализатора, для служебных полей — как есть.
    fn count(index: &MailIndex, field: Field, text: &str) -> usize {
        let term = if field == index.fields.subject {
            analyzers::tokens(text).remove(0)
        } else {
            text.to_owned()
        };
        let q = TermQuery::new(
            Term::from_field_text(field, &term),
            IndexRecordOption::Basic,
        );
        index.searcher().search(&q, &Count).expect("search")
    }

    #[test]
    fn index_and_search_1000() {
        let dir = tempfile::tempdir().expect("tempdir");
        let m = manager(&dir);
        let scope = scope();
        assert!(m.open(scope).expect("open").is_none());
        let index = m.open_or_create(scope).expect("create");
        let mailbox = MailboxId::new();
        let start = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        // Письма короче модели Д.3: тест о числе писем, а не об объёме текста (отладочная сборка).
        let model = LoadModel {
            avg_body: 2 << 10,
            avg_attachment: 4 << 10,
            ..LoadModel::default()
        };
        let ops: Vec<IndexOp> = SyntheticMail::new(7, model)
            .take(1000)
            .enumerate()
            .map(|(i, raw)| {
                let at = start + time::Duration::minutes(i as i64);
                let d = SearchDoc::parse(MessageId::new(), mailbox, at, &raw).expect("parse");
                IndexOp::Upsert(Box::new(doc(d)))
            })
            .collect();
        index.apply(&ops).expect("apply");
        assert_eq!(index.docs(), 1000);
        let f = index.fields;
        // Тема «Synthetic {n}: ...» у каждого письма, номер — у одного.
        assert_eq!(count(&index, f.subject, "synthetic"), 1000);
        assert_eq!(count(&index, f.subject, "537"), 1);
        assert_eq!(count(&index, f.mailbox, &mailbox.to_string()), 1000);
        assert!(count(&index, f.header_names, "message-id") > 0);

        // Повторная индексация письма заменяет документ, удаление — убирает.
        let IndexOp::Upsert(first) = &ops[0] else {
            unreachable!()
        };
        index
            .apply(&[IndexOp::Upsert(first.clone())])
            .expect("reindex");
        assert_eq!(index.docs(), 1000);
        index
            .apply(&[
                IndexOp::Delete(first.message),
                IndexOp::Delete(MessageId::new()),
            ])
            .expect("delete");
        assert_eq!(index.docs(), 999);

        // Индекс переживает перезапуск: новый менеджер открывает каталог.
        drop(index);
        let reopened = manager(&dir).open(scope).expect("reopen").expect("exists");
        assert_eq!(reopened.docs(), 999);
        assert_eq!(count(&reopened, f.subject, "537"), 1);
    }

    #[test]
    fn config_defaults_and_unknown() {
        assert_eq!(
            crate::tests::from_pairs::<IndexConfig>(&[]).expect("defaults"),
            IndexConfig::default()
        );
        assert!(crate::tests::from_pairs::<IndexConfig>(&[("dirr", "/x")]).is_err());
    }
}
