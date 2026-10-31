//! META-02: протокол поколений BlobIndex на PostgreSQL; META-19: перечисление и пометка повреждённых для verify.

mod common;

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use time::OffsetDateTime;
use tp_contracts::blob::{Acquire, BlobIndex, BlobStore, Commit};
use tp_contracts::{BlobHash, BlobKey, Error, Gen};
use tp_meta::{MetaConfig, PgMeta};
use tp_testkit::MemBlobStore;

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
    // Очистка надгробий — по всем организациям: срок 0 удалил бы надгробия параллельных тестов
    // (generations_protocol), поэтому срок короткий, но больше длительности их шагов.
    let ttl = Duration::from_secs(2);
    let Some(idx) = index(ttl).await else {
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
    // Срок надгробия истёк: удаляется при следующем gc_candidates, поколения начинаются заново.
    tokio::time::sleep(ttl + Duration::from_millis(100)).await;
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

/// Сверка, как в `thepost verify`: записи `live` страницами, объект в хранилище отсутствует или другого размера —
/// запись помечается повреждённой.
#[tokio::test]
async fn verify_pages_live_records_and_marks_damaged() {
    let Some(idx) = index(Duration::from_secs(3600)).await else {
        return;
    };
    let tenant = common::tenant(idx.pool()).await;
    let store = MemBlobStore::default();
    let mut keys = Vec::new();
    for i in 0..6u8 {
        let hash = random_hash();
        let Acquire::Absent { generation } = idx.acquire(tenant, &hash, 1).await.expect("acquire")
        else {
            panic!("новый хэш");
        };
        assert_eq!(
            idx.commit_new(tenant, &hash, generation, 10, 1)
                .await
                .expect("commit"),
            Commit::Done { generation }
        );
        let key = BlobKey {
            tenant,
            hash,
            generation,
        };
        // Расхождения: у записи 1 нет объекта, у записи 2 объект другого размера.
        if i != 1 {
            let size = if i == 2 { 7 } else { 10 };
            store.put(&key, vec![0u8; size].into()).await.expect("put");
        }
        keys.push(key);
    }
    // Запись 5 удалена (надгробие) — не `live`, в перечисление не входит.
    let gone = keys[5];
    idx.release(tenant, &gone.hash, gone.generation, 1)
        .await
        .expect("release");
    assert!(idx.mark_deleting(&gone, future()).await.expect("deleting"));
    assert!(idx.finalize_delete(&gone).await.expect("finalize"));

    // Перечисление — со своей организации (в общей тестовой БД есть записи других тестов).
    let start = BlobKey {
        tenant,
        hash: BlobHash([0; 32]),
        generation: Gen::INITIAL,
    };
    let pass = || async {
        let mut after = start;
        let mut seen = Vec::new();
        loop {
            let page = idx.live_blobs(Some(&after), 2).await.expect("page");
            assert!(page.len() <= 2);
            let last = page.last().map(|b| b.key);
            let done = page.len() < 2 || page.iter().any(|b| b.key.tenant != tenant);
            seen.extend(page.into_iter().filter(|b| b.key.tenant == tenant));
            match last {
                Some(k) if !done => after = k,
                _ => break,
            }
        }
        seen
    };

    let seen = pass().await;
    let mut expected: Vec<BlobKey> = keys[..5].to_vec();
    expected.sort_by_key(|k| k.hash.0);
    assert_eq!(seen.iter().map(|b| b.key).collect::<Vec<_>>(), expected);
    assert!(seen.iter().all(|b| b.size == 10 && !b.damaged));
    let mut found = Vec::new();
    for b in &seen {
        if store.head(&b.key).await.expect("head") != Some(b.size) {
            assert!(idx.mark_blob_damaged(&b.key).await.expect("mark"));
            found.push(b.key);
        }
    }
    found.sort_by_key(|k| k.hash.0);
    let mut damaged = vec![keys[1], keys[2]];
    damaged.sort_by_key(|k| k.hash.0);
    assert_eq!(found, damaged);

    // Повторная сверка видит пометку; повторная пометка — без ошибки.
    let again = pass().await;
    for b in &again {
        assert_eq!(b.damaged, damaged.contains(&b.key), "{:?}", b.key);
    }
    assert!(idx.mark_blob_damaged(&keys[1]).await.expect("again"));
    // Нет записи `live` этого поколения — помечать нечего.
    assert!(!idx.mark_blob_damaged(&gone).await.expect("deleted"));
    let other_gen = BlobKey {
        generation: keys[0].generation.next(),
        ..keys[0]
    };
    assert!(!idx.mark_blob_damaged(&other_gen).await.expect("other gen"));
}
