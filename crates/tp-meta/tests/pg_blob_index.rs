//! META-02: протокол поколений BlobIndex на PostgreSQL.

mod common;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use time::OffsetDateTime;
use tp_contracts::blob::{Acquire, BlobIndex, Commit};
use tp_contracts::{BlobHash, BlobKey, Error, Gen};
use tp_meta::{MetaConfig, PgMeta};

fn random_hash() -> BlobHash {
    let mut h = [0u8; 32];
    h[..16].copy_from_slice(uuid::Uuid::now_v7().as_bytes());
    h[16..].copy_from_slice(uuid::Uuid::now_v7().as_bytes());
    BlobHash(h)
}

fn future() -> OffsetDateTime {
    OffsetDateTime::now_utc() + Duration::from_secs(3600)
}

async fn index(ttl: Duration) -> Option<PgMeta> {
    let pool = common::pool().await?;
    let cfg = MetaConfig {
        blob_tombstone_ttl: ttl,
        ..MetaConfig::default()
    };
    Some(PgMeta::new(pool, cfg))
}

#[tokio::test]
async fn generations_protocol() {
    let Some(idx) = index(Duration::from_secs(3600)).await else {
        return;
    };
    let tenant = common::tenant(idx.pool()).await;
    let hash = random_hash();
    let key = |g| BlobKey {
        tenant,
        hash,
        generation: Gen(g),
    };

    assert_eq!(
        idx.acquire(tenant, &hash, 2).await.expect("acquire"),
        Acquire::Absent {
            generation: Gen::INITIAL
        }
    );
    assert_eq!(
        idx.commit_new(tenant, &hash, Gen(0), 10, 2)
            .await
            .expect("commit"),
        Commit::Done { generation: Gen(0) }
    );
    assert_eq!(
        idx.acquire(tenant, &hash, 1).await.expect("acquire"),
        Acquire::Existing { generation: Gen(0) }
    );
    // Параллельная загрузка того же объекта: ссылки добавляются к существующей записи.
    assert_eq!(
        idx.commit_new(tenant, &hash, Gen(0), 10, 1)
            .await
            .expect("commit"),
        Commit::Done { generation: Gen(0) }
    );
    idx.release(tenant, &hash, Gen(0), 3)
        .await
        .expect("release");
    assert!(
        !idx.mark_deleting(&key(0), future()).await.expect("mark"),
        "ещё есть ссылка"
    );
    idx.release(tenant, &hash, Gen(0), 1)
        .await
        .expect("release");
    assert_eq!(
        idx.release(tenant, &hash, Gen(0), 1).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        idx.release(tenant, &hash, Gen(7), 1).await,
        Err(Error::NotFound)
    );
    assert!(matches!(
        idx.acquire(tenant, &hash, 0).await,
        Err(Error::Invalid(_))
    ));

    // Окно W не истекло — удалять нельзя.
    assert!(
        !idx.mark_deleting(
            &key(0),
            OffsetDateTime::now_utc() - Duration::from_secs(3600)
        )
        .await
        .expect("mark")
    );
    let candidates = idx
        .gc_candidates(future(), 100_000)
        .await
        .expect("candidates");
    assert!(candidates.contains(&key(0)));
    assert!(idx.mark_deleting(&key(0), future()).await.expect("mark"));

    // Запись deleting: новая загрузка — следующим поколением.
    assert_eq!(
        idx.acquire(tenant, &hash, 1).await.expect("acquire"),
        Acquire::Absent { generation: Gen(1) }
    );
    assert_eq!(
        idx.commit_new(tenant, &hash, Gen(0), 10, 1)
            .await
            .expect("commit"),
        Commit::Bumped { generation: Gen(1) }
    );

    // Удаление завершено: надгробие помнит поколение, загрузка по старому поколению — Bumped.
    assert!(idx.finalize_delete(&key(0)).await.expect("finalize"));
    assert!(!idx.exists(&key(0)).await.expect("exists"));
    assert!(!idx.finalize_delete(&key(0)).await.expect("finalize"));
    assert_eq!(
        idx.commit_new(tenant, &hash, Gen(0), 10, 1)
            .await
            .expect("commit"),
        Commit::Bumped { generation: Gen(1) }
    );
    assert_eq!(
        idx.commit_new(tenant, &hash, Gen(1), 10, 1)
            .await
            .expect("commit"),
        Commit::Done { generation: Gen(1) }
    );
    assert!(idx.exists(&key(1)).await.expect("exists"));
    assert!(
        !idx.gc_candidates(future(), 100_000)
            .await
            .expect("gc")
            .contains(&key(1))
    );
}

#[tokio::test]
async fn finalize_checks_generation_and_tombstone_expires() {
    let Some(idx) = index(Duration::ZERO).await else {
        return;
    };
    let tenant = common::tenant(idx.pool()).await;
    let hash = random_hash();
    let key = |g| BlobKey {
        tenant,
        hash,
        generation: Gen(g),
    };
    idx.commit_new(tenant, &hash, Gen(0), 5, 1)
        .await
        .expect("commit");
    idx.release(tenant, &hash, Gen(0), 1)
        .await
        .expect("release");
    assert!(idx.mark_deleting(&key(0), future()).await.expect("mark"));
    // Пока GC удалял объект 0, письмо загрузило поколение 1: запись снова live.
    assert_eq!(
        idx.commit_new(tenant, &hash, Gen(1), 5, 1)
            .await
            .expect("commit"),
        Commit::Done { generation: Gen(1) }
    );
    assert!(!idx.finalize_delete(&key(0)).await.expect("finalize"));
    assert!(idx.exists(&key(1)).await.expect("exists"));

    idx.release(tenant, &hash, Gen(1), 1)
        .await
        .expect("release");
    assert!(idx.mark_deleting(&key(1), future()).await.expect("mark"));
    assert!(idx.finalize_delete(&key(1)).await.expect("finalize"));
    // Срок надгробия 0: удаляется при следующем gc_candidates, поколения начинаются заново.
    tokio::time::sleep(Duration::from_millis(20)).await;
    idx.gc_candidates(future(), 100_000).await.expect("gc");
    assert_eq!(
        idx.acquire(tenant, &hash, 1).await.expect("acquire"),
        Acquire::Absent {
            generation: Gen::INITIAL
        }
    );
}

/// 32 задачи на одном хэше: писатели (acquire → загрузка → commit_new → release) и сборщики мусора
/// (gc_candidates → mark_deleting → удаление объекта → finalize_delete). Хранилище объектов —
/// множество поколений. Инвариант: пока писатель держит ссылку, объект её поколения существует.
#[tokio::test]
async fn concurrent_acquire_release_gc_keeps_references() {
    const TASKS: usize = 32;
    const GC_TASKS: usize = 4;
    const ROUNDS: usize = 25;

    let Some(idx) = index(Duration::from_secs(3600)).await else {
        return;
    };
    let idx = Arc::new(idx);
    let tenant = common::tenant(idx.pool()).await;
    let hash = random_hash();
    let store: Arc<Mutex<HashSet<u32>>> = Arc::default();
    let writers_done = Arc::new(AtomicBool::new(false));
    let deleted = Arc::new(AtomicU32::new(0));
    let has = |store: &Mutex<HashSet<u32>>, g: Gen| store.lock().expect("lock").contains(&g.0);

    let mut gc = Vec::new();
    for _ in 0..GC_TASKS {
        let (idx, store, done, deleted) = (
            idx.clone(),
            store.clone(),
            writers_done.clone(),
            deleted.clone(),
        );
        gc.push(tokio::spawn(async move {
            while !done.load(Ordering::Relaxed) {
                let keys = idx.gc_candidates(future(), 100_000).await.expect("gc");
                for key in keys
                    .into_iter()
                    .filter(|k| k.tenant == tenant && k.hash == hash)
                {
                    if idx.mark_deleting(&key, future()).await.expect("mark") {
                        store.lock().expect("lock").remove(&key.generation.0);
                        deleted.fetch_add(1, Ordering::Relaxed);
                        idx.finalize_delete(&key).await.expect("finalize");
                    }
                }
                tokio::task::yield_now().await;
            }
        }));
    }

    let mut writers = Vec::new();
    for _ in 0..TASKS - GC_TASKS {
        let (idx, store) = (idx.clone(), store.clone());
        writers.push(tokio::spawn(async move {
            for _ in 0..ROUNDS {
                let held = match idx.acquire(tenant, &hash, 1).await.expect("acquire") {
                    Acquire::Existing { generation } => generation,
                    Acquire::Absent { mut generation } => loop {
                        store.lock().expect("lock").insert(generation.0);
                        match idx
                            .commit_new(tenant, &hash, generation, 1, 1)
                            .await
                            .expect("commit")
                        {
                            Commit::Done { generation } => break generation,
                            Commit::Bumped { generation: next } => generation = next,
                        }
                    },
                };
                assert!(has(&store, held), "объект {held} удалён при живой ссылке");
                tokio::task::yield_now().await;
                assert!(has(&store, held), "объект {held} удалён при живой ссылке");
                idx.release(tenant, &hash, held, 1).await.expect("release");
            }
        }));
    }
    for w in writers {
        w.await.expect("writer");
    }
    writers_done.store(true, Ordering::Relaxed);
    for g in gc {
        g.await.expect("gc");
    }

    // Все ссылки возвращены: у живой записи счётчик 0.
    let refcnt: Option<i64> = {
        let mut tx = tp_meta::schema::begin(idx.pool(), tenant)
            .await
            .expect("begin");
        sqlx::query_scalar("SELECT refcnt FROM blobs WHERE tenant_id = $1 AND state = 'live'")
            .bind(tenant.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .expect("select")
    };
    assert!(matches!(refcnt, None | Some(0)), "refcnt = {refcnt:?}");
    eprintln!("удалено объектов: {}", deleted.load(Ordering::Relaxed));
}
