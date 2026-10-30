//! `thepost verify [--fix]` (SRS 3.5; APP-03): сверка хранилища BLOB и учёта объектов в обе стороны (BLOB-07)
//! с отчётом JSON или кратким текстом.
//!
//! Код возврата: 0 — расхождений нет (или с `--fix` все исправлены), 1 — найдены расхождения или ошибки.
//! Режим исправления удаляет «сирот» и не должен идти одновременно с поиском «сирот» на другом узле.

use std::path::Path;
use std::sync::Arc;

use tp_blob::{BlobInventory, Verifier, VerifyConfig, VerifyReport, system_now};
use tp_contracts::blob::{BlobIndex, BlobStore};
use tp_contracts::secrets::SecretStore;
use tp_contracts::{Error, Result};
use tp_meta::{MetaConfig, PgMeta};

use crate::{infra, secrets, sections};

/// Сверка: учёт `index`/`inventory` против хранилища `store`.
pub fn verifier(
    index: Arc<dyn BlobIndex>,
    inventory: Arc<dyn BlobInventory>,
    store: Arc<dyn BlobStore>,
    cfg: VerifyConfig,
) -> Verifier {
    Verifier::new(index, inventory, store, cfg, system_now())
}

/// Код возврата по отчёту.
pub fn exit_code(report: &VerifyReport) -> i32 {
    let ok = report.errors == 0 && (report.fix || report.is_clean());
    i32::from(!ok)
}

/// Краткий текст отчёта для терминала.
pub fn summary(report: &VerifyReport) -> String {
    let mut out = format!(
        "verify{}: объектов {}, записей {}, «сирот» {} (удалено {}), повреждённых записей {} (помечено {}), ошибок {}\n",
        if report.fix { " --fix" } else { "" },
        report.objects_listed,
        report.records_checked,
        report.orphans,
        report.orphans_deleted,
        report.damaged,
        report.damaged_marked,
        report.errors,
    );
    for path in &report.orphan_paths {
        out.push_str(&format!("  сирота: {path}\n"));
    }
    for d in &report.damaged_blobs {
        out.push_str(&format!(
            "  повреждена: {} ({:?}, ожидалось {} байт, найдено {})\n",
            d.path,
            d.damage,
            d.expected_size,
            d.actual_size
                .map_or_else(|| "нет объекта".to_owned(), |s| format!("{s} байт"))
        ));
    }
    if report.truncated {
        out.push_str("  … список сокращён (verify.max_listed)\n");
    }
    out
}

/// Перечисление записей учёта BLOB в PostgreSQL для сверки.
///
/// `tp-meta` пока не даёт перечисления записей `live` и пометки повреждённых (трейт `BlobInventory` — в
/// `tp-blob`, от которого `tp-meta` не зависит): нужна правка полосы meta.
fn pg_inventory(_meta: &PgMeta) -> Result<Arc<dyn BlobInventory>> {
    Err(Error::Invalid(
        "verify: учёт BLOB в PostgreSQL пока не поддерживает перечисление записей и пометку повреждённых \
         (нужна правка tp-meta); сверка недоступна"
            .to_owned(),
    ))
}

/// `thepost verify` по config.yaml `path`.
pub async fn command(
    path: &Path,
    fix: bool,
    store: Option<Arc<dyn SecretStore>>,
) -> Result<VerifyReport> {
    let registry = sections::registry()?;
    let p = path.to_owned();
    let cfg = tokio::task::spawn_blocking(move || registry.load(&p))
        .await
        .map_err(|_| Error::internal("чтение конфигурации прервано"))??;
    let secrets = secrets::store(&cfg, store)?;
    let pool = infra::connect_db(&cfg, secrets.as_ref()).await?;
    let meta = PgMeta::new(
        pool,
        (*cfg.section::<MetaConfig>(MetaConfig::SECTION)?).clone(),
    );
    let blob = infra::open_blob(&cfg, secrets.as_ref()).await?;
    let inventory = pg_inventory(&meta)?;
    verifier(Arc::new(meta), inventory, blob, VerifyConfig::default())
        .run(fix)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::Mutex;
    use std::time::Duration;

    use async_trait::async_trait;
    use bytes::Bytes;
    use futures::StreamExt;
    use futures::stream::{self, BoxStream};
    use tp_blob::{BlobWriter, IndexedBlob, WriterConfig};
    use tp_contracts::{BlobKey, Gen, TenantId};
    use tp_testkit::blob::BlobState;
    use tp_testkit::{Clock, ManualClock, MemBlobIndex, MemBlobStore};

    const GRACE: u64 = 3600;

    /// Перечисление записей поверх `MemBlobIndex` по записанным ключам.
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

    #[tokio::test]
    async fn discrepancies_are_reported_and_fixed() {
        let clock = ManualClock::default();
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
        let mut keys = Vec::new();
        for body in [&b"ok"[..], b"missing", b"resized"] {
            let key = writer
                .write(TenantId::DEFAULT, Bytes::from_static(body), 1)
                .await
                .expect("write");
            keys.push(key);
        }
        inventory
            .keys
            .lock()
            .expect("lock")
            .extend(keys.iter().copied());
        let (ok, missing, resized) = (keys[0], keys[1], keys[2]);
        // Расхождения: объекта нет, объект другого размера, «сирота» без записи учёта.
        store.delete(&missing).await.expect("delete");
        store.delete(&resized).await.expect("delete");
        store
            .put(&resized, Bytes::from_static(b"re"))
            .await
            .expect("put");
        let orphan = BlobKey {
            generation: Gen(7),
            ..ok
        };
        store
            .put(&orphan, Bytes::from_static(b"ok"))
            .await
            .expect("put");
        clock.advance(Duration::from_secs(GRACE + 1));

        let c = clock.clone();
        let verifier = Verifier::new(
            Arc::new(index.clone()),
            inventory.clone(),
            Arc::new(store.clone()),
            VerifyConfig {
                grace_secs: GRACE,
                ..VerifyConfig::default()
            },
            Arc::new(move || c.now()),
        );

        let report = verifier.run(false).await.expect("verify");
        assert_eq!((report.orphans, report.damaged), (1, 2), "{report:?}");
        assert_eq!(exit_code(&report), 1);
        assert!(store.contains(&orphan), "без --fix ничего не удаляется");
        let json = report.to_json().expect("json");
        assert!(json.contains(&orphan.path()), "{json}");
        let text = summary(&report);
        assert!(
            text.contains("«сирот» 1") && text.contains("повреждённых записей 2"),
            "{text}"
        );

        let report = verifier.run(true).await.expect("verify --fix");
        assert_eq!(
            (report.orphans_deleted, report.damaged_marked),
            (1, 2),
            "{report:?}"
        );
        assert_eq!(exit_code(&report), 0);
        assert!(!store.contains(&orphan));
        assert_eq!(
            *inventory.damaged.lock().expect("lock"),
            BTreeSet::from([missing, resized])
        );

        // Хранилище без расхождений: код 0.
        inventory.keys.lock().expect("lock").retain(|k| *k == ok);
        let report = verifier.run(false).await.expect("verify");
        assert!(report.is_clean(), "{report:?}");
        assert_eq!(exit_code(&report), 0);
    }
}
