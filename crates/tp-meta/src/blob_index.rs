//! Учёт объектов BLOB на PostgreSQL: протокол поколений (SRS 3.4, CONTRACTS.md#BlobIndex).
//!
//! Одна строка `blobs` на хэш в организации; атомарность каждого шага — одна строка и одна короткая транзакция.
//! `finalize_delete` оставляет надгробие (`state = 'deleted'`) с поколением удалённого объекта: загрузка
//! по тому же поколению, начатая до удаления (`acquire` видел отсутствие записи), при `commit_new` получит
//! `Bumped`, а не ссылку на удалённый объект. Надгробия старше `MetaConfig::blob_tombstone_ttl`
//! удаляются в `gc_candidates`.
//!
//! Для `thepost verify` (SRS 3.5) — постраничное перечисление записей `live` по всем организациям
//! ([`PgMeta::live_blobs`]) и пометка повреждённых ([`PgMeta::mark_blob_damaged`]); в контракт [`BlobIndex`]
//! они не входят.

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::blob::{Acquire, BlobIndex, Commit};
use tp_contracts::{BlobHash, BlobKey, Error, Gen, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::schema::{begin, begin_system, db_err, to_u32};

/// Число ссылок операции: не меньше одной.
fn refs_arg(refs: u32) -> Result<i64> {
    if refs == 0 {
        return Err(Error::invalid("число ссылок должно быть больше нуля"));
    }
    Ok(i64::from(refs))
}

fn gen_of(v: i64) -> Result<Gen> {
    to_u32(v).map(Gen)
}

fn key_of(tenant: Uuid, hash: Vec<u8>, g: i64) -> Result<BlobKey> {
    let hash: [u8; 32] = hash
        .try_into()
        .map_err(|_| Error::internal("длина хэша в blobs"))?;
    Ok(BlobKey {
        tenant: TenantId::from_uuid(tenant),
        hash: BlobHash(hash),
        generation: gen_of(g)?,
    })
}

/// Запись учёта `live` для сверки ([`PgMeta::live_blobs`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveBlob {
    /// Ключ объекта (поколение записи).
    pub key: BlobKey,
    /// Размер по учёту, байт.
    pub size: u64,
    /// Запись уже помечена повреждённой ([`PgMeta::mark_blob_damaged`]).
    pub damaged: bool,
}

impl PgMeta {
    /// Страница записей `live` по всем организациям в порядке (организация, хэш): не больше `limit` записей
    /// после `after` — ключа последней записи предыдущей страницы (`None` — с начала; поколение не учитывается).
    /// Страница короче `limit` — последняя. Каждая страница — своя короткая транзакция по первичному ключу:
    /// записи, добавленные или удалённые во время перечисления, могут попасть или не попасть в него.
    pub async fn live_blobs(&self, after: Option<&BlobKey>, limit: usize) -> Result<Vec<LiveBlob>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        // Нулевой uuid и пустой хэш меньше любого ключа таблицы (хэш — 32 байта).
        let (tenant, hash) = after.map_or((Uuid::nil(), Vec::new()), |k| {
            (*k.tenant.as_uuid(), k.hash.as_bytes().to_vec())
        });
        let mut tx = begin_system(&self.pool).await?;
        let rows: Vec<(Uuid, Vec<u8>, i64, i64, bool)> = sqlx::query_as(
            "SELECT tenant_id, hash, gen, size, damaged_at IS NOT NULL FROM blobs
             WHERE (tenant_id, hash) > ($1, $2) AND state = 'live'
             ORDER BY tenant_id, hash LIMIT $3",
        )
        .bind(tenant)
        .bind(hash)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        rows.into_iter()
            .map(|(tenant, hash, g, size, damaged)| {
                Ok(LiveBlob {
                    key: key_of(tenant, hash, g)?,
                    size: u64::try_from(size).map_err(|_| Error::internal("размер в blobs"))?,
                    damaged,
                })
            })
            .collect()
    }

    /// Помечает запись `live` поколения `key` повреждённой: объекта нет в хранилище или он испорчен (SRS 3.5).
    /// Повторная пометка сохраняет время первой. `false` — такой записи нет (объект удалён или
    /// загружен заново другим поколением): помечать нечего.
    pub async fn mark_blob_damaged(&self, key: &BlobKey) -> Result<bool> {
        let mut tx = begin(&self.pool, key.tenant).await?;
        let marked = sqlx::query(
            "UPDATE blobs SET damaged_at = coalesce(damaged_at, now())
             WHERE tenant_id = $1 AND hash = $2 AND gen = $3 AND state = 'live'",
        )
        .bind(key.tenant.as_uuid())
        .bind(key.hash.as_bytes().as_slice())
        .bind(i64::from(key.generation.0))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected()
            == 1;
        tx.commit().await.map_err(db_err)?;
        Ok(marked)
    }
}

#[async_trait]
impl BlobIndex for PgMeta {
    async fn acquire(&self, tenant: TenantId, hash: &BlobHash, refs: u32) -> Result<Acquire> {
        let refs = refs_arg(refs)?;
        let mut tx = begin(&self.pool, tenant).await?;
        // Вложенный SELECT видит снимок до UPDATE: поколение записи в любом состоянии.
        let (live, any): (Option<i64>, Option<i64>) = sqlx::query_as(
            "WITH up AS (
                 UPDATE blobs SET refcnt = refcnt + $3, zero_since = NULL
                 WHERE tenant_id = $1 AND hash = $2 AND state = 'live'
                 RETURNING gen)
             SELECT (SELECT gen FROM up), (SELECT gen FROM blobs WHERE tenant_id = $1 AND hash = $2)",
        )
        .bind(tenant.as_uuid())
        .bind(hash.as_bytes().as_slice())
        .bind(refs)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(match (live, any) {
            (Some(g), _) => Acquire::Existing {
                generation: gen_of(g)?,
            },
            // Запись `deleting` или надгробие: объект этого поколения удаляется или удалён.
            (None, Some(g)) => Acquire::Absent {
                generation: gen_of(g)?.next(),
            },
            (None, None) => Acquire::Absent {
                generation: Gen::INITIAL,
            },
        })
    }

    async fn commit_new(
        &self,
        tenant: TenantId,
        hash: &BlobHash,
        generation: Gen,
        size: u64,
        refs: u32,
    ) -> Result<Commit> {
        let refs = refs_arg(refs)?;
        let size = crate::schema::to_i64(size)?;
        let mut tx = begin(&self.pool, tenant).await?;
        // Конфликт: запись live — добавить ссылки; deleting/deleted со старшим поколением — живая запись
        // нашего поколения; deleting/deleted с тем же или большим поколением — без изменений, Bumped.
        let (g, state): (i64, String) = sqlx::query_as(
            "INSERT INTO blobs (tenant_id, hash, gen, refcnt, state, size)
             VALUES ($1, $2, $3, $4, 'live', $5)
             ON CONFLICT (tenant_id, hash) DO UPDATE SET
                 refcnt = CASE WHEN blobs.state = 'live' THEN blobs.refcnt + EXCLUDED.refcnt
                               WHEN blobs.gen < EXCLUDED.gen THEN EXCLUDED.refcnt
                               ELSE blobs.refcnt END,
                 gen = CASE WHEN blobs.state <> 'live' AND blobs.gen < EXCLUDED.gen THEN EXCLUDED.gen
                            ELSE blobs.gen END,
                 size = CASE WHEN blobs.state <> 'live' AND blobs.gen < EXCLUDED.gen THEN EXCLUDED.size
                             ELSE blobs.size END,
                 zero_since = CASE WHEN blobs.state = 'live' OR blobs.gen < EXCLUDED.gen THEN NULL
                                   ELSE blobs.zero_since END,
                 state = CASE WHEN blobs.state = 'live' OR blobs.gen < EXCLUDED.gen THEN 'live'
                              ELSE blobs.state END
             RETURNING gen, state",
        )
        .bind(tenant.as_uuid())
        .bind(hash.as_bytes().as_slice())
        .bind(i64::from(generation.0))
        .bind(refs)
        .bind(size)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        let g = gen_of(g)?;
        Ok(if state == "live" {
            Commit::Done { generation: g }
        } else {
            Commit::Bumped {
                generation: g.next(),
            }
        })
    }

    async fn release(
        &self,
        tenant: TenantId,
        hash: &BlobHash,
        generation: Gen,
        refs: u32,
    ) -> Result<()> {
        let refs = refs_arg(refs)?;
        let mut tx = begin(&self.pool, tenant).await?;
        let (released, present): (bool, bool) = sqlx::query_as(
            "WITH up AS (
                 UPDATE blobs SET refcnt = refcnt - $4,
                                  zero_since = CASE WHEN refcnt = $4 THEN now() ELSE zero_since END
                 WHERE tenant_id = $1 AND hash = $2 AND gen = $3 AND state = 'live' AND refcnt >= $4
                 RETURNING 1)
             SELECT EXISTS (SELECT 1 FROM up),
                    EXISTS (SELECT 1 FROM blobs WHERE tenant_id = $1 AND hash = $2 AND gen = $3
                                                  AND state = 'live')",
        )
        .bind(tenant.as_uuid())
        .bind(hash.as_bytes().as_slice())
        .bind(i64::from(generation.0))
        .bind(refs)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        match (released, present) {
            (true, _) => Ok(()),
            (false, true) => Err(Error::Conflict),
            (false, false) => Err(Error::NotFound),
        }
    }

    async fn gc_candidates(
        &self,
        older_than: OffsetDateTime,
        limit: usize,
    ) -> Result<Vec<BlobKey>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut tx = begin_system(&self.pool).await?;
        sqlx::query(
            "DELETE FROM blobs WHERE (tenant_id, hash) IN (
                 SELECT tenant_id, hash FROM blobs
                 WHERE refcnt = 0 AND state = 'deleted' AND zero_since < now() - make_interval(secs => $1)
                 LIMIT $2)",
        )
        .bind(self.cfg.blob_tombstone_ttl.as_secs_f64())
        .bind(limit)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        let rows: Vec<(Uuid, Vec<u8>, i64)> = sqlx::query_as(
            "SELECT tenant_id, hash, gen FROM blobs
             WHERE refcnt = 0 AND state <> 'deleted' AND zero_since < $1
             ORDER BY zero_since LIMIT $2",
        )
        .bind(older_than)
        .bind(limit)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        rows.into_iter()
            .map(|(tenant, hash, g)| key_of(tenant, hash, g))
            .collect()
    }

    async fn mark_deleting(&self, key: &BlobKey, older_than: OffsetDateTime) -> Result<bool> {
        let mut tx = begin(&self.pool, key.tenant).await?;
        let marked = sqlx::query(
            "UPDATE blobs SET state = 'deleting'
             WHERE tenant_id = $1 AND hash = $2 AND gen = $3 AND refcnt = 0
               AND state IN ('live', 'deleting') AND zero_since < $4",
        )
        .bind(key.tenant.as_uuid())
        .bind(key.hash.as_bytes().as_slice())
        .bind(i64::from(key.generation.0))
        .bind(older_than)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected()
            == 1;
        tx.commit().await.map_err(db_err)?;
        Ok(marked)
    }

    async fn finalize_delete(&self, key: &BlobKey) -> Result<bool> {
        let mut tx = begin(&self.pool, key.tenant).await?;
        let done = sqlx::query(
            "UPDATE blobs SET state = 'deleted', zero_since = now()
             WHERE tenant_id = $1 AND hash = $2 AND gen = $3 AND state = 'deleting'",
        )
        .bind(key.tenant.as_uuid())
        .bind(key.hash.as_bytes().as_slice())
        .bind(i64::from(key.generation.0))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected()
            == 1;
        tx.commit().await.map_err(db_err)?;
        Ok(done)
    }

    async fn exists(&self, key: &BlobKey) -> Result<bool> {
        let mut tx = begin(&self.pool, key.tenant).await?;
        let found: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM blobs
                            WHERE tenant_id = $1 AND hash = $2 AND gen = $3 AND state <> 'deleted')",
        )
        .bind(key.tenant.as_uuid())
        .bind(key.hash.as_bytes().as_slice())
        .bind(i64::from(key.generation.0))
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(found)
    }
}
