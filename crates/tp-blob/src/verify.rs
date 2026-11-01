//! `thepost verify` (SRS 3.5): сверка хранилища и учёта объектов в обе стороны.
//!
//! - «Сироты»: объекты хранилища старше периода ожидания без записи учёта того же поколения.
//! - Повреждённые записи: запись `live`, у которой объекта нет или размер не совпадает.
//!
//! Отчёт — JSON ([`VerifyReport::to_json`]). В режиме исправления «сироты» удаляются, повреждённые
//! записи помечаются через [`BlobInventory::mark_damaged`] (письма со ссылкой на них — повреждённые).
//! Перечисление записей и пометка не входят в контракт [`BlobIndex`] — их даёт [`BlobInventory`].
//!
//! Режим исправления удаляет «сирот», как поиск «сирот», и не должен идти одновременно с ним
//! на другом узле: второй устаревший листинг может задеть объект, загруженный после удаления первым.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use schemars::JsonSchema;
use serde::Serialize;
use tp_contracts::blob::{BlobIndex, BlobStore, Compression};
use tp_contracts::{BlobKey, Error, Result};

use crate::compress::stored_content_size;
use crate::orphans::orphan_key;
use crate::worker::{Now, Pacer, before};

/// Запись учёта `live` для сверки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexedBlob {
    /// Ключ объекта.
    pub key: BlobKey,
    /// Размер по учёту, байт.
    pub size: u64,
}

/// Перечисление записей учёта и пометка повреждённых — для verify.
#[async_trait]
pub trait BlobInventory: Send + Sync + 'static {
    /// Записи `live` по всем организациям. Порядок не гарантирован.
    fn live(&self) -> BoxStream<'static, Result<IndexedBlob>>;

    /// Пометить запись повреждённой: объект отсутствует или испорчен.
    async fn mark_damaged(&self, key: &BlobKey) -> Result<()>;
}

/// Конфигурация verify.
#[derive(Debug, Clone, PartialEq, Eq, JsonSchema)]
pub struct VerifyConfig {
    /// Период ожидания для «сирот», с (как в поиске «сирот»).
    pub grace_secs: u64,
    /// Префикс путей листинга хранилища.
    pub prefix: String,
    /// Предел обращений к хранилищу в секунду; 0 — без ограничения.
    pub max_ops_per_sec: u32,
    /// Предел числа расхождений, перечисляемых в отчёте поимённо (счётчики — полные).
    pub max_listed: usize,
}

impl Default for VerifyConfig {
    fn default() -> Self {
        Self {
            grace_secs: 24 * 3600,
            prefix: String::new(),
            max_ops_per_sec: 0,
            max_listed: 1000,
        }
    }
}

/// Причина повреждения записи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Damage {
    /// Объекта нет в хранилище.
    Missing,
    /// Размер объекта не совпадает с учётом.
    SizeMismatch,
}

/// Повреждённая запись в отчёте.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DamagedBlob {
    /// Путь объекта (`{tenant}/{hash[0..2]}/{hash}/{gen}`).
    pub path: String,
    /// Причина.
    pub damage: Damage,
    /// Размер по учёту.
    pub expected_size: u64,
    /// Размер в хранилище, если объект есть.
    pub actual_size: Option<u64>,
}

/// Отчёт verify.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct VerifyReport {
    /// Начало сверки, Unix-время UTC, с.
    pub started_at: i64,
    /// Конец сверки, Unix-время UTC, с.
    pub finished_at: i64,
    /// Режим исправления.
    pub fix: bool,
    /// Объектов в листинге хранилища (старше периода ожидания).
    pub objects_listed: u64,
    /// Записей `live` проверено.
    pub records_checked: u64,
    /// Найдено «сирот».
    pub orphans: u64,
    /// Удалено «сирот» (режим исправления).
    pub orphans_deleted: u64,
    /// Найдено повреждённых записей.
    pub damaged: u64,
    /// Помечено повреждёнными (режим исправления).
    pub damaged_marked: u64,
    /// Ошибок отдельных операций (сверка продолжена).
    pub errors: u64,
    /// Пути «сирот» (не больше `max_listed`).
    pub orphan_paths: Vec<String>,
    /// Повреждённые записи (не больше `max_listed`).
    pub damaged_blobs: Vec<DamagedBlob>,
    /// Списки обрезаны по `max_listed`.
    pub truncated: bool,
}

impl VerifyReport {
    /// Отчёт в JSON.
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|e| Error::internal(e.to_string()))
    }

    /// Расхождений нет.
    pub fn is_clean(&self) -> bool {
        self.orphans == 0 && self.damaged == 0 && self.errors == 0
    }
}

/// Сверка хранилища и учёта.
pub struct Verifier {
    index: Arc<dyn BlobIndex>,
    inventory: Arc<dyn BlobInventory>,
    store: Arc<dyn BlobStore>,
    cfg: VerifyConfig,
    now: Now,
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verifier")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

impl Verifier {
    /// Сверка учёта `index`/`inventory` с хранилищем `store`.
    pub fn new(
        index: Arc<dyn BlobIndex>,
        inventory: Arc<dyn BlobInventory>,
        store: Arc<dyn BlobStore>,
        cfg: VerifyConfig,
        now: Now,
    ) -> Self {
        Self {
            index,
            inventory,
            store,
            cfg,
            now,
        }
    }

    /// Сверка; `fix` — удалить «сирот» и пометить повреждённые записи.
    /// Ошибка листинга прерывает сверку, ошибки отдельных объектов учитываются в отчёте.
    pub async fn run(&self, fix: bool) -> Result<VerifyReport> {
        let started = (self.now)();
        let mut report = VerifyReport {
            started_at: started.unix_timestamp(),
            fix,
            ..VerifyReport::default()
        };
        let mut pacer = Pacer::new(self.cfg.max_ops_per_sec);
        self.orphans(
            before(started, Duration::from_secs(self.cfg.grace_secs)),
            fix,
            &mut pacer,
            &mut report,
        )
        .await?;
        self.records(fix, &mut pacer, &mut report).await?;
        report.finished_at = (self.now)().unix_timestamp();
        tracing::info!(
            fix,
            orphans = report.orphans,
            damaged = report.damaged,
            errors = report.errors,
            "verify BLOB"
        );
        Ok(report)
    }

    async fn orphans(
        &self,
        older_than: time::OffsetDateTime,
        fix: bool,
        pacer: &mut Pacer,
        report: &mut VerifyReport,
    ) -> Result<()> {
        let mut list = self.store.list(&self.cfg.prefix, older_than);
        while let Some(entry) = list.next().await {
            let entry = entry?;
            report.objects_listed += 1;
            let key = match orphan_key(self.index.as_ref(), &entry).await {
                Ok(Some(key)) => key,
                Ok(None) => continue,
                Err(e) => {
                    report.errors += 1;
                    tracing::warn!(path = %entry.path, error = %e, "verify: проверка учёта не удалась");
                    continue;
                }
            };
            report.orphans += 1;
            self.list(&mut report.orphan_paths, &mut report.truncated, entry.path);
            if fix {
                pacer.wait().await;
                match self.store.delete(&key).await {
                    Ok(()) => report.orphans_deleted += 1,
                    Err(e) => {
                        report.errors += 1;
                        tracing::warn!(%key, error = %e, "verify: удаление «сироты» не удалось");
                    }
                }
            }
        }
        Ok(())
    }

    /// Размер объекта для сверки с учётом. Сжатый объект (признак [`Compression::Zstd`]) сверяется
    /// по размеру исходного содержимого из заголовка кадра zstd: хранилище знает только сжатый размер.
    async fn stored_size(&self, rec: &IndexedBlob) -> Result<Option<u64>> {
        let size = self.store.head(&rec.key).await?;
        if size.is_none_or(|s| s == rec.size)
            || self.index.compression(&rec.key).await? != Some(Compression::Zstd)
        {
            return Ok(size);
        }
        match stored_content_size(self.store.as_ref(), &rec.key).await {
            // Битый заголовок кадра — расхождение размера (сжатый размер в отчёте).
            Err(Error::Internal(_)) => Ok(size),
            other => other,
        }
    }

    async fn records(&self, fix: bool, pacer: &mut Pacer, report: &mut VerifyReport) -> Result<()> {
        let mut live = self.inventory.live();
        while let Some(rec) = live.next().await {
            let rec = rec?;
            report.records_checked += 1;
            pacer.wait().await;
            let actual = match self.stored_size(&rec).await {
                Ok(size) => size,
                Err(e) => {
                    report.errors += 1;
                    tracing::warn!(key = %rec.key, error = %e, "verify: проверка объекта не удалась");
                    continue;
                }
            };
            let damage = match actual {
                None => Damage::Missing,
                Some(size) if size != rec.size => Damage::SizeMismatch,
                Some(_) => continue,
            };
            report.damaged += 1;
            let item = DamagedBlob {
                path: rec.key.path(),
                damage,
                expected_size: rec.size,
                actual_size: actual,
            };
            self.list(&mut report.damaged_blobs, &mut report.truncated, item);
            if fix {
                match self.inventory.mark_damaged(&rec.key).await {
                    Ok(()) => report.damaged_marked += 1,
                    Err(e) => {
                        report.errors += 1;
                        tracing::warn!(key = %rec.key, error = %e, "verify: пометка повреждённой записи не удалась");
                    }
                }
            }
        }
        Ok(())
    }

    fn list<T>(&self, items: &mut Vec<T>, truncated: &mut bool, item: T) {
        if items.len() < self.cfg.max_listed {
            items.push(item);
        } else {
            *truncated = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Mutex;
    use std::time::Duration;

    use bytes::Bytes;
    use futures::stream;
    use time::OffsetDateTime;
    use tp_contracts::{Gen, TenantId};
    use tp_testkit::blob::{BlobState, MemBlobIndex, MemBlobStore};
    use tp_testkit::{Clock, ManualClock};

    use crate::writer::{BlobWriter, WriterConfig};

    const T: TenantId = TenantId::DEFAULT;
    const GRACE: u64 = 3600;

    /// Перечисление записей поверх `MemBlobIndex` по известным ключам.
    struct Inventory {
        index: MemBlobIndex,
        keys: Mutex<Vec<BlobKey>>,
        damaged: Mutex<BTreeSet<BlobKey>>,
    }

    #[async_trait]
    impl BlobInventory for Inventory {
        fn live(&self) -> BoxStream<'static, Result<IndexedBlob>> {
            let rows: Vec<_> = self
                .keys
                .lock()
                .expect("lock")
                .iter()
                .filter_map(|k| {
                    let r = self.index.row(k.tenant, &k.hash)?;
                    (r.state == BlobState::Live && r.generation == k.generation).then_some(Ok(
                        IndexedBlob {
                            key: *k,
                            size: r.size,
                        },
                    ))
                })
                .collect();
            stream::iter(rows).boxed()
        }

        async fn mark_damaged(&self, key: &BlobKey) -> Result<()> {
            self.damaged.lock().expect("lock").insert(*key);
            Ok(())
        }
    }

    struct Env {
        clock: ManualClock,
        store: MemBlobStore,
        writer: BlobWriter,
        inventory: Arc<Inventory>,
        verifier: Verifier,
    }

    fn env(max_listed: usize) -> Env {
        let clock = ManualClock::new(OffsetDateTime::UNIX_EPOCH + time::Duration::days(1000));
        let index = MemBlobIndex::new(clock.shared());
        let store = MemBlobStore::new(clock.shared());
        let writer = BlobWriter::new(
            Arc::new(index.clone()),
            Arc::new(store.clone()),
            WriterConfig::default(),
        );
        let inventory = Arc::new(Inventory {
            index: index.clone(),
            keys: Mutex::default(),
            damaged: Mutex::default(),
        });
        let c = clock.clone();
        let verifier = Verifier::new(
            Arc::new(index),
            inventory.clone(),
            Arc::new(store.clone()),
            VerifyConfig {
                grace_secs: GRACE,
                max_listed,
                ..VerifyConfig::default()
            },
            Arc::new(move || c.now()),
        );
        Env {
            clock,
            store,
            writer,
            inventory,
            verifier,
        }
    }

    async fn write(e: &Env, body: &'static [u8]) -> BlobKey {
        let key = e
            .writer
            .write(T, Bytes::from_static(body), 1)
            .await
            .expect("вызов");
        e.inventory.keys.lock().expect("lock").push(key);
        key
    }

    /// Сжимаемое тело: писатель по умолчанию хранит его сжатым.
    static TEXT: [u8; 4096] = [b'a'; 4096];

    #[tokio::test]
    async fn compressed_objects_by_content_size() {
        let e = env(10);
        let ok = write(&e, &TEXT).await;
        let broken = write(&e, &TEXT[1..]).await;
        assert!(e.store.bytes(&ok.path()).expect("объект").len() < TEXT.len());
        e.store.delete(&broken).await.expect("вызов");
        e.store
            .put(&broken, Bytes::from_static(b"not zstd at all"))
            .await
            .expect("вызов");
        e.clock.advance(Duration::from_secs(GRACE + 1));
        let report = e.verifier.run(false).await.expect("вызов");
        assert_eq!(
            (report.records_checked, report.damaged),
            (2, 1),
            "{report:?}"
        );
        assert_eq!(report.damaged_blobs[0].path, broken.path());
        assert_eq!(report.damaged_blobs[0].actual_size, Some(15));
    }

    #[tokio::test]
    async fn clean() {
        let e = env(10);
        write(&e, b"a").await;
        write(&e, b"b").await;
        e.clock.advance(Duration::from_secs(GRACE + 1));
        let report = e.verifier.run(false).await.expect("вызов");
        assert!(report.is_clean(), "{report:?}");
        assert_eq!((report.objects_listed, report.records_checked), (2, 2));
    }

    #[tokio::test]
    async fn both_directions_report_and_fix() {
        let e = env(10);
        let ok = write(&e, b"ok").await;
        let missing = write(&e, b"missing").await;
        let short = write(&e, b"short").await;
        // Расхождения: объект пропал, объект подменён другим размером, «сирота» поколения 3.
        e.store.delete(&missing).await.expect("вызов");
        e.store.delete(&short).await.expect("вызов");
        e.store
            .put(&short, Bytes::from_static(b"sh"))
            .await
            .expect("вызов");
        let orphan = BlobKey {
            generation: Gen(3),
            ..ok
        };
        e.store
            .put(&orphan, Bytes::from_static(b"ok"))
            .await
            .expect("вызов");
        e.clock.advance(Duration::from_secs(GRACE + 1));
        // Молодая «сирота» — незавершённая загрузка, не трогается.
        let young = BlobKey {
            generation: Gen(4),
            ..ok
        };
        e.store
            .put(&young, Bytes::from_static(b"ok"))
            .await
            .expect("вызов");

        let report = e.verifier.run(false).await.expect("вызов");
        assert_eq!((report.orphans, report.damaged, report.errors), (1, 2, 0));
        assert_eq!((report.orphans_deleted, report.damaged_marked), (0, 0));
        assert_eq!(report.orphan_paths, vec![orphan.path()]);
        let mut damaged = report.damaged_blobs.clone();
        damaged.sort_by(|a, b| a.path.cmp(&b.path));
        let mut expected = vec![
            DamagedBlob {
                path: missing.path(),
                damage: Damage::Missing,
                expected_size: 7,
                actual_size: None,
            },
            DamagedBlob {
                path: short.path(),
                damage: Damage::SizeMismatch,
                expected_size: 5,
                actual_size: Some(2),
            },
        ];
        expected.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(damaged, expected);
        assert!(e.store.contains(&orphan));
        assert!(e.inventory.damaged.lock().expect("lock").is_empty());

        let json: serde_json::Value =
            serde_json::from_str(&report.to_json().expect("значение")).expect("значение");
        assert_eq!(json["orphans"], 1);
        assert_eq!(json["fix"], false);
        assert!(
            json["damaged_blobs"]
                .as_array()
                .expect("значение")
                .iter()
                .any(|d| d["damage"] == "missing")
        );

        let fixed = e.verifier.run(true).await.expect("вызов");
        assert_eq!((fixed.orphans_deleted, fixed.damaged_marked), (1, 2));
        assert!(!e.store.contains(&orphan));
        assert!(e.store.contains(&young));
        assert!(e.store.contains(&ok));
        assert_eq!(
            *e.inventory.damaged.lock().expect("lock"),
            BTreeSet::from([missing, short])
        );
    }

    #[tokio::test]
    async fn listing_is_capped() {
        let e = env(1);
        for body in [&b"x"[..], b"y", b"z"] {
            let key = e
                .writer
                .write(T, Bytes::from_static(body), 1)
                .await
                .expect("вызов");
            e.store.delete(&key).await.expect("вызов");
            e.inventory.keys.lock().expect("lock").push(key);
        }
        let report = e.verifier.run(false).await.expect("вызов");
        assert_eq!(report.damaged, 3);
        assert_eq!(report.damaged_blobs.len(), 1);
        assert!(report.truncated);
    }
}
