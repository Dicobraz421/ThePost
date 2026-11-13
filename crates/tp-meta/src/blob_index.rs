//! Учёт объектов BLOB на PostgreSQL: протокол поколений (SRS 3.4, CONTRACTS.md#BlobIndex).
//!
//! Одна строка `blobs` на хэш в организации; атомарность каждого шага — одна строка и одна короткая транзакция.
//! `finalize_delete` оставляет надгробие (`state = 'deleted'`) с поколением удалённого объекта: загрузка
//! по тому же поколению, начатая до удаления (`acquire` видел отсутствие записи), при `commit_new` получит
//! `Bumped`, а не ссылку на удалённый объект. Надгробия старше `MetaConfig::blob_tombstone_ttl`
//! удаляются в `gc_candidates`.
//!
//! Признак сжатия объекта (META-24, SRS 3.2) — столбец `compression`: задаётся при создании записи `live`
//! ([`BlobIndex::commit_new_with`]), повторные ссылки и параллельная загрузка его не меняют.
//!
//! Для `thepost verify` (SRS 3.5) — постраничное перечисление записей `live` по всем организациям
//! ([`PgMeta::live_blobs`]) и пометка повреждённых ([`PgMeta::mark_blob_damaged`]); в контракт [`BlobIndex`]
//! они не входят.
//!
//! Шарды (META-31): учёт ссылок — на шарде ящика (ссылки копий и удалений писем меняют транзакции ящика;
//! загрузку на шарде учитывает вызывающий, знающий шард ящика, — [`PgMeta::on_shard`]). Хранилища со
//! справочником ([`PgMeta::with_shards`]) ведут `acquire`, `commit_new`, `release` в основной БД (учётную запись
//! `owner` из `acquire`, CTR-49, пока не используют: `commit_new` и `release` её не получают, а протокол
//! поколений требует одного шарда на все три шага), а сборку
//! мусора — по всем шардам: объект — кандидат, только если ссылок на него нет ни на одном шарде;
//! `mark_deleting` под блокировкой строки каждого шарда по очереди ставит `deleting` (шарду без строки —
//! надгробие того же поколения, чтобы параллельная загрузка на нём получила новое поколение) и откатывает
//! пометки, если на каком-то шарде ссылки есть; `finalize_delete` — на всех шардах.
//!
//! Кандидаты GC и срок следующего прохода (META-73) — один путь, `PgMeta::zeroed`: записи без ссылок в порядке
//! `zero_since`. На одном шарде — один запрос по индексу `blobs_gc` (срок — `MIN(zero_since) + W`); по всем
//! шардам занятые другим шардом записи отбрасываются до счёта `limit`, а срок записи — самое позднее
//! обнуление среди шардов.

use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::blob::{Acquire, BlobIndex, Commit, Compression};
use tp_contracts::{AccountId, BlobHash, BlobKey, Error, Gen, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::pool::with_tenant;
use crate::schema::{begin, begin_system, db_err, to_u32};
use crate::shards::ShardId;

/// Пометка `deleting` строки шарда в [`mark_deleting_sharded`]: как откатить.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marked {
    /// Строка была `live` — вернуть `live`.
    Live,
    /// Строка уже была `deleting` — оставить.
    Deleting,
    /// Строки не было — удалить надгробие.
    Tombstone,
}

/// Исход проверки шарда.
enum Check {
    /// Шард помечен (или помечать нечего — другое поколение, уже удалён); `true` — строка поколения
    /// объекта без ссылок, освобождённая раньше `older_than`.
    Marked(Option<Marked>, bool),
    /// На шарде есть ссылки или объект освобождён позже `older_than` — не удалять.
    Busy,
}

/// Строка учёта шарда для [`mark_shard`]: поколение, ссылки, состояние, момент обнуления.
type MarkRow = (i64, i64, String, Option<OffsetDateTime>);

/// Помечает строку шарда `view` поколения `key` для удаления (см. описание модуля).
async fn mark_shard(view: &PgMeta, key: &BlobKey, older_than: OffsetDateTime) -> Result<Check> {
    let mut tx = begin(view.pool(), key.tenant).await?;
    let row: Option<MarkRow> = sqlx::query_as(
        "SELECT gen, refcnt, state, zero_since FROM blobs WHERE tenant_id = $1 AND hash = $2 FOR UPDATE",
    )
    .bind(key.tenant.as_uuid())
    .bind(key.hash.as_bytes().as_slice())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    let g = i64::from(key.generation.0);
    let check = match row {
        None => {
            sqlx::query(
                "INSERT INTO blobs (tenant_id, hash, gen, refcnt, state, size, zero_since)
                 VALUES ($1, $2, $3, 0, 'deleting', 0, now())",
            )
            .bind(key.tenant.as_uuid())
            .bind(key.hash.as_bytes().as_slice())
            .bind(g)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            Check::Marked(Some(Marked::Tombstone), false)
        }
        Some((rg, refcnt, state, zero_since)) if rg == g && state != "deleted" => {
            if refcnt > 0 || zero_since.is_none_or(|z| z >= older_than) {
                Check::Busy
            } else {
                sqlx::query(
                    "UPDATE blobs SET state = 'deleting' WHERE tenant_id = $1 AND hash = $2 AND gen = $3",
                )
                .bind(key.tenant.as_uuid())
                .bind(key.hash.as_bytes().as_slice())
                .bind(g)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                let was = if state == "live" {
                    Marked::Live
                } else {
                    Marked::Deleting
                };
                Check::Marked(Some(was), true)
            }
        }
        // Другое поколение — другой объект; надгробие удалённого — помечать нечего.
        Some(_) => Check::Marked(None, false),
    };
    crate::commits::commit(tx).await?;
    Ok(check)
}

/// Откатывает пометку `marked` строки шарда `view`.
async fn unmark_shard(view: &PgMeta, key: &BlobKey, marked: Marked) -> Result<()> {
    let sql = match marked {
        Marked::Deleting => return Ok(()),
        Marked::Live => {
            "UPDATE blobs SET state = 'live'
             WHERE tenant_id = $1 AND hash = $2 AND gen = $3 AND state = 'deleting'"
        }
        Marked::Tombstone => {
            "DELETE FROM blobs
             WHERE tenant_id = $1 AND hash = $2 AND gen = $3 AND state = 'deleting' AND refcnt = 0"
        }
    };
    let mut tx = begin(view.pool(), key.tenant).await?;
    sqlx::query(sql)
        .bind(key.tenant.as_uuid())
        .bind(key.hash.as_bytes().as_slice())
        .bind(i64::from(key.generation.0))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    crate::commits::commit(tx).await?;
    Ok(())
}

/// `mark_deleting` по всем шардам `views` (см. описание модуля): `true` — объект помечен хотя бы на одном
/// шарде и ни на одном нет ссылок.
async fn mark_deleting_sharded(
    views: &[(ShardId, PgMeta)],
    key: &BlobKey,
    older_than: OffsetDateTime,
) -> Result<bool> {
    let mut done: Vec<(&PgMeta, Marked)> = Vec::new();
    let mut eligible = false;
    for (_, view) in views {
        match mark_shard(view, key, older_than).await {
            Ok(Check::Marked(m, e)) => {
                if let Some(m) = m {
                    done.push((view, m));
                }
                eligible |= e;
            }
            Ok(Check::Busy) => {
                rollback_marks(&done, key).await;
                return Ok(false);
            }
            Err(e) => {
                rollback_marks(&done, key).await;
                return Err(e);
            }
        }
    }
    if !eligible {
        rollback_marks(&done, key).await;
    }
    Ok(eligible)
}

/// Откат пометок; ошибка отката — в журнал (строка `deleting` без ссылок — снова кандидат GC).
async fn rollback_marks(done: &[(&PgMeta, Marked)], key: &BlobKey) {
    for (view, m) in done {
        if let Err(e) = unmark_shard(view, key, *m).await {
            tracing::warn!(error = %e, "пометка удаления BLOB на шарде не откачена");
        }
    }
}

/// Запись без ссылок для GC: ключ и момент обнуления (`zero_since`).
type Zeroed = (BlobKey, OffsetDateTime);

/// Наименьшая страница [`free_zeroed`]: занятые другим шардом записи впереди не требуют запроса на каждую.
const ZEROED_PAGE: usize = 256;

/// Записи шарда `view` без ссылок (`refcnt = 0`, не надгробие) в порядке (`zero_since`, организация, хэш):
/// после `after`, с `zero_since < older_than` (`None` — без границы), не больше `limit`. Один запрос по индексу
/// `blobs_gc`.
async fn zeroed_page(
    view: &PgMeta,
    older_than: Option<OffsetDateTime>,
    after: Option<&Zeroed>,
    limit: usize,
) -> Result<Vec<Zeroed>> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut tx = begin_system(view.pool()).await?;
    let q = match after {
        None => sqlx::query_as(
            "SELECT tenant_id, hash, gen, zero_since FROM blobs
             WHERE refcnt = 0 AND state <> 'deleted' AND zero_since IS NOT NULL
               AND ($1::timestamptz IS NULL OR zero_since < $1)
             ORDER BY zero_since, tenant_id, hash LIMIT $2",
        )
        .bind(older_than)
        .bind(limit),
        Some((key, zero)) => sqlx::query_as(
            "SELECT tenant_id, hash, gen, zero_since FROM blobs
             WHERE refcnt = 0 AND state <> 'deleted' AND zero_since >= $3
               AND (zero_since, tenant_id, hash) > ($3, $4, $5)
               AND ($1::timestamptz IS NULL OR zero_since < $1)
             ORDER BY zero_since, tenant_id, hash LIMIT $2",
        )
        .bind(older_than)
        .bind(limit)
        .bind(*zero)
        .bind(key.tenant.as_uuid())
        .bind(key.hash.as_bytes().as_slice()),
    };
    let rows: Vec<(Uuid, Vec<u8>, i64, OffsetDateTime)> =
        q.fetch_all(&mut *tx).await.map_err(db_err)?;
    crate::commits::commit(tx).await?;
    rows.into_iter()
        .map(|(t, h, g, z)| Ok((key_of(t, h, g)?, z)))
        .collect()
}

/// Ключи из `zeroed`, которые шард `view` не отдаёт GC: строка того же поколения (не надгробие) со ссылками
/// или обнулённая позже (срок записи — по самому позднему обнулению среди шардов).
async fn held(view: &PgMeta, zeroed: &[Zeroed]) -> Result<Vec<BlobKey>> {
    let tenants: Vec<Uuid> = zeroed.iter().map(|(k, _)| *k.tenant.as_uuid()).collect();
    let hashes: Vec<Vec<u8>> = zeroed
        .iter()
        .map(|(k, _)| k.hash.as_bytes().to_vec())
        .collect();
    let gens: Vec<i64> = zeroed
        .iter()
        .map(|(k, _)| i64::from(k.generation.0))
        .collect();
    let zeros: Vec<OffsetDateTime> = zeroed.iter().map(|(_, z)| *z).collect();
    let mut tx = begin_system(view.pool()).await?;
    let rows: Vec<(Uuid, Vec<u8>, i64)> = sqlx::query_as(
        "SELECT b.tenant_id, b.hash, b.gen
         FROM unnest($1::uuid[], $2::bytea[], $3::int8[], $4::timestamptz[]) AS k(t, h, g, z)
         JOIN blobs b ON b.tenant_id = k.t AND b.hash = k.h AND b.gen = k.g
         WHERE b.state <> 'deleted' AND (b.refcnt > 0 OR b.zero_since IS NULL OR b.zero_since > k.z)",
    )
    .bind(tenants)
    .bind(hashes)
    .bind(gens)
    .bind(zeros)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    crate::commits::commit(tx).await?;
    rows.into_iter().map(|(t, h, g)| key_of(t, h, g)).collect()
}

/// Записи без ссылок ни на одном шарде `views` в порядке срока — самого позднего обнуления среди шардов —
/// с ним раньше `older_than` (`None` — без границы), не больше `limit`.
///
/// Запись выбирается на шарде, где она обнулена позже всего (на остальных её отсекает [`held`]), поэтому
/// каждый шард отдаёт до `limit` своих записей по порядку `zero_since`. Занятые записи отбрасываются до счёта
/// `limit`: страницы шарда читаются дальше, пока не набрано `limit` свободных или записи не кончились.
async fn free_zeroed(
    views: &[(ShardId, PgMeta)],
    older_than: Option<OffsetDateTime>,
    limit: usize,
) -> Result<Vec<Zeroed>> {
    let page = limit.max(ZEROED_PAGE);
    let mut out: Vec<Zeroed> = Vec::new();
    for (i, (_, view)) in views.iter().enumerate() {
        let mut after: Option<Zeroed> = None;
        let mut found = 0;
        while found < limit {
            let mut rows = view
                .idempotent(|| zeroed_page(view, older_than, after.as_ref(), page))
                .await?;
            let Some(last) = rows.last().copied() else {
                break;
            };
            let full = rows.len() == page;
            after = Some(last);
            for (j, (_, other)) in views.iter().enumerate() {
                if j == i || rows.is_empty() {
                    continue;
                }
                let busy = other.idempotent(|| held(other, &rows)).await?;
                rows.retain(|(k, _)| !busy.contains(k));
            }
            for z in rows.into_iter().take(limit - found) {
                found += 1;
                // Одинаковый момент обнуления на двух шардах — запись выбрана на обоих.
                if !out.iter().any(|(k, _)| *k == z.0) {
                    out.push(z);
                }
            }
            if !full {
                break;
            }
        }
    }
    out.sort_by_key(|(_, z)| *z);
    out.truncate(limit);
    Ok(out)
}

/// Удаляет надгробия шарда `view` старше `MetaConfig::blob_tombstone_ttl`, не больше `limit`.
async fn prune_tombstones(view: &PgMeta, limit: usize) -> Result<()> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut tx = begin_system(view.pool()).await?;
    sqlx::query(
        "DELETE FROM blobs WHERE (tenant_id, hash) IN (
             SELECT tenant_id, hash FROM blobs
             WHERE refcnt = 0 AND state = 'deleted' AND zero_since < now() - make_interval(secs => $1)
             LIMIT $2)",
    )
    .bind(view.cfg.blob_tombstone_ttl.as_secs_f64())
    .bind(limit)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    crate::commits::commit(tx).await
}

/// Число ссылок операции: не меньше одной.
fn refs_arg(refs: u32) -> Result<i64> {
    if refs == 0 {
        return Err(Error::invalid("число ссылок должно быть больше нуля"));
    }
    Ok(i64::from(refs))
}

fn compression_of(s: &str) -> Result<Compression> {
    s.parse()
        .map_err(|_| Error::internal(format!("сжатие в blobs: {s}")))
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
    /// Размер исходного содержимого по учёту, байт (у сжатого объекта в хранилище — меньше).
    pub size: u64,
    /// Сжатие объекта в хранилище.
    pub compression: Compression,
    /// Запись уже помечена повреждённой ([`PgMeta::mark_blob_damaged`]).
    pub damaged: bool,
}

impl PgMeta {
    /// Записи без ссылок в порядке срока GC (`zero_since < older_than`; `None` — без границы), не больше
    /// `limit`: общий путь [`BlobIndex::gc_candidates`] и [`BlobIndex::next_gc_due`]. Без справочника шардов —
    /// один запрос; со справочником — [`free_zeroed`] по всем шардам.
    async fn zeroed(
        &self,
        older_than: Option<OffsetDateTime>,
        limit: usize,
    ) -> Result<Vec<Zeroed>> {
        match self.shard_views().await? {
            Some(views) => free_zeroed(&views, older_than, limit).await,
            None => {
                self.idempotent(|| zeroed_page(self, older_than, None, limit))
                    .await
            }
        }
    }

    /// Страница записей `live` по всем организациям в порядке (организация, хэш): не больше `limit` записей
    /// после `after` — ключа последней записи предыдущей страницы (`None` — с начала; поколение не учитывается).
    /// Страница короче `limit` — последняя. Каждая страница — своя короткая транзакция по первичному ключу:
    /// записи, добавленные или удалённые во время перечисления, могут попасть или не попасть в него.
    ///
    /// Со справочником шардов — слияние страниц всех шардов (запись с тем же ключом на нескольких шардах —
    /// одна; повреждённая, если помечена хоть на одном).
    pub async fn live_blobs(&self, after: Option<&BlobKey>, limit: usize) -> Result<Vec<LiveBlob>> {
        if let Some(views) = self.shard_views().await? {
            let mut all: Vec<LiveBlob> = Vec::new();
            for (_, view) in &views {
                all.extend(Box::pin(view.live_blobs(after, limit)).await?);
            }
            all.sort_by(|a, b| {
                (a.key.tenant, a.key.hash.as_bytes(), a.key.generation).cmp(&(
                    b.key.tenant,
                    b.key.hash.as_bytes(),
                    b.key.generation,
                ))
            });
            let mut out: Vec<LiveBlob> = Vec::with_capacity(all.len().min(limit));
            for b in all {
                match out.last_mut() {
                    Some(last) if last.key == b.key => last.damaged |= b.damaged,
                    _ => out.push(b),
                }
            }
            out.truncate(limit);
            return Ok(out);
        }
        self.idempotent(|| async move {
            let limit = i64::try_from(limit).unwrap_or(i64::MAX);
            // Нулевой uuid и пустой хэш меньше любого ключа таблицы (хэш — 32 байта).
            let (tenant, hash) = after.map_or((Uuid::nil(), Vec::new()), |k| {
                (*k.tenant.as_uuid(), k.hash.as_bytes().to_vec())
            });
            let mut tx = begin_system(&self.read_pool(ShardId::MAIN).await?).await?;
            let rows: Vec<(Uuid, Vec<u8>, i64, i64, bool, String)> = sqlx::query_as(
                "SELECT tenant_id, hash, gen, size, damaged_at IS NOT NULL, compression FROM blobs
             WHERE (tenant_id, hash) > ($1, $2) AND state = 'live'
             ORDER BY tenant_id, hash LIMIT $3",
            )
            .bind(tenant)
            .bind(hash)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            rows.into_iter()
                .map(|(tenant, hash, g, size, damaged, compression)| {
                    Ok(LiveBlob {
                        key: key_of(tenant, hash, g)?,
                        size: u64::try_from(size).map_err(|_| Error::internal("размер в blobs"))?,
                        damaged,
                        compression: compression_of(&compression)?,
                    })
                })
                .collect()
        })
        .await
    }

    /// Помечает запись `live` поколения `key` повреждённой: объекта нет в хранилище или он испорчен (SRS 3.5).
    /// Повторная пометка сохраняет время первой. `false` — такой записи нет (объект удалён или
    /// загружен заново другим поколением): помечать нечего.
    pub async fn mark_blob_damaged(&self, key: &BlobKey) -> Result<bool> {
        if let Some(views) = self.shard_views().await? {
            let mut marked = false;
            for (_, view) in &views {
                marked |= Box::pin(view.mark_blob_damaged(key)).await?;
            }
            return Ok(marked);
        }
        self.rewrite(|| async move {
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
            crate::commits::commit(tx).await?;
            Ok(marked)
        })
        .await
    }
}

#[async_trait]
impl BlobIndex for PgMeta {
    async fn acquire(
        &self,
        tenant: TenantId,
        owner: Option<AccountId>,
        hash: &BlobHash,
        refs: u32,
    ) -> Result<Acquire> {
        // Шард по `owner` — после `commit_new`/`release` с учётной записью (см. описание модуля).
        let _ = owner;
        self.once(async move {
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
        crate::commits::commit(tx).await?;
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
}).await
    }

    async fn commit_new(
        &self,
        tenant: TenantId,
        hash: &BlobHash,
        generation: Gen,
        size: u64,
        refs: u32,
    ) -> Result<Commit> {
        self.commit_new_with(tenant, hash, generation, size, refs, Compression::None)
            .await
    }

    fn supports_compression(&self) -> bool {
        true
    }

    async fn commit_new_with(
        &self,
        tenant: TenantId,
        hash: &BlobHash,
        generation: Gen,
        size: u64,
        refs: u32,
        compression: Compression,
    ) -> Result<Commit> {
        self.once(async move {
        let refs = refs_arg(refs)?;
        let size = crate::schema::to_i64(size)?;
        let mut tx = begin(&self.pool, tenant).await?;
        // Конфликт: запись live — добавить ссылки (признак сжатия — её); deleting/deleted со старшим
        // поколением — живая запись нашего поколения и сжатия; deleting/deleted с тем же или большим
        // поколением — без изменений, Bumped.
        let (g, state): (i64, String) = sqlx::query_as(
            "INSERT INTO blobs (tenant_id, hash, gen, refcnt, state, size, compression)
             VALUES ($1, $2, $3, $4, 'live', $5, $6)
             ON CONFLICT (tenant_id, hash) DO UPDATE SET
                 refcnt = CASE WHEN blobs.state = 'live' THEN blobs.refcnt + EXCLUDED.refcnt
                               WHEN blobs.gen < EXCLUDED.gen THEN EXCLUDED.refcnt
                               ELSE blobs.refcnt END,
                 gen = CASE WHEN blobs.state <> 'live' AND blobs.gen < EXCLUDED.gen THEN EXCLUDED.gen
                            ELSE blobs.gen END,
                 size = CASE WHEN blobs.state <> 'live' AND blobs.gen < EXCLUDED.gen THEN EXCLUDED.size
                             ELSE blobs.size END,
                 compression = CASE WHEN blobs.state <> 'live' AND blobs.gen < EXCLUDED.gen
                                    THEN EXCLUDED.compression ELSE blobs.compression END,
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
        .bind(compression.as_str())
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        crate::commits::commit(tx).await?;
        let g = gen_of(g)?;
        Ok(if state == "live" {
            Commit::Done { generation: g }
        } else {
            Commit::Bumped {
                generation: g.next(),
            }
        })
}).await
    }

    async fn release(
        &self,
        tenant: TenantId,
        hash: &BlobHash,
        generation: Gen,
        refs: u32,
    ) -> Result<()> {
        self.once(async move {
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
            crate::commits::commit(tx).await?;
            match (released, present) {
                (true, _) => Ok(()),
                (false, true) => Err(Error::Conflict),
                (false, false) => Err(Error::NotFound),
            }
        })
        .await
    }

    async fn gc_candidates(
        &self,
        older_than: OffsetDateTime,
        limit: usize,
    ) -> Result<Vec<BlobKey>> {
        match self.shard_views().await? {
            Some(views) => {
                for (_, view) in &views {
                    view.idempotent(|| prune_tombstones(view, limit)).await?;
                }
            }
            None => self.idempotent(|| prune_tombstones(self, limit)).await?,
        }
        Ok(self
            .zeroed(Some(older_than), limit)
            .await?
            .into_iter()
            .map(|(k, _)| k)
            .collect())
    }

    async fn next_gc_due(&self, window: Duration) -> Result<Option<OffsetDateTime>> {
        let window = time::Duration::try_from(window).unwrap_or(time::Duration::MAX);
        Ok(self
            .zeroed(None, 1)
            .await?
            .first()
            .map(|(_, z)| z.saturating_add(window)))
    }

    async fn mark_deleting(&self, key: &BlobKey, older_than: OffsetDateTime) -> Result<bool> {
        if let Some(views) = self.shard_views().await? {
            return mark_deleting_sharded(&views, key, older_than).await;
        }
        self.once(async move {
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
            crate::commits::commit(tx).await?;
            Ok(marked)
        })
        .await
    }

    async fn finalize_delete(&self, key: &BlobKey) -> Result<bool> {
        if let Some(views) = self.shard_views().await? {
            let mut done = false;
            for (_, view) in &views {
                done |= BlobIndex::finalize_delete(view, key).await?;
            }
            return Ok(done);
        }
        self.once(async move {
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
            crate::commits::commit(tx).await?;
            Ok(done)
        })
        .await
    }

    async fn exists(&self, key: &BlobKey) -> Result<bool> {
        if let Some(views) = self.shard_views().await? {
            for (_, view) in &views {
                if BlobIndex::exists(view, key).await? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        self.idempotent(|| async move {
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
        crate::commits::commit(tx).await?;
        Ok(found)
}).await
    }

    async fn compression(&self, key: &BlobKey) -> Result<Option<Compression>> {
        if let Some(views) = self.shard_views().await? {
            for (_, view) in &views {
                if let Some(c) = BlobIndex::compression(view, key).await? {
                    return Ok(Some(c));
                }
            }
            return Ok(None);
        }
        self.idempotent(|| async move {
            let key = *key;
            let found: Option<String> = with_tenant(&self.pool, key.tenant, move |tx| {
                Box::pin(async move {
                    sqlx::query_scalar(
                        "SELECT compression FROM blobs
                     WHERE tenant_id = $1 AND hash = $2 AND gen = $3 AND state <> 'deleted'",
                    )
                    .bind(key.tenant.as_uuid())
                    .bind(key.hash.as_bytes().as_slice())
                    .bind(i64::from(key.generation.0))
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db_err)
                })
            })
            .await?;
            found.as_deref().map(compression_of).transpose()
        })
        .await
    }
}
