//! Сегменты индекса в BlobStore и манифест версии (SRS 4.6, CONTRACTS.md#Search).
//!
//! Индекс ящика на узле — каталог Tantivy (штатный `MmapDirectory`). После коммита индексатор выгружает
//! новые файлы сегментов ([`BlobStore::put_raw`]: файлы Tantivy неизменяемы, путь пишется один раз) и снимок
//! `meta.json`, затем записывает манифест ([`IndexManifestStore::put_manifest`]) с CAS по версии. Другой узел
//! загружает индекс по манифесту в свой каталог: недостающие файлы и `meta.json` последним (атомарно).
//!
//! Пути: `idx/{tenant}/{account}/{поколение}/{файл}`; поколение новое у перестроенного индекса, снимок
//! `meta.json` — `meta.{opstamp}.{случайное}.json`. Число писем сегмента — у первого по имени файла сегмента
//! Tantivy, у остальных файлов и снимка — 0, поэтому [`IndexManifest::docs`] — число писем индекса.
//! Файлы, выпавшие из манифеста, удаляет сборка мусора по листингу префикса ящика не раньше
//! [`PersistConfig::gc_delay`] после записи: узлы, загружающие прежнюю версию, успевают её дочитать.
//!
//! Операции с индексом учётной записи — под [`IndexManager::lock`].

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
use futures::TryStreamExt;
use schemars::JsonSchema;
use serde::Deserialize;
use tantivy::index::SegmentId;
use time::OffsetDateTime;
use tokio::io::AsyncWriteExt;
use tokio::time::Instant;
use tp_contracts::blob::BlobStore;
use tp_contracts::config::duration;
use tp_contracts::meta::AccountScope;
use tp_contracts::search::{
    IndexManifest, IndexManifestStore, IndexSegment, ManifestUpdate, RebuildReason, RebuildRequest,
};
use tp_contracts::{AccountId, Error, Result, TenantId};

use crate::index::{IndexManager, MailIndex, SCHEMA_VERSION, blocking, lock};

/// Файл состояния индекса Tantivy.
const META_FILE: &str = "meta.json";
/// Начало имени снимка `meta.json` в BlobStore.
const META_BLOB: &str = "meta.";

/// Сегменты в BlobStore (секция `search.persist`; поля необязательны, неизвестное — ошибка).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PersistConfig {
    /// Через сколько после записи удаляются файлы, выпавшие из манифеста (`15m`); не чаще этого же
    /// интервала для ящика выполняется сборка мусора.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub gc_delay: Duration,
}

impl Default for PersistConfig {
    fn default() -> Self {
        Self {
            gc_delay: Duration::from_secs(15 * 60),
        }
    }
}

/// Префикс путей индекса учётной записи в BlobStore.
pub fn prefix(scope: AccountScope) -> String {
    format!("idx/{}/{}/", scope.tenant, scope.account)
}

/// Случайная строка для имён (поколение, снимок `meta.json`).
pub(crate) fn random_name() -> String {
    SegmentId::generate_random().short_uuid_string()
}

/// Поколение манифеста: каталог его файлов под префиксом ящика.
fn generation(scope: AccountScope, m: &IndexManifest) -> Option<&str> {
    let p = prefix(scope);
    m.segments
        .first()
        .and_then(|s| s.path.strip_prefix(p.as_str()))
        .and_then(|rest| rest.split_once('/'))
        .map(|(generation, _)| generation)
}

/// Имя локального файла для пути в BlobStore.
fn local_name(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    if name.starts_with(META_BLOB) {
        META_FILE
    } else {
        name
    }
}

type Key = (TenantId, AccountId);

/// Сегменты индексов в BlobStore и манифесты в метаданных.
pub struct IndexStore {
    blob: Arc<dyn BlobStore>,
    manifests: Arc<dyn IndexManifestStore>,
    cfg: PersistConfig,
    /// Последняя сборка мусора ящика.
    last_gc: Mutex<HashMap<Key, Instant>>,
    /// Ящики, чей индекс перестраивается на этом узле: файлы нового поколения ещё не в манифесте.
    building: Mutex<HashSet<Key>>,
}

impl std::fmt::Debug for IndexStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexStore")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

/// Как записывается манифест.
#[derive(Debug, Clone, Copy)]
enum Mode {
    /// Изменения индексатора: запрос перестройки сохраняется.
    Live,
    /// Завершение перестройки, начатой при запросе `request`; появился другой запрос — `Conflict`.
    #[allow(dead_code)]
    Rebuilt(Option<RebuildRequest>),
}

impl IndexStore {
    /// Хранилище сегментов `blob` с манифестами `manifests`.
    pub fn new(
        blob: Arc<dyn BlobStore>,
        manifests: Arc<dyn IndexManifestStore>,
        cfg: PersistConfig,
    ) -> Self {
        Self {
            blob,
            manifests,
            cfg,
            last_gc: Mutex::new(HashMap::new()),
            building: Mutex::new(HashSet::new()),
        }
    }

    /// Текущий манифест индекса учётной записи.
    pub async fn manifest(&self, scope: AccountScope) -> Result<IndexManifest> {
        self.manifests.get_manifest(scope).await
    }

    /// Выгружает последний коммит индекса и записывает манифест. Версия манифеста изменилась только
    /// запросом перестройки — запись повторяется с новой версией; изменился набор сегментов (индекс пишет
    /// другой узел) — `Error::Conflict`.
    pub async fn publish(
        &self,
        scope: AccountScope,
        index: &Arc<MailIndex>,
    ) -> Result<IndexManifest> {
        let base = index.synced();
        let generation = generation(scope, &base).map_or_else(random_name, str::to_owned);
        self.upload(scope, index, &base, &generation, Mode::Live)
            .await
    }

    async fn upload(
        &self,
        scope: AccountScope,
        index: &Arc<MailIndex>,
        base: &IndexManifest,
        generation: &str,
        mode: Mode,
    ) -> Result<IndexManifest> {
        let files = {
            let index = index.clone();
            blocking(move || index.files()).await?
        };
        let dir = format!("{}{generation}/", prefix(scope));
        let meta_prefix = format!("{dir}{META_BLOB}{}.", files.opstamp);
        if matches!(mode, Mode::Live)
            && base
                .segments
                .iter()
                .any(|s| s.path.starts_with(&meta_prefix))
        {
            return Ok(base.clone());
        }
        let have: HashMap<&str, u64> = base
            .segments
            .iter()
            .map(|s| (s.path.as_str(), s.size))
            .collect();
        let mut segments = Vec::with_capacity(files.files.len() + 1);
        for (name, docs) in &files.files {
            let path = format!("{dir}{name}");
            let size = match have.get(path.as_str()) {
                Some(size) => *size,
                None => self.put_file(&path, &index.path().join(name)).await?,
            };
            segments.push(IndexSegment {
                path,
                size,
                docs: *docs,
            });
        }
        let path = format!("{meta_prefix}{}.json", random_name());
        let size = self.put_file(&path, &index.path().join(META_FILE)).await?;
        segments.push(IndexSegment {
            path,
            size,
            docs: 0,
        });

        let update = ManifestUpdate {
            schema: SCHEMA_VERSION,
            segments,
            rebuilt: matches!(mode, Mode::Rebuilt(_)),
        };
        let (mut expected, mut rebuild) = (base.version, base.rebuild);
        let version = loop {
            match self
                .manifests
                .put_manifest(scope, expected, update.clone())
                .await
            {
                Ok(version) => break version,
                Err(Error::Conflict) => {
                    let fresh = self.manifests.get_manifest(scope).await?;
                    let request_only = fresh.version != expected
                        && fresh.segments == base.segments
                        && match mode {
                            Mode::Live => true,
                            Mode::Rebuilt(request) => fresh.rebuild == request,
                        };
                    if !request_only {
                        return Err(Error::Conflict);
                    }
                    (expected, rebuild) = (fresh.version, fresh.rebuild);
                }
                Err(e) => return Err(e),
            }
        };
        let manifest = IndexManifest {
            version,
            schema: SCHEMA_VERSION,
            segments: update.segments,
            rebuild: match mode {
                Mode::Live => rebuild,
                Mode::Rebuilt(_) => None,
            },
        };
        index.set_synced(manifest.clone());
        tracing::debug!(
            tenant = %scope.tenant,
            account = %scope.account,
            version,
            "search index published"
        );
        if let Err(e) = self.collect_garbage(scope, &manifest).await {
            tracing::warn!(tenant = %scope.tenant, account = %scope.account, error = %e,
                "search index garbage collection failed");
        }
        Ok(manifest)
    }

    async fn put_file(&self, path: &str, local: &Path) -> Result<u64> {
        let data = tokio::fs::read(local).await?;
        let size = data.len() as u64;
        self.blob.put_raw(path, Bytes::from(data)).await?;
        Ok(size)
    }

    /// Удаляет файлы ящика в BlobStore, которых нет в манифесте `keep` и которые записаны раньше
    /// `gc_delay`; не чаще раза в `gc_delay` и не во время перестройки индекса на этом узле.
    async fn collect_garbage(&self, scope: AccountScope, keep: &IndexManifest) -> Result<()> {
        let key = (scope.tenant, scope.account);
        if lock(&self.building).contains(&key) {
            return Ok(());
        }
        {
            let mut last = lock(&self.last_gc);
            if last
                .get(&key)
                .is_some_and(|t| t.elapsed() < self.cfg.gc_delay)
            {
                return Ok(());
            }
            last.insert(key, Instant::now());
        }
        let keep: HashSet<&str> = keep.segments.iter().map(|s| s.path.as_str()).collect();
        let older_than = OffsetDateTime::now_utc()
            - time::Duration::try_from(self.cfg.gc_delay).unwrap_or(time::Duration::ZERO);
        let stale: Vec<String> = self
            .blob
            .list(&prefix(scope), older_than)
            .try_filter_map(|e| {
                let stale = (!keep.contains(e.path.as_str())).then_some(e.path);
                async move { Ok(stale) }
            })
            .try_collect()
            .await?;
        for path in stale {
            self.blob.delete_raw(&path).await?;
        }
        Ok(())
    }

    /// Индекс учётной записи на этом узле по текущему манифесту.
    ///
    /// Открытый индекс, отставший от манифеста, догружается; не открытый загружается из BlobStore в каталог
    /// `manager` (совпадающие по размеру файлы не скачиваются). В манифесте нет сегментов — индекс из
    /// каталога, если он есть. Индекс другой схемы не читается: при `create` каталог очищается, запрашивается
    /// перестройка и возвращается пустой индекс, иначе — `None`. Вызывать под [`IndexManager::lock`].
    pub async fn sync(
        &self,
        manager: &Arc<IndexManager>,
        scope: AccountScope,
        create: bool,
    ) -> Result<Option<Arc<MailIndex>>> {
        let want = self.manifest(scope).await?;
        self.sync_to(manager, scope, want, create).await
    }

    pub(crate) async fn sync_to(
        &self,
        manager: &Arc<IndexManager>,
        scope: AccountScope,
        mut want: IndexManifest,
        create: bool,
    ) -> Result<Option<Arc<MailIndex>>> {
        let open = manager.cached(scope);
        if want.segments.is_empty() {
            let index = match open {
                Some(index) => Some(index),
                None => {
                    let m = manager.clone();
                    blocking(move || m.open(scope)).await?
                }
            };
            if let Some(index) = &index {
                index.set_synced(want);
            }
            return Ok(index);
        }
        if want.schema != SCHEMA_VERSION {
            if !create {
                return Ok(None);
            }
            let m = manager.clone();
            blocking(move || m.evict(scope)).await?;
            if want.rebuild.is_none() {
                let request = RebuildRequest {
                    requested_at: OffsetDateTime::now_utc(),
                    // Индекс прежней схемы этой версией не читается.
                    reason: RebuildReason::Corrupt,
                };
                want.version = self.manifests.request_rebuild(scope, request).await?;
                want.rebuild = Some(request);
            }
            tracing::warn!(tenant = %scope.tenant, account = %scope.account, schema = want.schema,
                "search index has another schema: rebuild requested");
            let m = manager.clone();
            let index = blocking(move || m.open_or_create(scope)).await?;
            index.set_synced(want);
            return Ok(Some(index));
        }
        if let Some(index) = &open
            && index.synced().segments == want.segments
        {
            index.set_synced(want);
            return Ok(open);
        }
        let dir = manager.path(scope);
        self.download(&dir, &want).await?;
        let keep: HashSet<String> = want
            .segments
            .iter()
            .map(|s| local_name(&s.path).to_owned())
            .collect();
        let index = match open {
            Some(index) => {
                let i = index.clone();
                blocking(move || i.reload()).await?;
                index
            }
            None => {
                let m = manager.clone();
                blocking(move || m.open(scope))
                    .await?
                    .ok_or_else(|| Error::internal("search index was not downloaded"))?
            }
        };
        let i = index.clone();
        blocking(move || i.prune(&keep)).await?;
        index.set_synced(want);
        Ok(Some(index))
    }

    /// Скачивает файлы манифеста в каталог; `meta.json` — последним, атомарно.
    async fn download(&self, dir: &Path, want: &IndexManifest) -> Result<()> {
        tokio::fs::create_dir_all(dir).await?;
        let mut meta = None;
        for s in &want.segments {
            let name = local_name(&s.path);
            if name == META_FILE {
                meta = Some(s);
                continue;
            }
            let local = dir.join(name);
            if tokio::fs::metadata(&local)
                .await
                .is_ok_and(|m| m.len() == s.size)
            {
                continue;
            }
            self.fetch(&s.path, dir, name).await?;
        }
        let meta =
            meta.ok_or_else(|| Error::internal("search index manifest without meta.json"))?;
        self.fetch(&meta.path, dir, META_FILE).await
    }

    /// Скачивает объект во временный файл и переименовывает его в `name`.
    async fn fetch(&self, path: &str, dir: &Path, name: &str) -> Result<()> {
        let tmp = dir.join(format!(".{name}.part"));
        let mut stream = self.blob.get_raw(path, None).await?;
        let mut file = tokio::fs::File::create(&tmp).await?;
        while let Some(chunk) = stream.try_next().await? {
            file.write_all(&chunk).await?;
        }
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&tmp, dir.join(name)).await?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::index::tests::{manager, scope};
    use crate::index::{IndexDoc, IndexOp};
    use crate::{QueryConfig, Search};
    use tp_contracts::search::{SearchIndex, SearchSort, TextField, TextQuery};
    use tp_contracts::{MailboxId, MessageId};
    use tp_testkit::{BlobOp, ManualClock, MemBlobStore, MemIndexManifests, SystemClock};

    /// Узел: свой каталог индексов и исполнитель запросов.
    pub(crate) struct Node {
        _dir: tempfile::TempDir,
        pub(crate) indexes: Arc<IndexManager>,
        pub(crate) search: Search,
    }

    pub(crate) fn node() -> Node {
        let dir = tempfile::tempdir().expect("tempdir");
        let indexes = Arc::new(manager(&dir));
        let search = Search::new(indexes.clone(), QueryConfig::default());
        Node {
            _dir: dir,
            indexes,
            search,
        }
    }

    /// Хранилище в памяти; объекты записаны «давно» (часы BLOB — 2026-01-01), сборка мусора — сразу.
    pub(crate) fn store() -> (Arc<MemBlobStore>, Arc<MemIndexManifests>, Arc<IndexStore>) {
        let blob = Arc::new(MemBlobStore::new(ManualClock::default().shared()));
        let manifests = Arc::new(MemIndexManifests::default());
        let store = Arc::new(IndexStore::new(
            blob.clone(),
            manifests.clone(),
            PersistConfig {
                gc_delay: Duration::ZERO,
            },
        ));
        (blob, manifests, store)
    }

    pub(crate) fn mail(subject: &str) -> IndexDoc {
        IndexDoc {
            message: MessageId::new(),
            mailbox: MailboxId::default(),
            received_at: OffsetDateTime::UNIX_EPOCH,
            subject: subject.to_owned(),
            from: String::new(),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            body: String::new(),
            attachments: String::new(),
            headers: Vec::new(),
        }
    }

    pub(crate) fn upserts(docs: &[IndexDoc]) -> Vec<IndexOp> {
        docs.iter()
            .map(|d| IndexOp::Upsert(Box::new(d.clone())))
            .collect()
    }

    pub(crate) async fn apply(index: &Arc<MailIndex>, ops: Vec<IndexOp>) {
        let index = index.clone();
        blocking(move || index.apply(&ops)).await.expect("apply");
    }

    pub(crate) async fn count(search: &Search, scope: AccountScope, text: &str) -> usize {
        search
            .query(
                scope,
                &[],
                &TextQuery::term(TextField::Subject, text),
                SearchSort::NewestFirst,
                1000,
            )
            .await
            .expect("query")
            .len()
    }

    fn local_files(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .expect("read_dir")
            .filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| crate::index::is_segment_file(&e.file_name().to_string_lossy()))
            })
            .count()
    }

    #[tokio::test]
    async fn index_restored_on_another_node() {
        let (blob, manifests, store) = store();
        let scope = scope();
        let a = node();
        let index = a.indexes.open_or_create(scope).expect("create");
        let reports: Vec<_> = (0..20).map(|i| mail(&format!("Отчёт {i}"))).collect();
        apply(&index, upserts(&reports)).await;
        let m1 = store.publish(scope, &index).await.expect("publish");
        assert_eq!((m1.version, m1.schema, m1.docs()), (1, SCHEMA_VERSION, 20));
        assert_eq!(manifests.get_manifest(scope).await.expect("get"), m1);
        assert!(
            m1.segments
                .iter()
                .all(|s| s.path.starts_with(&prefix(scope)))
        );
        // Без изменений — без выгрузки.
        let puts = blob.calls(BlobOp::PutRaw);
        assert_eq!(store.publish(scope, &index).await.expect("same"), m1);
        assert_eq!(blob.calls(BlobOp::PutRaw), puts);

        // Второй коммит выгружает только новые файлы; выпавшие из манифеста удаляются.
        let mut ops = upserts(&[mail("Счёт на оплату"), mail("Отчёт за год")]);
        ops.push(IndexOp::Delete(reports[0].message));
        apply(&index, ops).await;
        let m2 = store.publish(scope, &index).await.expect("publish 2");
        assert_eq!((m2.version, m2.docs()), (2, 21));
        let uploaded = blob.calls(BlobOp::PutRaw) - puts;
        let reused = m2
            .segments
            .iter()
            .filter(|s| m1.segments.contains(s))
            .count();
        assert!(reused > 0, "unchanged segment files are not uploaded again");
        assert_eq!(uploaded as usize, m2.segments.len() - reused);
        let mut stored = blob.paths();
        stored.retain(|p| p.starts_with(&prefix(scope)));
        let mut listed: Vec<_> = m2.segments.iter().map(|s| s.path.clone()).collect();
        stored.sort();
        listed.sort();
        assert_eq!(stored, listed, "only manifest files remain in BlobStore");

        // «Другой узел»: пустой каталог, тот же BlobStore и манифесты.
        let b = node();
        {
            let _gate = b.indexes.lock(scope).await;
            let restored = store
                .sync(&b.indexes, scope, false)
                .await
                .expect("restore")
                .expect("index");
            assert_eq!(restored.docs(), 21);
            assert_eq!(restored.synced(), m2);
        }
        assert_eq!(count(&b.search, scope, "отчет").await, 20);
        assert_eq!(count(&b.search, scope, "счет").await, 1);

        // Узел A продолжает запись — узел B догружает изменения, лишние файлы удаляются.
        apply(&index, upserts(&[mail("Счёт повторный")])).await;
        let m3 = store.publish(scope, &index).await.expect("publish 3");
        {
            let _gate = b.indexes.lock(scope).await;
            let refreshed = store
                .sync(&b.indexes, scope, false)
                .await
                .expect("refresh")
                .expect("index");
            assert_eq!(refreshed.synced(), m3);
            assert_eq!(local_files(refreshed.path()), m3.segments.len() - 1);
        }
        assert_eq!(count(&b.search, scope, "счет").await, 2);

        // Ящик без индекса — `None`.
        let empty = crate::index::tests::scope();
        let _gate = b.indexes.lock(empty).await;
        assert!(
            store
                .sync(&b.indexes, empty, false)
                .await
                .expect("none")
                .is_none()
        );
    }

    #[tokio::test]
    async fn manifest_cas() {
        let (_, manifests, store) = store();
        let scope = scope();
        let (a, b) = (node(), node());
        let ia = a.indexes.open_or_create(scope).expect("a");
        let ib = b.indexes.open_or_create(scope).expect("b");
        apply(&ia, upserts(&[mail("Узел А")])).await;
        apply(&ib, upserts(&[mail("Узел Б")])).await;
        store.publish(scope, &ia).await.expect("a wins");
        // Узел B не видел версию 1: набор сегментов другой — конфликт.
        assert!(matches!(
            store.publish(scope, &ib).await,
            Err(Error::Conflict)
        ));

        // Запрос перестройки меняет только версию: запись повторяется и сохраняет запрос.
        let request = RebuildRequest {
            requested_at: OffsetDateTime::UNIX_EPOCH,
            reason: RebuildReason::Admin,
        };
        let v = manifests
            .request_rebuild(scope, request)
            .await
            .expect("request");
        apply(&ia, upserts(&[mail("После запроса")])).await;
        let m = store.publish(scope, &ia).await.expect("retry");
        assert_eq!(m.version, v + 1);
        assert_eq!(m.rebuild, Some(request));
        assert_eq!(manifests.get_manifest(scope).await.expect("get"), m);
    }

    #[tokio::test]
    async fn garbage_waits_for_delay() {
        let blob = Arc::new(MemBlobStore::new(Arc::new(SystemClock)));
        let manifests = Arc::new(MemIndexManifests::default());
        let store = IndexStore::new(blob.clone(), manifests, PersistConfig::default());
        let scope = scope();
        let a = node();
        let index = a.indexes.open_or_create(scope).expect("create");
        apply(&index, upserts(&[mail("Первое")])).await;
        let m1 = store.publish(scope, &index).await.expect("1");
        apply(&index, upserts(&[mail("Второе")])).await;
        store.publish(scope, &index).await.expect("2");
        // Снимок meta.json первой версии ещё нужен узлам, загружающим её.
        let paths = blob.paths();
        assert!(m1.segments.iter().all(|s| paths.contains(&s.path)));
    }

    #[test]
    fn config_defaults_and_unknown() {
        assert_eq!(
            crate::tests::from_pairs::<PersistConfig>(&[]).expect("defaults"),
            PersistConfig::default()
        );
        let cfg = crate::tests::from_pairs::<PersistConfig>(&[("gc_delay", "1m")]).expect("1m");
        assert_eq!(cfg.gc_delay, Duration::from_secs(60));
        assert!(crate::tests::from_pairs::<PersistConfig>(&[("delay", "1m")]).is_err());
    }
}
