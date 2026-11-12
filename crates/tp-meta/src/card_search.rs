//! Пересчёт поисковых строк контактов, записанных до миграции 0030 (META-78; CTR-54, DAV-24).
//!
//! `addressbook-query` ищет по `dav_objects.search` ([`DavStore::find_cards`](tp_contracts::dav::DavStore)); строки
//! считает вызывающий из тела при записи. У карточек, записанных до 0030, строк нет — признак `search_pending`
//! (миграция 0033). Строки считает функция узла (`tp_dav::index::card`): у `tp-meta` нет зависимости от `tp-dav`, а
//! второй разбор vCard здесь разошёлся бы с правилом индекса.
//!
//! Пачка читается короткой транзакцией, строки считаются вне транзакции, каждая карточка обновляется своим запросом
//! при неизменном `modseq`: параллельная запись клиента выигрывает, взаимных блокировок с изменениями ящика нет.
//! `ModSeq` и ETag не меняются — тело то же.

use tp_contracts::Result;
use tp_contracts::dav::CardSearch;
use uuid::Uuid;

use crate::PgMeta;
use crate::pool::{DbPool, with_system};
use crate::schema::db_err;

/// Непосчитанная карточка пачки.
#[derive(sqlx::FromRow)]
struct Pending {
    tenant_id: Uuid,
    account_id: Uuid,
    object_id: Uuid,
    modseq: i64,
    body: String,
}

/// Позиция пачки: ключ последней прочитанной карточки.
type Cursor = (Uuid, Uuid, Uuid);

impl PgMeta {
    /// Пересчитывает поисковые строки карточек без них (записанных до миграции 0030) на всех шардах: строки тела
    /// считает `index`; `None` (тело не разбирается) — карточка остаётся без строк, но больше не пересчитывается.
    /// Пачки — `meta.dav.card_reindex_batch` карточек, между ними пауза `meta.dav.card_reindex_pause`. Возвращает
    /// число пересчитанных карточек; повторный запуск — 0 без чтения тел. Задание узла: один раз в фоне после
    /// запуска; прерванный пересчёт продолжается следующим запуском.
    pub async fn reindex_cards<F>(&self, index: F) -> Result<u64>
    where
        F: Fn(&str) -> Option<CardSearch> + Sync,
    {
        let index = &index;
        self.rewrite(|| async move {
            let mut total = 0u64;
            for db in self.all_shards().await? {
                total += self.reindex_shard(&db.pool, index).await?;
            }
            if total > 0 {
                tracing::info!(cards = total, "поисковые строки контактов пересчитаны");
            }
            Ok(total)
        })
        .await
    }

    async fn reindex_shard<F>(&self, pool: &DbPool, index: &F) -> Result<u64>
    where
        F: Fn(&str) -> Option<CardSearch> + Sync,
    {
        let batch = i64::try_from(self.cfg.dav.card_reindex_batch.max(1)).unwrap_or(i64::MAX);
        let mut cursor: Cursor = (Uuid::nil(), Uuid::nil(), Uuid::nil());
        let mut total = 0u64;
        loop {
            let rows = pending(pool, cursor, batch).await?;
            let Some(last) = rows.last() else {
                return Ok(total);
            };
            cursor = (last.tenant_id, last.account_id, last.object_id);
            let full = i64::try_from(rows.len()).unwrap_or(i64::MAX) == batch;
            for row in rows {
                let search = index(&row.body).filter(|s| s.validate().is_ok());
                if store(pool, &row, search.as_ref()).await? && search.is_some() {
                    total += 1;
                }
            }
            if !full {
                return Ok(total);
            }
            tokio::time::sleep(self.cfg.dav.card_reindex_pause).await;
        }
    }
}

/// Следующие после `cursor` непосчитанные карточки шарда, не больше `batch` (частичный индекс
/// `dav_objects_search_pending`).
async fn pending(pool: &DbPool, cursor: Cursor, batch: i64) -> Result<Vec<Pending>> {
    with_system(pool, move |tx| {
        Box::pin(async move {
            sqlx::query_as(
                "SELECT tenant_id, account_id, object_id, modseq, body FROM dav_objects
                 WHERE search_pending AND component = 'VCARD' AND (tenant_id, account_id, object_id) > ($1, $2, $3)
                 ORDER BY tenant_id, account_id, object_id LIMIT $4",
            )
            .bind(cursor.0)
            .bind(cursor.1)
            .bind(cursor.2)
            .bind(batch)
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)
        })
    })
    .await
}

/// Записывает строки карточки `row` (`None` — прежние) и снимает признак, если карточку не перезаписали после
/// чтения. Возвращает `true`, если строка обновлена.
async fn store(pool: &DbPool, row: &Pending, search: Option<&CardSearch>) -> Result<bool> {
    let key = (row.tenant_id, row.account_id, row.object_id, row.modseq);
    let strings = search.map(|s| s.strings.clone());
    with_system(pool, move |tx| {
        Box::pin(async move {
            let done = sqlx::query(
                "UPDATE dav_objects SET search = COALESCE($5, search), search_pending = false
                 WHERE tenant_id = $1 AND account_id = $2 AND object_id = $3 AND modseq = $4 AND search_pending",
            )
            .bind(key.0)
            .bind(key.1)
            .bind(key.2)
            .bind(key.3)
            .bind(strings)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            Ok(done.rows_affected() == 1)
        })
    })
    .await
}
