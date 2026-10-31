//! Локальный BlobStore: POSIX-директория, в том числе общая для узлов (SRS 3.2).
//!
//! Объект `path` лежит в файле `root/path`; путь ключа уже разложен по префиксу хэша
//! (`{tenant}/{hash[0..2]}/{hash}/{gen}`, [`BlobKey::path`]). Запись — во временный файл
//! `root/.tmp/…`, fsync, rename и fsync каталога: объект виден только целиком, обрыв записи
//! оставляет лишь временный файл. Брошенные временные файлы удаляются при открытии хранилища.

use std::hash::{BuildHasher, RandomState};
use std::io::SeekFrom;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use futures::{StreamExt, TryStreamExt, stream};
use schemars::JsonSchema;
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::fs;
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tp_contracts::blob::{BlobStore, ByteStream, ObjectEntry, ObjectStream};
use tp_contracts::config::duration;
use tp_contracts::{BlobKey, Error, Result};

use crate::{check_raw_path, clamp_range, config};

/// Каталог временных файлов внутри корня; в листинг не попадает.
const TMP_DIR: &str = ".tmp";

/// Конфигурация локального хранилища (секция config.yaml; поля необязательны, неизвестное — ошибка).
///
/// ```yaml
/// root: /var/lib/thepost/blob
/// fsync: true
/// read_chunk_bytes: 262144
/// stale_tmp: 24h
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LocalConfig {
    /// Корневой каталог объектов.
    pub root: PathBuf,
    /// fsync файла и каталога при записи. Выключать только для кэша, не для источника истины.
    pub fsync: bool,
    /// Размер блока потокового чтения, байт.
    pub read_chunk_bytes: usize,
    /// Возраст временного файла, после которого он считается брошенным и удаляется
    /// при открытии хранилища. Больше максимальной длительности записи объекта.
    /// В config.yaml — `stale_tmp: 24h` (целые секунды).
    #[serde(rename = "stale_tmp", deserialize_with = "config::secs")]
    #[schemars(with = "duration::HumanDuration")]
    pub stale_tmp_secs: u64,
}

impl Default for LocalConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("/var/lib/thepost/blob"),
            fsync: true,
            read_chunk_bytes: 256 << 10,
            stale_tmp_secs: 24 * 3600,
        }
    }
}

/// BlobStore на локальном каталоге.
#[derive(Debug)]
pub struct LocalStore {
    cfg: LocalConfig,
    /// Случайная часть имён временных файлов: каталог может быть общим для нескольких узлов.
    tmp_seed: u64,
    tmp_seq: AtomicU64,
}

impl LocalStore {
    /// Открывает хранилище: создаёт каталоги и удаляет брошенные временные файлы.
    pub async fn open(cfg: LocalConfig) -> Result<Self> {
        fs::create_dir_all(cfg.root.join(TMP_DIR)).await?;
        let store = Self {
            tmp_seed: RandomState::new().hash_one(std::process::id()),
            tmp_seq: AtomicU64::new(0),
            cfg,
        };
        store.remove_stale_tmp().await?;
        Ok(store)
    }

    /// Корневой каталог объектов.
    pub fn root(&self) -> &Path {
        &self.cfg.root
    }

    async fn remove_stale_tmp(&self) -> Result<()> {
        let stale = SystemTime::now() - Duration::from_secs(self.cfg.stale_tmp_secs);
        let mut dir = fs::read_dir(self.cfg.root.join(TMP_DIR)).await?;
        while let Some(entry) = dir.next_entry().await? {
            let meta = entry.metadata().await?;
            if meta.is_file() && meta.modified()? <= stale {
                remove_if_exists(&entry.path()).await?;
            }
        }
        Ok(())
    }

    fn tmp_path(&self) -> PathBuf {
        let seq = self.tmp_seq.fetch_add(1, Ordering::Relaxed);
        self.cfg
            .root
            .join(TMP_DIR)
            .join(format!("{:016x}-{seq}", self.tmp_seed))
    }

    /// Атомарная запись: временный файл → fsync → rename. Повторная запись заменяет объект.
    pub(crate) async fn write(&self, path: &str, data: ByteStream) -> Result<()> {
        let tmp = self.tmp_path();
        let mut res = self.write_tmp(&tmp, data).await;
        if res.is_ok() {
            res = self.publish(&tmp, path).await;
        }
        if res.is_err()
            && let Err(e) = remove_if_exists(&tmp).await
        {
            tracing::warn!(tmp = %tmp.display(), error = %e, "blob: temp file not removed");
        }
        res
    }

    async fn write_tmp(&self, tmp: &Path, mut data: ByteStream) -> Result<()> {
        let mut file = fs::File::create(tmp).await?;
        while let Some(chunk) = data.try_next().await? {
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        if self.cfg.fsync {
            file.sync_all().await?;
        }
        Ok(())
    }

    async fn publish(&self, tmp: &Path, path: &str) -> Result<()> {
        let dst = self.cfg.root.join(path);
        let dir = dst
            .parent()
            .ok_or_else(|| Error::invalid(format!("bad object path: {path}")))?;
        fs::create_dir_all(dir).await?;
        fs::rename(tmp, &dst).await?;
        if self.cfg.fsync {
            sync_dir(dir).await?;
        }
        Ok(())
    }

    /// Чтение объекта: файл открывается сразу (отсутствие — `NotFound` из вызова), байты — потоком.
    pub(crate) async fn read(&self, path: &str, range: Option<Range<u64>>) -> Result<ByteStream> {
        let mut file = fs::File::open(self.cfg.root.join(path)).await?;
        let range = clamp_range(range, file.metadata().await?.len())?;
        if range.start > 0 {
            file.seek(SeekFrom::Start(range.start)).await?;
        }
        let chunk = self.cfg.read_chunk_bytes.max(1);
        let reader = file.take(range.end - range.start);
        Ok(stream::try_unfold(reader, move |mut reader| async move {
            let mut buf = BytesMut::with_capacity(chunk);
            let n = reader.read_buf(&mut buf).await?;
            Ok::<_, Error>((n > 0).then(|| (buf.freeze(), reader)))
        })
        .boxed())
    }

    async fn size(&self, path: &str) -> Result<Option<u64>> {
        match fs::metadata(self.cfg.root.join(path)).await {
            Ok(meta) => Ok(Some(meta.len())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) async fn remove(&self, path: &str) -> Result<()> {
        remove_if_exists(&self.cfg.root.join(path)).await
    }

    /// Листинг файлов с путём на `prefix`, изменённых раньше `older_than`.
    /// Обходятся только каталоги, совместимые с префиксом.
    fn walk(&self, prefix: &str, older_than: SystemTime) -> ObjectStream {
        let walk = Walk {
            root: self.cfg.root.clone(),
            prefix: prefix.to_owned(),
            older_than,
            dirs: vec![String::new()],
            current: None,
        };
        stream::try_unfold(walk, |mut walk| async move {
            Ok(walk.next().await?.map(|entry| (entry, walk)))
        })
        .boxed()
    }
}

/// Состояние обхода каталога для [`LocalStore::walk`].
struct Walk {
    root: PathBuf,
    prefix: String,
    older_than: SystemTime,
    /// Относительные пути каталогов, ожидающих обхода.
    dirs: Vec<String>,
    current: Option<(fs::ReadDir, String)>,
}

impl Walk {
    async fn next(&mut self) -> Result<Option<ObjectEntry>> {
        loop {
            let Some((dir, rel)) = &mut self.current else {
                let Some(rel) = self.dirs.pop() else {
                    return Ok(None);
                };
                let dir = fs::read_dir(self.root.join(&rel)).await?;
                self.current = Some((dir, rel));
                continue;
            };
            let Some(entry) = dir.next_entry().await? else {
                self.current = None;
                continue;
            };
            // Имена не в UTF-8 и служебные каталоги объектами не являются.
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let path = if rel.is_empty() {
                name
            } else {
                format!("{rel}/{name}")
            };
            let meta = entry.metadata().await?;
            if meta.is_dir() {
                let dir_prefix = format!("{path}/");
                if dir_prefix.starts_with(&self.prefix) || self.prefix.starts_with(&dir_prefix) {
                    self.dirs.push(path);
                }
            } else if meta.is_file() && path.starts_with(&self.prefix) {
                let modified = meta.modified()?;
                if modified < self.older_than {
                    return Ok(Some(ObjectEntry {
                        path,
                        size: meta.len(),
                        modified: modified.into(),
                    }));
                }
            }
        }
    }
}

async fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path).await {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

/// fsync каталога, чтобы rename пережил сбой питания. На Windows каталоги так не синхронизируются.
async fn sync_dir(dir: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::File::open(dir).await?.sync_all().await?;
    #[cfg(not(unix))]
    let _ = dir;
    Ok(())
}

fn once(data: Bytes) -> ByteStream {
    stream::once(async move { Ok(data) }).boxed()
}

#[async_trait]
impl BlobStore for LocalStore {
    async fn put(&self, key: &BlobKey, data: Bytes) -> Result<()> {
        self.write(&key.path(), once(data)).await
    }

    async fn put_stream(
        &self,
        key: &BlobKey,
        data: ByteStream,
        _size_hint: Option<u64>,
    ) -> Result<()> {
        self.write(&key.path(), data).await
    }

    async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<ByteStream> {
        self.read(&key.path(), range).await
    }

    async fn head(&self, key: &BlobKey) -> Result<Option<u64>> {
        self.size(&key.path()).await
    }

    async fn delete(&self, key: &BlobKey) -> Result<()> {
        self.remove(&key.path()).await
    }

    fn list(&self, prefix: &str, older_than: OffsetDateTime) -> ObjectStream {
        self.walk(prefix, older_than.into())
    }

    async fn put_raw(&self, path: &str, data: Bytes) -> Result<()> {
        check_raw_path(path)?;
        self.write(path, once(data)).await
    }

    async fn get_raw(&self, path: &str, range: Option<Range<u64>>) -> Result<ByteStream> {
        check_raw_path(path)?;
        self.read(path, range).await
    }

    async fn delete_raw(&self, path: &str) -> Result<()> {
        check_raw_path(path)?;
        self.remove(path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_contracts::{BlobHash, Gen, TenantId};

    fn key(n: u8) -> BlobKey {
        BlobKey {
            tenant: TenantId::DEFAULT,
            hash: BlobHash([n; 32]),
            generation: Gen(1),
        }
    }

    async fn open(dir: &Path) -> LocalStore {
        LocalStore::open(LocalConfig {
            root: dir.to_path_buf(),
            read_chunk_bytes: 3,
            ..LocalConfig::default()
        })
        .await
        .expect("open")
    }

    async fn read_all(stream: ByteStream) -> Vec<u8> {
        let chunks: Vec<Bytes> = stream.try_collect().await.expect("read");
        chunks.concat()
    }

    async fn tmp_files(dir: &Path) -> usize {
        let mut rd = fs::read_dir(dir.join(TMP_DIR)).await.expect("tmp dir");
        let mut n = 0;
        while rd.next_entry().await.expect("entry").is_some() {
            n += 1;
        }
        n
    }

    fn future() -> OffsetDateTime {
        OffsetDateTime::now_utc() + time::Duration::hours(1)
    }

    #[tokio::test]
    async fn put_is_idempotent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(dir.path()).await;
        let k = key(1);
        store
            .put(&k, Bytes::from_static(b"hello"))
            .await
            .expect("put");
        store
            .put(&k, Bytes::from_static(b"hello"))
            .await
            .expect("put again");
        assert_eq!(store.head(&k).await, Ok(Some(5)));
        assert_eq!(
            read_all(store.get(&k, None).await.expect("get")).await,
            b"hello"
        );
        assert!(dir.path().join(k.path()).is_file());
        assert_eq!(tmp_files(dir.path()).await, 0);
    }

    #[tokio::test]
    async fn range_read() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(dir.path()).await;
        let k = key(2);
        store
            .put(&k, Bytes::from_static(b"0123456789"))
            .await
            .expect("put");
        let get = |r| store.get(&k, Some(r));
        assert_eq!(read_all(get(2..7).await.expect("get")).await, b"23456");
        assert_eq!(read_all(get(8..100).await.expect("get")).await, b"89");
        assert_eq!(read_all(get(10..10).await.expect("get")).await, b"");
        assert!(matches!(get(11..12).await, Err(Error::Invalid(_))));
    }

    #[tokio::test]
    async fn missing_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(dir.path()).await;
        let k = key(3);
        assert!(matches!(store.get(&k, None).await, Err(Error::NotFound)));
        assert_eq!(store.head(&k).await, Ok(None));
        assert_eq!(store.delete(&k).await, Ok(()));
    }

    #[tokio::test]
    async fn delete_removes_object() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(dir.path()).await;
        let k = key(4);
        store.put(&k, Bytes::from_static(b"x")).await.expect("put");
        store.delete(&k).await.expect("delete");
        assert_eq!(store.head(&k).await, Ok(None));
    }

    #[tokio::test]
    async fn interrupted_write_is_invisible() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(dir.path()).await;
        let k = key(5);
        let data = stream::iter([Ok(Bytes::from_static(b"part")), Err(Error::UNAVAILABLE)]).boxed();
        assert_eq!(
            store.put_stream(&k, data, Some(100)).await,
            Err(Error::UNAVAILABLE)
        );
        assert_eq!(store.head(&k).await, Ok(None));
        let listed: Vec<ObjectEntry> = store.list("", future()).try_collect().await.expect("list");
        assert!(listed.is_empty(), "{listed:?}");
        assert_eq!(tmp_files(dir.path()).await, 0);
    }

    #[tokio::test]
    async fn list_by_prefix_and_mtime() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(dir.path()).await;
        let k = key(6);
        store
            .put(&k, Bytes::from_static(b"abc"))
            .await
            .expect("put");
        store
            .put_raw("idx/t/a/seg1", Bytes::from_static(b"s"))
            .await
            .expect("put_raw");

        let list = |prefix: &str, older_than| {
            store
                .list(prefix, older_than)
                .map_ok(|e| e.path)
                .try_collect::<Vec<_>>()
        };
        let tenant = BlobKey::tenant_prefix(TenantId::DEFAULT);
        assert_eq!(list(&tenant, future()).await, Ok(vec![k.path()]));
        assert_eq!(
            list("idx/t/a/se", future()).await,
            Ok(vec!["idx/t/a/seg1".into()])
        );
        assert_eq!(list("idx/t/b", future()).await, Ok(vec![]));
        let past = OffsetDateTime::now_utc() - time::Duration::hours(1);
        assert_eq!(list("", past).await, Ok(vec![]));

        let all: Vec<ObjectEntry> = store.list("", future()).try_collect().await.expect("list");
        assert_eq!(all.len(), 2);
        let entry = all.iter().find(|e| e.key() == Some(k)).expect("key listed");
        assert_eq!(entry.size, 3);
    }

    #[tokio::test]
    async fn raw_paths_are_checked() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = open(dir.path()).await;
        for bad in ["../x", "/abs", ".tmp/x", "a//b"] {
            assert!(
                matches!(
                    store.put_raw(bad, Bytes::new()).await,
                    Err(Error::Invalid(_))
                ),
                "{bad}"
            );
        }
        store
            .put_raw("idx/x", Bytes::from_static(b"r"))
            .await
            .expect("put_raw");
        assert_eq!(
            read_all(store.get_raw("idx/x", None).await.expect("get")).await,
            b"r"
        );
        store.delete_raw("idx/x").await.expect("delete_raw");
        assert!(matches!(
            store.get_raw("idx/x", None).await,
            Err(Error::NotFound)
        ));
    }

    #[tokio::test]
    async fn stale_tmp_removed_on_open() {
        let dir = tempfile::tempdir().expect("tempdir");
        drop(open(dir.path()).await);
        let tmp = dir.path().join(TMP_DIR).join("left-over");
        fs::write(&tmp, b"partial").await.expect("write");
        let fresh = open(dir.path()).await;
        assert!(tmp.exists(), "fresh temp file must survive");
        drop(fresh);
        LocalStore::open(LocalConfig {
            root: dir.path().to_path_buf(),
            stale_tmp_secs: 0,
            ..LocalConfig::default()
        })
        .await
        .expect("open");
        assert!(!tmp.exists());
    }
}
