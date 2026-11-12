//! Календари и контакты на PostgreSQL (CONTRACTS.md#Dav; META-46…48).
//!
//! Таблицы ящика `dav_collections`, `dav_objects`, `dav_tombstones` — на шарде учётной записи. Изменяющая операция
//! блокирует строку `mailbox_state` (счётчик `ModSeq`, общий с письмами) до фиксации: номера изменений видны по
//! порядку, проверки лимитов и UID не гоняются с параллельной записью. ETag — `ModSeq` записи.
//!
//! Надгробия старше `meta.dav.tombstone_ttl` удаляет [`PgMeta::purge_dav_tombstones`] (роль maintenance); граница
//! очищенных — по виду (`mailbox_state.dav_*_floor`, для `collection_changes` и `account_changes`) и по коллекции
//! (`dav_collections.tombstone_floor`, для `changes`): `since` ниже неё — `Conflict` (RFC 6578 `valid-sync-token`).
//! Токен полной синхронизации — состояние вида или коллекции — не ниже своей границы.

use std::collections::{BTreeSet, HashMap};
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection, PgPool};
use time::OffsetDateTime;
use tp_contracts::config::duration;
use tp_contracts::dav::{
    Collection, CollectionId, CollectionKind, Component, DavLimits, DavObject, DavObjectId,
    DavStore, DeleteOutcome, ObjectIndex, ObjectRef, Precondition, Put, PutOutcome, Span,
    TimeRange, dav_changed, valid_name,
};
use tp_contracts::meta::{AccountScope, Changes};
use tp_contracts::{AccountId, Error, ModSeq, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::changelog::lock_modseq;
use crate::mailboxes::ensure_state;
use crate::pool::with_system;
use crate::schema::{begin, db_err, to_i64, to_u32, to_u64};
use crate::shards::ShardId;

/// Секция `meta.dav`: лимиты хранилища ([`DavLimits`]) и срок надгробий.
///
/// ```yaml
/// meta:
///   dav:
///     max_object_size: 1048576
///     max_objects: 20000
///     max_collections: 100
///     busy_horizon: 400d
///     tombstone_ttl: 90d
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DavConfig {
    /// Размер тела объекта, байт.
    pub max_object_size: usize,
    /// Объектов в коллекции.
    pub max_objects: usize,
    /// Коллекций учётной записи (всех видов).
    pub max_collections: usize,
    /// Горизонт индекса занятости от момента записи объекта ([`DavLimits::busy_horizon`]).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub busy_horizon: Duration,
    /// Срок хранения надгробий удалённых объектов и коллекций: токен синхронизации старше очищенных — полная
    /// синхронизация клиента.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub tombstone_ttl: Duration,
}

impl DavConfig {
    /// Лимиты хранилища.
    pub fn limits(&self) -> DavLimits {
        DavLimits {
            max_object_size: self.max_object_size,
            max_objects: self.max_objects,
            max_collections: self.max_collections,
            busy_horizon: self.busy_horizon,
        }
    }

    /// Секция с лимитами `limits` и сроком надгробий по умолчанию.
    pub fn with_limits(limits: DavLimits) -> Self {
        Self {
            max_object_size: limits.max_object_size,
            max_objects: limits.max_objects,
            max_collections: limits.max_collections,
            busy_horizon: limits.busy_horizon,
            ..Self::default()
        }
    }
}

impl Default for DavConfig {
    /// Лимиты [`DavLimits::default`], надгробия — 90 суток.
    fn default() -> Self {
        let l = DavLimits::default();
        Self {
            max_object_size: l.max_object_size,
            max_objects: l.max_objects,
            max_collections: l.max_collections,
            busy_horizon: l.busy_horizon,
            tombstone_ttl: Duration::from_secs(90 * 24 * 3600),
        }
    }
}

fn kind_str(kind: CollectionKind) -> &'static str {
    match kind {
        CollectionKind::Calendar => "calendar",
        CollectionKind::AddressBook => "address_book",
    }
}

fn parse_kind(s: &str) -> Result<CollectionKind> {
    match s {
        "calendar" => Ok(CollectionKind::Calendar),
        "address_book" => Ok(CollectionKind::AddressBook),
        other => Err(Error::internal(format!("вид коллекции в БД: {other:?}"))),
    }
}

fn modseq(v: i64) -> Result<ModSeq> {
    to_u64(v).map(ModSeq)
}

fn arg(m: ModSeq) -> Result<i64> {
    to_i64(m.0)
}

/// `LIMIT` выборки страницы: на одну строку больше `limit` (признак `has_more`).
fn fetch_limit(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX).min(i64::MAX - 1) + 1
}

const COLLECTION_COLUMNS: &str = "collection_id, kind, name, display_name, description, color, sort_order, \
                                  timezone, components, modseq";

#[derive(Debug, FromRow)]
struct CollectionRow {
    collection_id: Uuid,
    kind: String,
    name: String,
    display_name: String,
    description: Option<String>,
    color: Option<String>,
    sort_order: i64,
    timezone: Option<String>,
    components: Vec<String>,
    modseq: i64,
}

impl CollectionRow {
    fn into_collection(self) -> Result<Collection> {
        let components = self
            .components
            .iter()
            .map(|c| c.parse::<Component>())
            .collect::<Result<BTreeSet<_>>>()?;
        Ok(Collection {
            id: CollectionId::from_uuid(self.collection_id),
            kind: parse_kind(&self.kind)?,
            name: self.name,
            display_name: self.display_name,
            description: self.description,
            color: self.color,
            order: to_u32(self.sort_order)?,
            timezone: self.timezone,
            components,
            // Прозрачность календаря хранит META-51.
            transparent: false,
            state: modseq(self.modseq)?,
        })
    }
}

const OBJECT_COLUMNS: &str =
    "object_id, collection_id, name, etag, body, component, uid, span_start, span_end";

#[derive(Debug, FromRow)]
struct ObjectRow {
    object_id: Uuid,
    collection_id: Uuid,
    name: String,
    etag: String,
    body: String,
    component: String,
    uid: String,
    span_start: Option<OffsetDateTime>,
    span_end: Option<OffsetDateTime>,
}

impl ObjectRow {
    fn into_object(self) -> Result<DavObject> {
        Ok(DavObject {
            id: DavObjectId::from_uuid(self.object_id),
            collection: CollectionId::from_uuid(self.collection_id),
            name: self.name,
            etag: self.etag,
            body: self.body,
            // Индекс занятости и `Schedule-Tag` хранит META-51.
            index: ObjectIndex::new(
                self.component.parse()?,
                self.uid,
                self.span_start.map(|start| Span {
                    start,
                    end: self.span_end,
                }),
            ),
        })
    }
}

/// Коллекция учётной записи по идентификатору.
async fn fetch_collection(
    tx: &mut PgConnection,
    scope: AccountScope,
    id: CollectionId,
) -> Result<Option<Collection>> {
    let row: Option<CollectionRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLLECTION_COLUMNS} FROM dav_collections
         WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3"
    )))
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(id.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    row.map(CollectionRow::into_collection).transpose()
}

/// Коллекция есть; иначе — `NotFound`.
async fn require_collection(
    tx: &mut PgConnection,
    scope: AccountScope,
    id: CollectionId,
) -> Result<Collection> {
    fetch_collection(tx, scope, id)
        .await?
        .ok_or(Error::NotFound)
}

/// Объекты коллекции по условию `filter` (параметры с `$4`) по имени побайтно.
async fn select_objects<'q>(
    tx: &mut PgConnection,
    scope: AccountScope,
    collection: CollectionId,
    filter: &str,
    bind: impl FnOnce(
        sqlx::query::QueryAs<'q, sqlx::Postgres, ObjectRow, sqlx::postgres::PgArguments>,
    ) -> sqlx::query::QueryAs<
        'q,
        sqlx::Postgres,
        ObjectRow,
        sqlx::postgres::PgArguments,
    >,
) -> Result<Vec<DavObject>> {
    let q = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {OBJECT_COLUMNS} FROM dav_objects
         WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3 AND ({filter})
         ORDER BY name COLLATE \"C\""
    )))
    .bind(*scope.tenant.as_uuid())
    .bind(*scope.account.as_uuid())
    .bind(*collection.as_uuid());
    let rows: Vec<ObjectRow> = bind(q).fetch_all(&mut *tx).await.map_err(db_err)?;
    rows.into_iter().map(ObjectRow::into_object).collect()
}

/// Граница очищенных надгробий: вида учётной записи или коллекции.
#[derive(Debug, Clone, Copy)]
enum Floor {
    Kind(CollectionKind),
    Collection(CollectionId),
}

/// `since` больше счётчика учётной записи или старше очищенных надгробий `floor` — `Conflict`; коллекция удалена
/// после выборки — `NotFound`. Вызывается после выборки изменений: очистка, зафиксированная между запросами,
/// поднимет границу, а не потеряет надгробия молча.
async fn check_since(
    tx: &mut PgConnection,
    scope: AccountScope,
    since: ModSeq,
    floor: Floor,
) -> Result<()> {
    let (calendar, collection) = match floor {
        Floor::Kind(kind) => (kind == CollectionKind::Calendar, None),
        Floor::Collection(id) => (false, Some(*id.as_uuid())),
    };
    let row: Option<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT s.modseq,
                CASE WHEN $4::uuid IS NOT NULL THEN
                         (SELECT c.tombstone_floor FROM dav_collections c
                          WHERE c.tenant_id = $1 AND c.account_id = $2 AND c.collection_id = $4)
                     WHEN $3 THEN s.dav_calendar_floor
                     ELSE s.dav_addressbook_floor END
         FROM mailbox_state s WHERE s.tenant_id = $1 AND s.account_id = $2",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(calendar)
    .bind(collection)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    let (counter, floor) = match row {
        Some((counter, Some(floor))) => (modseq(counter)?, modseq(floor)?),
        Some((_, None)) => return Err(Error::NotFound),
        None => (ModSeq::ZERO, ModSeq::ZERO),
    };
    if since > counter || (since != ModSeq::ZERO && since < floor) {
        return Err(Error::Conflict);
    }
    Ok(())
}

/// Состояние вида `kind`.
async fn kind_state(
    tx: &mut PgConnection,
    scope: AccountScope,
    kind: CollectionKind,
) -> Result<ModSeq> {
    let column = match kind {
        CollectionKind::Calendar => "dav_calendar_state",
        CollectionKind::AddressBook => "dav_addressbook_state",
    };
    let v: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT {column} FROM mailbox_state WHERE tenant_id = $1 AND account_id = $2"
    )))
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    modseq(v.unwrap_or(0))
}

/// Счётчик учётной записи становится `to` (изменения вида `kind` с номерами до `to`); строка уже заблокирована
/// [`lock_modseq`].
async fn advance(
    tx: &mut PgConnection,
    scope: AccountScope,
    kind: CollectionKind,
    to: ModSeq,
) -> Result<()> {
    sqlx::query(
        "UPDATE mailbox_state SET modseq = $3,
             dav_calendar_state = CASE WHEN $4 THEN $3 ELSE dav_calendar_state END,
             dav_addressbook_state = CASE WHEN $4 THEN dav_addressbook_state ELSE $3 END
         WHERE tenant_id = $1 AND account_id = $2",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(arg(to)?)
    .bind(kind == CollectionKind::Calendar)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Строка изменения: идентификатор, `ModSeq`, создан после `since`, удалён.
type ChangeRow<T> = (T, i64, bool, bool);

/// Страница изменений из выборки по `ModSeq` (не больше `limit + 1` строк). Полный ответ: `new_state` — наибольшее
/// из `since`, `complete` и номеров выборки.
fn page<T>(
    mut rows: Vec<ChangeRow<T>>,
    since: ModSeq,
    limit: usize,
    complete: ModSeq,
) -> Result<Changes<T>> {
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let last = match rows.last() {
        Some(r) => modseq(r.1)?,
        None => since,
    };
    let mut out = Changes {
        created: Vec::new(),
        updated: Vec::new(),
        destroyed: Vec::new(),
        new_state: if has_more {
            last
        } else {
            complete.max(since).max(last)
        },
        has_more,
    };
    for (id, _, created, destroyed) in rows {
        if destroyed {
            out.destroyed.push(id);
        } else if created {
            out.created.push(id);
        } else {
            out.updated.push(id);
        }
    }
    Ok(out)
}

impl PgMeta {
    async fn dav_signal(
        &self,
        scope: AccountScope,
        kind: CollectionKind,
        collection: CollectionId,
        modseq: ModSeq,
    ) {
        self.signal(dav_changed(scope, kind, collection, modseq))
            .await;
    }

    /// Удаляет надгробия DAV старше `meta.dav.tombstone_ttl` на всех шардах (задание роли maintenance) и поднимает
    /// границу очищенных: прежние токены синхронизации этих учётных записей — `Conflict`. Возвращает число
    /// удалённых надгробий.
    pub async fn purge_dav_tombstones(&self) -> Result<u64> {
        let ttl = time::Duration::try_from(self.cfg.dav.tombstone_ttl)
            .map_err(|_| Error::invalid("meta.dav.tombstone_ttl вне диапазона"))?;
        let before = OffsetDateTime::now_utc()
            .checked_sub(ttl)
            .ok_or_else(|| Error::invalid("meta.dav.tombstone_ttl вне диапазона"))?;
        self.purge_dav_tombstones_before(before).await
    }

    /// Удаляет надгробия DAV, удалённые раньше `before`, на всех шардах (см. [`PgMeta::purge_dav_tombstones`]).
    /// Каждая учётная запись — своей транзакцией под блокировкой строки состояния ящика, как изменения DAV:
    /// порядок блокировок тот же, границы поднимаются вместе с удалением.
    pub async fn purge_dav_tombstones_before(&self, before: OffsetDateTime) -> Result<u64> {
        // Повтор после обрыва удаляет только оставшееся: идемпотентно по сути.
        self.rewrite(|| async move {
            let mut total = 0u64;
            for db in self.all_shards().await? {
                let accounts: Vec<(Uuid, Uuid)> = with_system(&db.pool, move |tx| {
                    Box::pin(async move {
                        sqlx::query_as(
                            "SELECT DISTINCT tenant_id, account_id FROM dav_tombstones WHERE deleted_at < $1",
                        )
                        .bind(before)
                        .fetch_all(&mut *tx)
                        .await
                        .map_err(db_err)
                    })
                })
                .await?;
                for (tenant, account) in accounts {
                    let scope = AccountScope {
                        tenant: TenantId::from_uuid(tenant),
                        account: AccountId::from_uuid(account),
                    };
                    total += purge_account(&db.pool, scope, before).await?;
                }
            }
            Ok(total)
        })
        .await
    }
}

/// Очистка надгробий учётной записи `scope` старше `before` с подъёмом границ вида и коллекций.
async fn purge_account(pool: &PgPool, scope: AccountScope, before: OffsetDateTime) -> Result<u64> {
    let mut tx = begin(pool, scope.tenant).await?;
    lock_modseq(&mut tx, scope).await?;
    let n: i64 = sqlx::query_scalar(
        "WITH gone AS (
             DELETE FROM dav_tombstones WHERE tenant_id = $1 AND account_id = $2 AND deleted_at < $3
             RETURNING kind, collection_id, modseq
         ), by_collection AS (
             UPDATE dav_collections c SET tombstone_floor = greatest(c.tombstone_floor, g.floor)
             FROM (SELECT collection_id, max(modseq) AS floor FROM gone GROUP BY collection_id) g
             WHERE c.tenant_id = $1 AND c.account_id = $2 AND c.collection_id = g.collection_id
         ), by_kind AS (
             UPDATE mailbox_state s SET
                 dav_calendar_floor = greatest(s.dav_calendar_floor,
                     coalesce((SELECT max(modseq) FROM gone WHERE kind = 'calendar'), 0)),
                 dav_addressbook_floor = greatest(s.dav_addressbook_floor,
                     coalesce((SELECT max(modseq) FROM gone WHERE kind = 'address_book'), 0))
             WHERE s.tenant_id = $1 AND s.account_id = $2
         )
         SELECT count(*) FROM gone",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(before)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err)?;
    crate::commits::commit(tx).await?;
    to_u64(n)
}

#[async_trait]
impl DavStore for PgMeta {
    fn limits(&self) -> DavLimits {
        self.cfg.dav.limits()
    }

    async fn list_collections(
        &self,
        scope: AccountScope,
        kind: CollectionKind,
    ) -> Result<Vec<Collection>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            let rows: Vec<CollectionRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {COLLECTION_COLUMNS} FROM dav_collections
                 WHERE tenant_id = $1 AND account_id = $2 AND kind = $3
                 ORDER BY sort_order, name COLLATE \"C\""
            )))
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(kind_str(kind))
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            rows.into_iter()
                .map(CollectionRow::into_collection)
                .collect()
        })
        .await
    }

    async fn get_collection(
        &self,
        scope: AccountScope,
        id: CollectionId,
    ) -> Result<Option<Collection>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            let c = fetch_collection(&mut tx, scope, id).await?;
            crate::commits::commit(tx).await?;
            Ok(c)
        })
        .await
    }

    async fn collection_by_name(
        &self,
        scope: AccountScope,
        kind: CollectionKind,
        name: &str,
    ) -> Result<Option<Collection>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            let row: Option<CollectionRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {COLLECTION_COLUMNS} FROM dav_collections
                 WHERE tenant_id = $1 AND account_id = $2 AND kind = $3 AND name = $4"
            )))
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(kind_str(kind))
            .bind(name)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            row.map(CollectionRow::into_collection).transpose()
        })
        .await
    }

    async fn create_collection(
        &self,
        scope: AccountScope,
        collection: Collection,
    ) -> Result<ModSeq> {
        collection.validate()?;
        let limit = i64::try_from(self.limits().max_collections).unwrap_or(i64::MAX);
        let modseq = self
            .once(async {
                let db = self.box_db(scope).await?;
                let mut tx = begin(&db.pool, scope.tenant).await?;
                if db.shard != ShardId::MAIN {
                    // Учётная запись есть в каталоге (шард найден), состояния ящика на шарде может ещё не быть.
                    ensure_state(&mut tx, scope).await?;
                }
                let cur = lock_modseq(&mut tx, scope).await?;
                let (taken, count): (bool, i64) = sqlx::query_as(
                    "SELECT EXISTS (SELECT 1 FROM dav_collections
                                    WHERE tenant_id = $1 AND account_id = $2
                                      AND (collection_id = $3 OR (kind = $4 AND name = $5)))
                            OR EXISTS (SELECT 1 FROM dav_tombstones
                                       WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3),
                            (SELECT count(*) FROM dav_collections WHERE tenant_id = $1 AND account_id = $2)",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(collection.id.as_uuid())
                .bind(kind_str(collection.kind))
                .bind(&collection.name)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
                if taken {
                    return Err(Error::Conflict);
                }
                if count >= limit {
                    return Err(Error::QuotaExceeded);
                }
                let ms = cur.next();
                let components: Vec<&str> =
                    collection.components.iter().map(|c| c.as_str()).collect();
                sqlx::query(
                    "INSERT INTO dav_collections (tenant_id, account_id, collection_id, kind, name, display_name,
                                                  description, color, sort_order, timezone, components,
                                                  created_modseq, props_modseq, modseq)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $12, $12)",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(collection.id.as_uuid())
                .bind(kind_str(collection.kind))
                .bind(&collection.name)
                .bind(&collection.display_name)
                .bind(&collection.description)
                .bind(&collection.color)
                .bind(i64::from(collection.order))
                .bind(&collection.timezone)
                .bind(&components)
                .bind(arg(ms)?)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                advance(&mut tx, scope, collection.kind, ms).await?;
                crate::commits::commit(tx).await?;
                Ok(ms)
            })
            .await?;
        self.dav_signal(scope, collection.kind, collection.id, modseq)
            .await;
        Ok(modseq)
    }

    async fn update_collection(
        &self,
        scope: AccountScope,
        collection: Collection,
    ) -> Result<ModSeq> {
        collection.validate()?;
        let modseq = self
            .once(async {
                let mut tx = begin(&self.box_pool(scope).await?, scope.tenant).await?;
                let cur = lock_modseq(&mut tx, scope).await?;
                let now = require_collection(&mut tx, scope, collection.id).await?;
                if now.kind != collection.kind || now.name != collection.name {
                    return Err(Error::invalid("вид и имя коллекции не меняются"));
                }
                let ms = cur.next();
                let components: Vec<&str> =
                    collection.components.iter().map(|c| c.as_str()).collect();
                sqlx::query(
                    "UPDATE dav_collections SET display_name = $4, description = $5, color = $6, sort_order = $7,
                                                timezone = $8, components = $9, props_modseq = $10, modseq = $10
                     WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(collection.id.as_uuid())
                .bind(&collection.display_name)
                .bind(&collection.description)
                .bind(&collection.color)
                .bind(i64::from(collection.order))
                .bind(&collection.timezone)
                .bind(&components)
                .bind(arg(ms)?)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                advance(&mut tx, scope, collection.kind, ms).await?;
                crate::commits::commit(tx).await?;
                Ok(ms)
            })
            .await?;
        self.dav_signal(scope, collection.kind, collection.id, modseq)
            .await;
        Ok(modseq)
    }

    async fn delete_collection(&self, scope: AccountScope, id: CollectionId) -> Result<ModSeq> {
        let (kind, modseq) = self
            .once(async {
                let mut tx = begin(&self.box_pool(scope).await?, scope.tenant).await?;
                let cur = lock_modseq(&mut tx, scope).await?;
                let c = require_collection(&mut tx, scope, id).await?;
                let kind = kind_str(c.kind);
                // Объекты — в надгробия, каждый со своим ModSeq по порядку имён; коллекция — последней.
                let n = sqlx::query(
                    "INSERT INTO dav_tombstones (tenant_id, account_id, modseq, kind, collection_id, object_id, name)
                     SELECT tenant_id, account_id, $4 + row_number() OVER (ORDER BY name COLLATE \"C\"), $5,
                            collection_id, object_id, name
                     FROM dav_objects WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(id.as_uuid())
                .bind(arg(cur)?)
                .bind(kind)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?
                .rows_affected();
                let ms = ModSeq(cur.0 + n + 1);
                sqlx::query(
                    "INSERT INTO dav_tombstones (tenant_id, account_id, modseq, kind, collection_id, object_id, name)
                     VALUES ($1, $2, $3, $4, $5, NULL, $6)",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(arg(ms)?)
                .bind(kind)
                .bind(id.as_uuid())
                .bind(&c.name)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                sqlx::query(
                    "DELETE FROM dav_collections WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(id.as_uuid())
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                advance(&mut tx, scope, c.kind, ms).await?;
                crate::commits::commit(tx).await?;
                Ok((c.kind, ms))
            })
            .await?;
        self.dav_signal(scope, kind, id, modseq).await;
        Ok(modseq)
    }

    async fn get_object(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        name: &str,
    ) -> Result<Option<DavObject>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            require_collection(&mut tx, scope, collection).await?;
            let mut out =
                select_objects(&mut tx, scope, collection, "name = $4", |q| q.bind(name)).await?;
            crate::commits::commit(tx).await?;
            Ok(out.pop())
        })
        .await
    }

    async fn object_by_uid(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        uid: &str,
    ) -> Result<Option<DavObject>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            require_collection(&mut tx, scope, collection).await?;
            let mut out =
                select_objects(&mut tx, scope, collection, "uid = $4", |q| q.bind(uid)).await?;
            crate::commits::commit(tx).await?;
            Ok(out.pop())
        })
        .await
    }

    async fn list_objects(
        &self,
        scope: AccountScope,
        collection: CollectionId,
    ) -> Result<Vec<ObjectRef>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            require_collection(&mut tx, scope, collection).await?;
            let rows: Vec<(Uuid, String, String)> = sqlx::query_as(
                "SELECT object_id, name, etag FROM dav_objects
                 WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3
                 ORDER BY name COLLATE \"C\"",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(collection.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(rows
                .into_iter()
                .map(|(id, name, etag)| ObjectRef {
                    id: DavObjectId::from_uuid(id),
                    name,
                    etag,
                })
                .collect())
        })
        .await
    }

    async fn multiget(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        names: &[String],
    ) -> Result<Vec<DavObject>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            require_collection(&mut tx, scope, collection).await?;
            let found = select_objects(&mut tx, scope, collection, "name = ANY($4)", |q| {
                q.bind(names)
            })
            .await?;
            crate::commits::commit(tx).await?;
            let by_name: HashMap<&str, &DavObject> =
                found.iter().map(|o| (o.name.as_str(), o)).collect();
            Ok(names
                .iter()
                .filter_map(|n| by_name.get(n.as_str()).map(|o| (*o).clone()))
                .collect())
        })
        .await
    }

    async fn objects_by_id(
        &self,
        scope: AccountScope,
        ids: &[DavObjectId],
    ) -> Result<Vec<DavObject>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let raw: Vec<Uuid> = ids.iter().map(|id| *id.as_uuid()).collect();
        self.idempotent(|| {
            let raw = raw.clone();
            async move {
                let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
                let rows: Vec<ObjectRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "SELECT {OBJECT_COLUMNS} FROM dav_objects
                     WHERE tenant_id = $1 AND account_id = $2 AND object_id = ANY($3)"
                )))
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(&raw)
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?;
                crate::commits::commit(tx).await?;
                let found: HashMap<Uuid, DavObject> = rows
                    .into_iter()
                    .map(|r| Ok((r.object_id, r.into_object()?)))
                    .collect::<Result<_>>()?;
                Ok(raw.iter().filter_map(|id| found.get(id).cloned()).collect())
            }
        })
        .await
    }

    async fn in_range(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        range: TimeRange,
    ) -> Result<Vec<DavObject>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            require_collection(&mut tx, scope, collection).await?;
            // Условие ObjectIndex::may_overlap: без интервала — всегда; иначе start < range.end и end ≥ range.start.
            let out = select_objects(
                &mut tx,
                scope,
                collection,
                "span_start IS NULL
                 OR (($5::timestamptz IS NULL OR span_start < $5)
                     AND (span_end IS NULL OR $4::timestamptz IS NULL OR span_end >= $4))",
                |q| q.bind(range.start).bind(range.end),
            )
            .await?;
            crate::commits::commit(tx).await?;
            Ok(out)
        })
        .await
    }

    async fn put_object(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        name: &str,
        body: String,
        index: ObjectIndex,
        pre: Precondition,
    ) -> Result<PutOutcome> {
        let limits = self.limits();
        let body = &body;
        let index = &index;
        let pre = &pre;
        let (kind, outcome) = self
            .once(async move {
                let mut tx = begin(&self.box_pool(scope).await?, scope.tenant).await?;
                let cur = lock_modseq(&mut tx, scope).await?;
                let c = require_collection(&mut tx, scope, collection).await?;
                if !valid_name(name) {
                    return Err(Error::invalid(format!(
                        "недопустимое имя ресурса: {name:?}"
                    )));
                }
                index.validate()?;
                if !c.components.contains(&index.component) {
                    return Err(Error::invalid(format!(
                        "компонент {} не поддерживается коллекцией",
                        index.component
                    )));
                }
                if body.len() > limits.max_object_size {
                    return Err(Error::QuotaExceeded);
                }
                let existing: Option<(Uuid, String)> = sqlx::query_as(
                    "SELECT object_id, etag FROM dav_objects
                     WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3 AND name = $4",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(collection.as_uuid())
                .bind(name)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
                let ok = match pre {
                    Precondition::Any => true,
                    Precondition::IfMatch(etag) => existing.as_ref().is_some_and(|e| e.1 == *etag),
                    Precondition::IfNoneMatch => existing.is_none(),
                };
                if !ok {
                    return Ok((c.kind, PutOutcome::PreconditionFailed));
                }
                let other: Option<String> = sqlx::query_scalar(
                    "SELECT name FROM dav_objects
                     WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3 AND uid = $4 AND name <> $5",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(collection.as_uuid())
                .bind(&index.uid)
                .bind(name)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
                if let Some(name) = other {
                    return Ok((c.kind, PutOutcome::UidConflict { name }));
                }
                let count: i32 = sqlx::query_scalar(
                    "SELECT object_count FROM dav_collections
                     WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(collection.as_uuid())
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
                if existing.is_none()
                    && usize::try_from(count).unwrap_or(usize::MAX) >= limits.max_objects
                {
                    return Err(Error::QuotaExceeded);
                }
                let ms = cur.next();
                let etag = ms.to_string();
                let (start, end) = match index.span {
                    Some(s) => (Some(s.start), s.end),
                    None => (None, None),
                };
                let id = match &existing {
                    Some((id, _)) => {
                        sqlx::query(
                            "UPDATE dav_objects SET uid = $4, etag = $5, component = $6, span_start = $7,
                                                    span_end = $8, body = $9, modseq = $10
                             WHERE tenant_id = $1 AND account_id = $2 AND object_id = $3",
                        )
                        .bind(scope.tenant.as_uuid())
                        .bind(scope.account.as_uuid())
                        .bind(id)
                        .bind(&index.uid)
                        .bind(&etag)
                        .bind(index.component.as_str())
                        .bind(start)
                        .bind(end)
                        .bind(body)
                        .bind(arg(ms)?)
                        .execute(&mut *tx)
                        .await
                        .map_err(db_err)?;
                        DavObjectId::from_uuid(*id)
                    }
                    None => {
                        let id = DavObjectId::new();
                        sqlx::query(
                            "INSERT INTO dav_objects (tenant_id, account_id, object_id, collection_id, name, uid,
                                                      etag, component, span_start, span_end, body,
                                                      created_modseq, modseq)
                             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $12)",
                        )
                        .bind(scope.tenant.as_uuid())
                        .bind(scope.account.as_uuid())
                        .bind(id.as_uuid())
                        .bind(collection.as_uuid())
                        .bind(name)
                        .bind(&index.uid)
                        .bind(&etag)
                        .bind(index.component.as_str())
                        .bind(start)
                        .bind(end)
                        .bind(body)
                        .bind(arg(ms)?)
                        .execute(&mut *tx)
                        .await
                        .map_err(db_err)?;
                        id
                    }
                };
                sqlx::query(
                    "UPDATE dav_collections SET modseq = $4, object_count = object_count + $5
                     WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(collection.as_uuid())
                .bind(arg(ms)?)
                .bind(i32::from(existing.is_none()))
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                advance(&mut tx, scope, c.kind, ms).await?;
                crate::commits::commit(tx).await?;
                Ok((
                    c.kind,
                    PutOutcome::Stored(Put {
                        id,
                        etag,
                        modseq: ms,
                        created: existing.is_none(),
                    }),
                ))
            })
            .await?;
        if let PutOutcome::Stored(p) = &outcome {
            self.dav_signal(scope, kind, collection, p.modseq).await;
        }
        Ok(outcome)
    }

    async fn delete_object(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        name: &str,
        pre: Precondition,
    ) -> Result<DeleteOutcome> {
        let pre = &pre;
        let (kind, outcome) = self
            .once(async move {
                let mut tx = begin(&self.box_pool(scope).await?, scope.tenant).await?;
                let cur = lock_modseq(&mut tx, scope).await?;
                let c = require_collection(&mut tx, scope, collection).await?;
                let existing: Option<(Uuid, String)> = sqlx::query_as(
                    "SELECT object_id, etag FROM dav_objects
                     WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3 AND name = $4",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(collection.as_uuid())
                .bind(name)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
                let object = match (pre, existing) {
                    (Precondition::Any | Precondition::IfNoneMatch, None) => {
                        return Err(Error::NotFound);
                    }
                    (Precondition::IfMatch(etag), Some((id, cur_etag))) if cur_etag == *etag => id,
                    (Precondition::IfMatch(_), _) | (Precondition::IfNoneMatch, Some(_)) => {
                        return Ok((c.kind, DeleteOutcome::PreconditionFailed));
                    }
                    (Precondition::Any, Some((id, _))) => id,
                };
                let ms = cur.next();
                sqlx::query(
                    "DELETE FROM dav_objects WHERE tenant_id = $1 AND account_id = $2 AND object_id = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(object)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                sqlx::query(
                    "INSERT INTO dav_tombstones (tenant_id, account_id, modseq, kind, collection_id, object_id, name)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(arg(ms)?)
                .bind(kind_str(c.kind))
                .bind(collection.as_uuid())
                .bind(object)
                .bind(name)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                sqlx::query(
                    "UPDATE dav_collections SET modseq = $4, object_count = object_count - 1
                     WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(collection.as_uuid())
                .bind(arg(ms)?)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                advance(&mut tx, scope, c.kind, ms).await?;
                crate::commits::commit(tx).await?;
                Ok((c.kind, DeleteOutcome::Deleted(ms)))
            })
            .await?;
        if let DeleteOutcome::Deleted(ms) = outcome {
            self.dav_signal(scope, kind, collection, ms).await;
        }
        Ok(outcome)
    }

    async fn changes(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<String>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            let c = require_collection(&mut tx, scope, collection).await?;
            let mut rows: Vec<ChangeRow<String>> = Vec::new();
            if since <= ModSeq(i64::MAX as u64) {
                // Существующие ресурсы после since и последние надгробия имён, которых сейчас нет.
                rows = sqlx::query_as(
                    "SELECT name, modseq, created_modseq > $4, false FROM dav_objects
                     WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3 AND modseq > $4
                     UNION ALL
                     SELECT t.name, max(t.modseq), false, true FROM dav_tombstones t
                     WHERE $4 > 0 AND t.tenant_id = $1 AND t.account_id = $2 AND t.collection_id = $3
                       AND t.object_id IS NOT NULL AND t.modseq > $4
                       AND NOT EXISTS (SELECT 1 FROM dav_objects o
                                       WHERE o.tenant_id = $1 AND o.account_id = $2
                                         AND o.collection_id = $3 AND o.name = t.name)
                     GROUP BY t.name
                     ORDER BY 2 LIMIT $5",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(collection.as_uuid())
                .bind(arg(since)?)
                .bind(fetch_limit(limit))
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?;
            }
            check_since(&mut tx, scope, since, Floor::Collection(collection)).await?;
            crate::commits::commit(tx).await?;
            page(rows, since, limit, c.state)
        })
        .await
    }

    async fn collection_changes(
        &self,
        scope: AccountScope,
        kind: CollectionKind,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<CollectionId>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            let mut rows: Vec<ChangeRow<Uuid>> = Vec::new();
            if since <= ModSeq(i64::MAX as u64) {
                rows = sqlx::query_as(
                    "SELECT collection_id, props_modseq, created_modseq > $4, false FROM dav_collections
                     WHERE tenant_id = $1 AND account_id = $2 AND kind = $3 AND props_modseq > $4
                     UNION ALL
                     SELECT collection_id, modseq, false, true FROM dav_tombstones
                     WHERE $4 > 0 AND tenant_id = $1 AND account_id = $2 AND kind = $3
                       AND object_id IS NULL AND modseq > $4
                     ORDER BY 2 LIMIT $5",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(kind_str(kind))
                .bind(arg(since)?)
                .bind(fetch_limit(limit))
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?;
            }
            let complete = kind_state(&mut tx, scope, kind).await?;
            check_since(&mut tx, scope, since, Floor::Kind(kind)).await?;
            crate::commits::commit(tx).await?;
            let rows = rows
                .into_iter()
                .map(|(id, m, c, d)| (CollectionId::from_uuid(id), m, c, d))
                .collect();
            page(rows, since, limit, complete)
        })
        .await
    }

    async fn account_changes(
        &self,
        scope: AccountScope,
        kind: CollectionKind,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<DavObjectId>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            let mut rows: Vec<ChangeRow<Uuid>> = Vec::new();
            if since <= ModSeq(i64::MAX as u64) {
                rows = sqlx::query_as(
                    "SELECT o.object_id, o.modseq, o.created_modseq > $4, false
                     FROM dav_objects o JOIN dav_collections c USING (tenant_id, account_id, collection_id)
                     WHERE o.tenant_id = $1 AND o.account_id = $2 AND c.kind = $3 AND o.modseq > $4
                     UNION ALL
                     SELECT object_id, modseq, false, true FROM dav_tombstones
                     WHERE $4 > 0 AND tenant_id = $1 AND account_id = $2 AND kind = $3
                       AND object_id IS NOT NULL AND modseq > $4
                     ORDER BY 2 LIMIT $5",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(kind_str(kind))
                .bind(arg(since)?)
                .bind(fetch_limit(limit))
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?;
            }
            let complete = kind_state(&mut tx, scope, kind).await?;
            check_since(&mut tx, scope, since, Floor::Kind(kind)).await?;
            crate::commits::commit(tx).await?;
            let rows = rows
                .into_iter()
                .map(|(id, m, c, d)| (DavObjectId::from_uuid(id), m, c, d))
                .collect();
            page(rows, since, limit, complete)
        })
        .await
    }

    async fn state(&self, scope: AccountScope, kind: CollectionKind) -> Result<ModSeq> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            let s = kind_state(&mut tx, scope, kind).await?;
            crate::commits::commit(tx).await?;
            Ok(s)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_from_yaml() {
        let c: DavConfig = serde_yaml_ng::from_str(
            "max_object_size: 4096\nmax_objects: 4\nmax_collections: 3\nbusy_horizon: 30d\ntombstone_ttl: 7d\n",
        )
        .expect("yaml");
        assert_eq!(
            c.limits(),
            DavLimits {
                max_object_size: 4096,
                max_objects: 4,
                max_collections: 3,
                busy_horizon: Duration::from_secs(30 * 24 * 3600),
            }
        );
        assert_eq!(c.tombstone_ttl, Duration::from_secs(7 * 24 * 3600));
        assert_eq!(DavConfig::default().limits(), DavLimits::default());
        assert!(serde_yaml_ng::from_str::<DavConfig>("max_object: 1\n").is_err());
    }

    #[test]
    fn pages() {
        let rows = vec![
            ("a".to_owned(), 3, true, false),
            ("b".to_owned(), 5, false, false),
            ("c".to_owned(), 7, false, true),
        ];
        let p = page(rows.clone(), ModSeq(2), 2, ModSeq(9)).expect("page");
        assert!(p.has_more);
        assert_eq!(p.new_state, ModSeq(5));
        assert_eq!(
            (p.created, p.updated),
            (vec!["a".to_owned()], vec!["b".to_owned()])
        );
        let p = page(rows, ModSeq(2), 3, ModSeq(9)).expect("page");
        assert!(!p.has_more);
        assert_eq!(p.new_state, ModSeq(9));
        assert_eq!(p.destroyed, ["c"]);
        let empty: Changes<String> = page(Vec::new(), ModSeq(4), 10, ModSeq(1)).expect("page");
        assert_eq!(empty.new_state, ModSeq(4));
        assert_eq!(fetch_limit(usize::MAX), i64::MAX);
        assert_eq!(fetch_limit(0), 1);
    }
}
