//! Календари, контакты и планирование встреч на PostgreSQL (CONTRACTS.md#Dav, CONTRACTS.md#Schedule; META-46…48,
//! META-51).
//!
//! Таблицы ящика `dav_collections`, `dav_objects`, `dav_tombstones`, `dav_busy`, `dav_inbox` — на шарде учётной записи. Изменяющая операция
//! блокирует строку `mailbox_state` (счётчик `ModSeq`, общий с письмами) до фиксации: номера изменений видны по
//! порядку, проверки лимитов и UID не гоняются с параллельной записью. ETag — `ModSeq` записи.
//!
//! Надгробия старше `meta.dav.tombstone_ttl` удаляет [`PgMeta::purge_dav_tombstones`] (роль maintenance); граница
//! очищенных — по виду (`mailbox_state.dav_*_floor`, для `collection_changes` и `account_changes`) и по коллекции
//! (`dav_collections.tombstone_floor`, для `changes`): `since` ниже неё — `Conflict` (RFC 6578 `valid-sync-token`).
//! Токен полной синхронизации — состояние вида или коллекции — не ниже своей границы.
//!
//! Планирование (CTR-47): индекс занятости объекта — строки `dav_busy`, заменяются при записи объекта и удаляются
//! вместе с ним; [`FreeBusy::query`] читает их одним запросом на шард по учётным записям и интервалу. У
//! повторяющихся событий (`dav_objects.recurring`; META-54, решение 59) строк индекса нет: тем же запросом читаются
//! их тела, повторы разворачивает calcard вне транзакции ([`crate::busy`]). Входящие
//! планирования — `dav_inbox`: номера изменений из того же счётчика, но состояние календарей они не меняют
//! (`mailbox_state.dav_inbox_state`). Журнал входящих (CTR-65, META-69) — `dav_inbox.created_modseq` и надгробия
//! `dav_inbox_tombstones`; их очищает тот же [`PgMeta::purge_dav_tombstones`], граница —
//! `mailbox_state.dav_inbox_floor`.
//!
//! Книга GAL организации (CTR-60) — в основной БД, в тех же таблицах, под служебной «учётной записью» организации
//! (`gal_scope`: идентификатор — идентификатор организации) со своей строкой `mailbox_state` (счётчик `ModSeq`
//! книги, границы надгробий) и единственной коллекцией — адресной книгой `gal`. Её строки читаются по коллекции:
//! коллекции нет среди своих — она ищется среди книги GAL организации (`PgMeta::open_visible`); запись
//! пользователя в неё — `Forbidden`. [`DavStore::sync_gal`] заменяет набор одной транзакцией: неизменные карточки не
//! трогаются, исчезнувшие — в надгробия, остальные — записью с новым `ModSeq` и ETag.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgConnection, Postgres, Transaction};
use time::OffsetDateTime;
use tp_contracts::config::duration;
use tp_contracts::dav::{
    CardSearch, Collection, CollectionId, CollectionKind, Component, DavLimits, DavObject,
    DavObjectId, DavStore, DeleteOutcome, GAL_COLLECTION_NAME, GalCard, MoveOutcome, Moved,
    ObjectIndex, ObjectRef, Precondition, Put, PutOutcome, Span, TimeRange, dav_changed,
    gal_changed, valid_name, validate_gal,
};
use tp_contracts::meta::{AccountScope, Changes};
use tp_contracts::schedule::{
    BusyKind, BusyPeriod, FreeBusy, Inbox, InboxItem, InboxMessage, Period, merge_busy,
    schedule_inbox_changed,
};
use tp_contracts::{AccountId, Error, ModSeq, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::changelog::lock_modseq;
use crate::mailboxes::ensure_state;
use crate::pool::with_system;
use crate::schema::{begin, db_err, to_i64, to_u32, to_u64};
use crate::shards::ShardId;

/// Секция `meta.dav`: лимиты хранилища ([`DavLimits`]), срок надгробий и пересчёт поисковых строк контактов
/// ([`PgMeta::reindex_cards`]).
///
/// ```yaml
/// meta:
///   dav:
///     max_object_size: 1048576
///     max_objects: 20000
///     max_collections: 100
///     max_busy_instances: 20000
///     tombstone_ttl: 90d
///     card_reindex_batch: 100
///     card_reindex_pause: 100ms
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
    /// Вхождений одного повторяющегося события, разворачиваемых при запросе занятости, считая от начала повторов
    /// ([`DavLimits::max_busy_instances`]).
    pub max_busy_instances: usize,
    /// Срок хранения надгробий удалённых объектов и коллекций: токен синхронизации старше очищенных — полная
    /// синхронизация клиента.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub tombstone_ttl: Duration,
    /// Карточек в пачке пересчёта поисковых строк ([`PgMeta::reindex_cards`]).
    pub card_reindex_batch: usize,
    /// Пауза между пачками пересчёта: узел обслуживает клиентов во время пересчёта.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub card_reindex_pause: Duration,
}

impl DavConfig {
    /// Лимиты хранилища.
    pub fn limits(&self) -> DavLimits {
        DavLimits {
            max_object_size: self.max_object_size,
            max_objects: self.max_objects,
            max_collections: self.max_collections,
            max_busy_instances: self.max_busy_instances,
        }
    }

    /// Секция с лимитами `limits` и сроком надгробий по умолчанию.
    pub fn with_limits(limits: DavLimits) -> Self {
        Self {
            max_object_size: limits.max_object_size,
            max_objects: limits.max_objects,
            max_collections: limits.max_collections,
            max_busy_instances: limits.max_busy_instances,
            ..Self::default()
        }
    }
}

impl Default for DavConfig {
    /// Лимиты [`DavLimits::default`], надгробия — 90 суток, пересчёт — по 100 карточек с паузой 100 мс.
    fn default() -> Self {
        let l = DavLimits::default();
        Self {
            max_object_size: l.max_object_size,
            max_objects: l.max_objects,
            max_collections: l.max_collections,
            max_busy_instances: l.max_busy_instances,
            tombstone_ttl: Duration::from_secs(90 * 24 * 3600),
            card_reindex_batch: 100,
            card_reindex_pause: Duration::from_millis(100),
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

fn busy_kind_str(kind: BusyKind) -> &'static str {
    match kind {
        BusyKind::Busy => "busy",
        BusyKind::Tentative => "tentative",
        BusyKind::Unavailable => "unavailable",
    }
}

fn parse_busy_kind(s: &str) -> Result<BusyKind> {
    match s {
        "busy" => Ok(BusyKind::Busy),
        "tentative" => Ok(BusyKind::Tentative),
        "unavailable" => Ok(BusyKind::Unavailable),
        other => Err(Error::internal(format!("вид занятости в БД: {other:?}"))),
    }
}

const COLLECTION_COLUMNS: &str = "collection_id, kind, name, display_name, description, color, sort_order, \
                                  timezone, components, transparent, is_default, subscribed, visible, \
                                  alerts_with_time, alerts_without_time, modseq";

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
    transparent: bool,
    is_default: bool,
    subscribed: bool,
    visible: bool,
    alerts_with_time: Option<String>,
    alerts_without_time: Option<String>,
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
            transparent: self.transparent,
            is_default: self.is_default,
            subscribed: self.subscribed,
            visible: self.visible,
            alerts_with_time: self.alerts_with_time,
            alerts_without_time: self.alerts_without_time,
            state: modseq(self.modseq)?,
        })
    }
}

const OBJECT_COLUMNS: &str = "object_id, collection_id, name, etag, body, component, uid, span_start, span_end, \
                              schedule_tag, search, recurring, busy_starts, busy_ends, busy_kinds";

/// Источник выборки объектов с индексом занятости (массивы по порядку периодов); условия — по столбцам `dav_objects`.
const OBJECT_FROM: &str = "dav_objects LEFT JOIN LATERAL (
             SELECT array_agg(b.busy_start ORDER BY b.ord) AS busy_starts,
                    array_agg(b.busy_end ORDER BY b.ord) AS busy_ends,
                    array_agg(b.kind ORDER BY b.ord) AS busy_kinds
             FROM dav_busy b
             WHERE b.tenant_id = dav_objects.tenant_id AND b.account_id = dav_objects.account_id
               AND b.object_id = dav_objects.object_id
         ) busy ON true";

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
    schedule_tag: Option<String>,
    search: Vec<String>,
    recurring: bool,
    busy_starts: Option<Vec<OffsetDateTime>>,
    busy_ends: Option<Vec<OffsetDateTime>>,
    busy_kinds: Option<Vec<String>>,
}

impl ObjectRow {
    fn into_object(self) -> Result<DavObject> {
        let busy = self
            .busy_starts
            .unwrap_or_default()
            .into_iter()
            .zip(self.busy_ends.unwrap_or_default())
            .zip(self.busy_kinds.unwrap_or_default())
            .map(|((start, end), kind)| {
                Ok(BusyPeriod {
                    start,
                    end,
                    kind: parse_busy_kind(&kind)?,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(DavObject {
            id: DavObjectId::from_uuid(self.object_id),
            collection: CollectionId::from_uuid(self.collection_id),
            name: self.name,
            etag: self.etag,
            body: self.body,
            index: ObjectIndex {
                busy,
                recurring: self.recurring,
                schedule_tag: self.schedule_tag,
                search: CardSearch {
                    strings: self.search,
                },
                ..ObjectIndex::new(
                    self.component.parse()?,
                    self.uid,
                    self.span_start.map(|start| Span {
                        start,
                        end: self.span_end,
                    }),
                )
            },
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

/// Служебная «учётная запись» книги GAL организации `tenant` в основной БД: строки книги и её счётчик `ModSeq`.
/// Идентификатор — идентификатор организации: среди учётных записей каталога его нет.
fn gal_scope(tenant: TenantId) -> AccountScope {
    AccountScope {
        tenant,
        account: AccountId::from_uuid(*tenant.as_uuid()),
    }
}

/// Книга GAL организации `tenant` (транзакция основной БД), если есть.
async fn fetch_gal(tx: &mut PgConnection, tenant: TenantId) -> Result<Option<Collection>> {
    let scope = gal_scope(tenant);
    let row: Option<CollectionRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {COLLECTION_COLUMNS} FROM dav_collections
         WHERE tenant_id = $1 AND account_id = $2 AND kind = $3 AND name = $4"
    )))
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(kind_str(CollectionKind::AddressBook))
    .bind(GAL_COLLECTION_NAME)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    row.map(CollectionRow::into_collection).transpose()
}

/// Вставляет коллекцию `collection` учётной записи `scope` с номером создания `ms`.
async fn insert_collection(
    tx: &mut PgConnection,
    scope: AccountScope,
    collection: &Collection,
    ms: ModSeq,
) -> Result<()> {
    let components: Vec<&str> = collection.components.iter().map(|c| c.as_str()).collect();
    sqlx::query(
        "INSERT INTO dav_collections (tenant_id, account_id, collection_id, kind, name, display_name,
                                      description, color, sort_order, timezone, components,
                                      created_modseq, props_modseq, modseq, transparent,
                                      is_default, subscribed, visible, alerts_with_time,
                                      alerts_without_time)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $12, $12, $13, $14, $15, $16, $17,
                 $18)",
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
    .bind(collection.transparent)
    .bind(collection.is_default)
    .bind(collection.subscribed)
    .bind(collection.visible)
    .bind(&collection.alerts_with_time)
    .bind(&collection.alerts_without_time)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Записывает ресурс `name` коллекции `collection` с номером `ms` (ETag — он же) и индексом занятости:
/// `existing` — перезапись прежнего объекта (идентификатор сохраняется), `None` — новый. Счётчики коллекции и
/// учётной записи не меняет. Возвращает идентификатор объекта.
#[allow(clippy::too_many_arguments)]
async fn store_object(
    tx: &mut PgConnection,
    scope: AccountScope,
    collection: CollectionId,
    existing: Option<Uuid>,
    name: &str,
    body: &str,
    index: &ObjectIndex,
    ms: ModSeq,
) -> Result<DavObjectId> {
    let etag = ms.to_string();
    let (start, end) = match index.span {
        Some(s) => (Some(s.start), s.end),
        None => (None, None),
    };
    let id = match existing {
        Some(id) => {
            sqlx::query(
                "UPDATE dav_objects SET uid = $4, etag = $5, component = $6, span_start = $7,
                                        span_end = $8, body = $9, modseq = $10, schedule_tag = $11,
                                        search = $12, search_pending = false, recurring = $13
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
            .bind(&index.schedule_tag)
            .bind(&index.search.strings)
            .bind(index.recurring)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            DavObjectId::from_uuid(id)
        }
        None => {
            let id = DavObjectId::new();
            sqlx::query(
                "INSERT INTO dav_objects (tenant_id, account_id, object_id, collection_id, name, uid,
                                          etag, component, span_start, span_end, body,
                                          created_modseq, placed_modseq, modseq, schedule_tag,
                                          search, recurring)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $12, $12, $13, $14, $15)",
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
            .bind(&index.schedule_tag)
            .bind(&index.search.strings)
            .bind(index.recurring)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            id
        }
    };
    replace_busy(tx, scope, collection, id, existing.is_some(), &index.busy).await?;
    Ok(id)
}

/// Снимает признак коллекции по умолчанию с коллекции вида `kind`, кроме `keep`: её изменение свойств с номером
/// `ms`. Возвращает прежнюю коллекцию по умолчанию, если она была.
async fn clear_default(
    tx: &mut PgConnection,
    scope: AccountScope,
    kind: CollectionKind,
    keep: CollectionId,
    ms: ModSeq,
) -> Result<Option<CollectionId>> {
    let prev: Option<Uuid> = sqlx::query_scalar(
        "UPDATE dav_collections SET is_default = false, props_modseq = $5, modseq = $5
         WHERE tenant_id = $1 AND account_id = $2 AND kind = $3 AND is_default AND collection_id <> $4
         RETURNING collection_id",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(kind_str(kind))
    .bind(keep.as_uuid())
    .bind(arg(ms)?)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(prev.map(CollectionId::from_uuid))
}

/// Номера изменений записи коллекции: с признаком по умолчанию прежняя коллекция по умолчанию теряет его под
/// номером `cur + 1`, сама запись — следующим; иначе запись — `cur + 1`.
async fn default_switch(
    tx: &mut PgConnection,
    scope: AccountScope,
    collection: &Collection,
    cur: ModSeq,
) -> Result<(Option<(CollectionId, ModSeq)>, ModSeq)> {
    if collection.is_default {
        let first = cur.next();
        if let Some(prev) = clear_default(tx, scope, collection.kind, collection.id, first).await? {
            return Ok((Some((prev, first)), first.next()));
        }
    }
    Ok((None, cur.next()))
}

/// Объекты коллекции по условию `filter` (параметры с `$4`) по имени побайтно, не более `limit` (`None` — все).
async fn select_objects<'q>(
    tx: &mut PgConnection,
    scope: AccountScope,
    collection: CollectionId,
    filter: &str,
    limit: Option<usize>,
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
        "SELECT {OBJECT_COLUMNS} FROM {OBJECT_FROM}
         WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3 AND ({filter})
         ORDER BY name COLLATE \"C\"{}",
        limit.map_or_else(String::new, |n| format!(
            " LIMIT {}",
            i64::try_from(n).unwrap_or(i64::MAX)
        ))
    )))
    .bind(*scope.tenant.as_uuid())
    .bind(*scope.account.as_uuid())
    .bind(*collection.as_uuid());
    let rows: Vec<ObjectRow> = bind(q).fetch_all(&mut *tx).await.map_err(db_err)?;
    rows.into_iter().map(ObjectRow::into_object).collect()
}

/// Заменяет индекс занятости объекта `object` периодами `busy` (порядок сохраняется); `replace` — у объекта могли
/// быть прежние периоды.
async fn replace_busy(
    tx: &mut PgConnection,
    scope: AccountScope,
    collection: CollectionId,
    object: DavObjectId,
    replace: bool,
    busy: &[BusyPeriod],
) -> Result<()> {
    if replace {
        sqlx::query(
            "DELETE FROM dav_busy WHERE tenant_id = $1 AND account_id = $2 AND object_id = $3",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(object.as_uuid())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    }
    if busy.is_empty() {
        return Ok(());
    }
    let starts: Vec<OffsetDateTime> = busy.iter().map(|p| p.start).collect();
    let ends: Vec<OffsetDateTime> = busy.iter().map(|p| p.end).collect();
    let kinds: Vec<&str> = busy.iter().map(|p| busy_kind_str(p.kind)).collect();
    sqlx::query(
        "INSERT INTO dav_busy (tenant_id, account_id, object_id, ord, collection_id, busy_start, busy_end, kind)
         SELECT $1, $2, $3, (p.ord - 1)::integer, $4, p.busy_start, p.busy_end, p.kind
         FROM unnest($5::timestamptz[], $6::timestamptz[], $7::text[])
              WITH ORDINALITY AS p (busy_start, busy_end, kind, ord)",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(object.as_uuid())
    .bind(collection.as_uuid())
    .bind(&starts)
    .bind(&ends)
    .bind(&kinds)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Счётчик учётной записи и состояние входящих планирования становятся `to`; состояния календарей и адресных книг
/// не меняются. Строка уже заблокирована [`lock_modseq`].
async fn advance_inbox(tx: &mut PgConnection, scope: AccountScope, to: ModSeq) -> Result<()> {
    sqlx::query(
        "UPDATE mailbox_state SET modseq = $3, dav_inbox_state = $3 WHERE tenant_id = $1 AND account_id = $2",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(arg(to)?)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Состояние входящих планирования (`mailbox_state.dav_inbox_state`; строки нет — `ModSeq::ZERO`).
async fn inbox_state(tx: &mut PgConnection, scope: AccountScope) -> Result<ModSeq> {
    let v: Option<i64> = sqlx::query_scalar(
        "SELECT dav_inbox_state FROM mailbox_state WHERE tenant_id = $1 AND account_id = $2",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    modseq(v.unwrap_or(0))
}

#[derive(Debug, FromRow)]
struct InboxRow {
    item_id: Uuid,
    name: String,
    method: String,
    uid: String,
    sender: String,
    body: String,
    etag: String,
    modseq: i64,
    received_at: OffsetDateTime,
}

impl InboxRow {
    fn into_item(self) -> Result<InboxItem> {
        Ok(InboxItem {
            id: DavObjectId::from_uuid(self.item_id),
            name: self.name,
            etag: self.etag,
            modseq: modseq(self.modseq)?,
            received_at: self.received_at,
            message: InboxMessage {
                method: self.method.parse()?,
                uid: self.uid,
                from: self.sender,
                body: self.body,
            },
        })
    }
}

#[derive(Debug, FromRow)]
struct BusyRow {
    account_id: Uuid,
    busy_start: OffsetDateTime,
    busy_end: OffsetDateTime,
    kind: String,
}

/// Повторяющееся событие — кандидат запроса занятости: тело и часовой пояс календаря.
#[derive(Debug, FromRow)]
struct RecurringRow {
    account_id: Uuid,
    body: String,
    timezone: Option<String>,
}

/// Граница очищенных надгробий: вида учётной записи или коллекции.
#[derive(Debug, Clone, Copy)]
enum Floor {
    Kind(CollectionKind),
    Collection(CollectionId),
    /// Входящие планирования (`mailbox_state.dav_inbox_floor`).
    Inbox,
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
    let (which, collection) = match floor {
        Floor::Kind(CollectionKind::Calendar) => ("calendar", None),
        Floor::Kind(CollectionKind::AddressBook) => ("address_book", None),
        Floor::Collection(id) => ("collection", Some(*id.as_uuid())),
        Floor::Inbox => ("inbox", None),
    };
    let row: Option<(i64, Option<i64>)> = sqlx::query_as(
        "SELECT s.modseq,
                CASE $3 WHEN 'collection' THEN
                         (SELECT c.tombstone_floor FROM dav_collections c
                          WHERE c.tenant_id = $1 AND c.account_id = $2 AND c.collection_id = $4)
                     WHEN 'calendar' THEN s.dav_calendar_floor
                     WHEN 'inbox' THEN s.dav_inbox_floor
                     ELSE s.dav_addressbook_floor END
         FROM mailbox_state s WHERE s.tenant_id = $1 AND s.account_id = $2",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(which)
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

/// Изменения объектов коллекций вида `kind` учётной записи `scope` после `since` по идентификаторам (JMAP
/// `/changes`): полный ответ — до состояния вида.
async fn object_changes(
    tx: &mut PgConnection,
    scope: AccountScope,
    kind: CollectionKind,
    since: ModSeq,
    limit: usize,
) -> Result<Changes<DavObjectId>> {
    let mut rows: Vec<ChangeRow<Uuid>> = Vec::new();
    if since <= ModSeq(i64::MAX as u64) {
        rows = sqlx::query_as(
            "SELECT o.object_id, o.modseq, o.created_modseq > $4, false
             FROM dav_objects o JOIN dav_collections c USING (tenant_id, account_id, collection_id)
             WHERE o.tenant_id = $1 AND o.account_id = $2 AND c.kind = $3 AND o.modseq > $4
             UNION ALL
             SELECT t.object_id, t.modseq, false, true FROM dav_tombstones t
             WHERE $4 > 0 AND t.tenant_id = $1 AND t.account_id = $2 AND t.kind = $3
               AND t.object_id IS NOT NULL AND t.modseq > $4
               -- Надгробие перенесённого объекта — не удаление: объект жив в другой коллекции.
               AND NOT EXISTS (SELECT 1 FROM dav_objects l
                               WHERE l.tenant_id = $1 AND l.account_id = $2 AND l.object_id = t.object_id)
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
    let complete = kind_state(tx, scope, kind).await?;
    check_since(tx, scope, since, Floor::Kind(kind)).await?;
    let rows = rows
        .into_iter()
        .map(|(id, m, c, d)| (DavObjectId::from_uuid(id), m, c, d))
        .collect();
    page(rows, since, limit, complete)
}

/// Объекты учётной записи `scope` с идентификаторами `ids` (транзакция чтения на пуле `pool`).
async fn objects_with_ids(
    pool: &crate::pool::DbPool,
    scope: AccountScope,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, DavObject>> {
    let mut tx = begin(pool, scope.tenant).await?;
    let rows: Vec<ObjectRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {OBJECT_COLUMNS} FROM {OBJECT_FROM}
         WHERE tenant_id = $1 AND account_id = $2 AND object_id = ANY($3)"
    )))
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    crate::commits::commit(tx).await?;
    rows.into_iter()
        .map(|r| Ok((r.object_id, r.into_object()?)))
        .collect()
}

impl PgMeta {
    /// Книга GAL организации `tenant` (основная БД), если есть.
    async fn gal_book(&self, tenant: TenantId) -> Result<Option<Collection>> {
        let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
        let gal = fetch_gal(&mut tx, tenant).await?;
        crate::commits::commit(tx).await?;
        Ok(gal)
    }

    /// Транзакция чтения коллекции `id`, видимой учётной записи `scope`, с областью строк коллекции и самой
    /// коллекцией: своя — на шарде учётной записи; иначе книга GAL организации — в основной БД ([`gal_scope`]).
    /// Нет ни той, ни другой — `NotFound`.
    async fn open_visible(
        &self,
        scope: AccountScope,
        id: CollectionId,
    ) -> Result<(Transaction<'static, Postgres>, AccountScope, Collection)> {
        let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
        if let Some(c) = fetch_collection(&mut tx, scope, id).await? {
            return Ok((tx, scope, c));
        }
        crate::commits::commit(tx).await?;
        let gal = gal_scope(scope.tenant);
        let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, scope.tenant).await?;
        let c = require_collection(&mut tx, gal, id).await?;
        Ok((tx, gal, c))
    }

    /// Своя коллекция `id` для записи; нет — книга GAL организации `Forbidden`, иначе `NotFound`.
    async fn own_collection(
        &self,
        tx: &mut PgConnection,
        scope: AccountScope,
        id: CollectionId,
    ) -> Result<Collection> {
        if let Some(c) = fetch_collection(tx, scope, id).await? {
            return Ok(c);
        }
        Err(
            if self
                .gal_book(scope.tenant)
                .await?
                .is_some_and(|g| g.id == id)
            {
                Error::Forbidden
            } else {
                Error::NotFound
            },
        )
    }

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
                            "SELECT tenant_id, account_id FROM dav_tombstones WHERE deleted_at < $1
                             UNION
                             SELECT tenant_id, account_id FROM dav_inbox_tombstones WHERE deleted_at < $1",
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

/// Очистка надгробий учётной записи `scope` старше `before` (DAV и входящих планирования) с подъёмом границ вида,
/// коллекций и входящих.
async fn purge_account(
    pool: &crate::pool::DbPool,
    scope: AccountScope,
    before: OffsetDateTime,
) -> Result<u64> {
    let mut tx = begin(pool, scope.tenant).await?;
    lock_modseq(&mut tx, scope).await?;
    let n: i64 = sqlx::query_scalar(
        "WITH gone AS (
             DELETE FROM dav_tombstones WHERE tenant_id = $1 AND account_id = $2 AND deleted_at < $3
             RETURNING kind, collection_id, modseq
         ), inbox_gone AS (
             DELETE FROM dav_inbox_tombstones WHERE tenant_id = $1 AND account_id = $2 AND deleted_at < $3
             RETURNING modseq
         ), by_collection AS (
             UPDATE dav_collections c SET tombstone_floor = greatest(c.tombstone_floor, g.floor)
             FROM (SELECT collection_id, max(modseq) AS floor FROM gone GROUP BY collection_id) g
             WHERE c.tenant_id = $1 AND c.account_id = $2 AND c.collection_id = g.collection_id
         ), by_kind AS (
             UPDATE mailbox_state s SET
                 dav_calendar_floor = greatest(s.dav_calendar_floor,
                     coalesce((SELECT max(modseq) FROM gone WHERE kind = 'calendar'), 0)),
                 dav_addressbook_floor = greatest(s.dav_addressbook_floor,
                     coalesce((SELECT max(modseq) FROM gone WHERE kind = 'address_book'), 0)),
                 dav_inbox_floor = greatest(s.dav_inbox_floor,
                     coalesce((SELECT max(modseq) FROM inbox_gone), 0))
             WHERE s.tenant_id = $1 AND s.account_id = $2
         )
         SELECT (SELECT count(*) FROM gone) + (SELECT count(*) FROM inbox_gone)",
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
            let mut out = rows
                .into_iter()
                .map(CollectionRow::into_collection)
                .collect::<Result<Vec<_>>>()?;
            if kind == CollectionKind::AddressBook
                && let Some(gal) = self.gal_book(scope.tenant).await?
            {
                out.push(gal);
                out.sort_by(|a, b| (a.order, a.name.as_bytes()).cmp(&(b.order, b.name.as_bytes())));
            }
            Ok(out)
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
            match c {
                Some(c) => Ok(Some(c)),
                None => Ok(self.gal_book(scope.tenant).await?.filter(|g| g.id == id)),
            }
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
            match row {
                Some(row) => row.into_collection().map(Some),
                None if kind == CollectionKind::AddressBook && name == GAL_COLLECTION_NAME => {
                    self.gal_book(scope.tenant).await
                }
                None => Ok(None),
            }
        })
        .await
    }

    async fn create_collection(
        &self,
        scope: AccountScope,
        collection: Collection,
    ) -> Result<ModSeq> {
        collection.validate()?;
        if collection.is_gal() {
            return Err(Error::Conflict);
        }
        let limit = i64::try_from(self.limits().max_collections).unwrap_or(i64::MAX);
        let (cleared, modseq) = self
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
                let (cleared, ms) = default_switch(&mut tx, scope, &collection, cur).await?;
                insert_collection(&mut tx, scope, &collection, ms).await?;
                advance(&mut tx, scope, collection.kind, ms).await?;
                crate::commits::commit(tx).await?;
                Ok((cleared, ms))
            })
            .await?;
        if let Some((prev, m)) = cleared {
            self.dav_signal(scope, collection.kind, prev, m).await;
        }
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
        let (cleared, modseq) = self
            .once(async {
                let mut tx = begin(&self.box_pool(scope).await?, scope.tenant).await?;
                let cur = lock_modseq(&mut tx, scope).await?;
                let now = self.own_collection(&mut tx, scope, collection.id).await?;
                if now.kind != collection.kind || now.name != collection.name {
                    return Err(Error::invalid("вид и имя коллекции не меняются"));
                }
                let (cleared, ms) = default_switch(&mut tx, scope, &collection, cur).await?;
                let components: Vec<&str> =
                    collection.components.iter().map(|c| c.as_str()).collect();
                sqlx::query(
                    "UPDATE dav_collections SET display_name = $4, description = $5, color = $6, sort_order = $7,
                                                timezone = $8, components = $9, props_modseq = $10, modseq = $10,
                                                transparent = $11, is_default = $12, subscribed = $13,
                                                visible = $14, alerts_with_time = $15, alerts_without_time = $16
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
                .bind(collection.transparent)
                .bind(collection.is_default)
                .bind(collection.subscribed)
                .bind(collection.visible)
                .bind(&collection.alerts_with_time)
                .bind(&collection.alerts_without_time)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                advance(&mut tx, scope, collection.kind, ms).await?;
                crate::commits::commit(tx).await?;
                Ok((cleared, ms))
            })
            .await?;
        if let Some((prev, m)) = cleared {
            self.dav_signal(scope, collection.kind, prev, m).await;
        }
        self.dav_signal(scope, collection.kind, collection.id, modseq)
            .await;
        Ok(modseq)
    }

    async fn delete_collection(&self, scope: AccountScope, id: CollectionId) -> Result<ModSeq> {
        let (kind, modseq) = self
            .once(async {
                let mut tx = begin(&self.box_pool(scope).await?, scope.tenant).await?;
                let cur = lock_modseq(&mut tx, scope).await?;
                let c = self.own_collection(&mut tx, scope, id).await?;
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
            let (mut tx, scope, _) = self.open_visible(scope, collection).await?;
            let mut out = select_objects(&mut tx, scope, collection, "name = $4", None, |q| {
                q.bind(name)
            })
            .await?;
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
            let (mut tx, scope, _) = self.open_visible(scope, collection).await?;
            let mut out = select_objects(&mut tx, scope, collection, "uid = $4", None, |q| {
                q.bind(uid)
            })
            .await?;
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
            let (mut tx, scope, _) = self.open_visible(scope, collection).await?;
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
            let (mut tx, scope, _) = self.open_visible(scope, collection).await?;
            let found = select_objects(&mut tx, scope, collection, "name = ANY($4)", None, |q| {
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
                let pool = self.read_db(scope).await?.pool;
                let mut found = objects_with_ids(&pool, scope, &raw).await?;
                // Не найденные среди своих — карточки книги GAL организации (основная БД).
                let rest: Vec<Uuid> = raw
                    .iter()
                    .filter(|id| !found.contains_key(id))
                    .copied()
                    .collect();
                if !rest.is_empty() {
                    let main = self.read_pool(ShardId::MAIN).await?;
                    found.extend(objects_with_ids(&main, gal_scope(scope.tenant), &rest).await?);
                }
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
            let (mut tx, scope, _) = self.open_visible(scope, collection).await?;
            // Условие ObjectIndex::may_overlap: без интервала — всегда; иначе start < range.end и end ≥ range.start.
            let out = select_objects(
                &mut tx,
                scope,
                collection,
                "span_start IS NULL
                 OR (($5::timestamptz IS NULL OR span_start < $5)
                     AND (span_end IS NULL OR $4::timestamptz IS NULL OR span_end >= $4))",
                None,
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
                let c = self.own_collection(&mut tx, scope, collection).await?;
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
                let id = store_object(
                    &mut tx,
                    scope,
                    collection,
                    existing.as_ref().map(|e| e.0),
                    name,
                    body,
                    index,
                    ms,
                )
                .await?;
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
                        etag: ms.to_string(),
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
                let c = self.own_collection(&mut tx, scope, collection).await?;
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

    async fn move_object(
        &self,
        scope: AccountScope,
        from: CollectionId,
        name: &str,
        to: CollectionId,
        pre: Precondition,
    ) -> Result<MoveOutcome> {
        let limits = self.limits();
        let pre = &pre;
        let (kind, outcome) = self
            .once(async move {
                let mut tx = begin(&self.box_pool(scope).await?, scope.tenant).await?;
                let cur = lock_modseq(&mut tx, scope).await?;
                let src = fetch_collection(&mut tx, scope, from).await?;
                let dst = fetch_collection(&mut tx, scope, to).await?;
                let (src, dst) = match (src, dst) {
                    (Some(src), Some(dst)) => (src, dst),
                    (src, dst) => {
                        // Обе видны (своя или книга GAL) — книга GAL среди них: `Forbidden`.
                        let gal = self.gal_book(scope.tenant).await?.map(|g| g.id);
                        let seen = |own: bool, id: CollectionId| own || gal == Some(id);
                        return Err(if seen(src.is_some(), from) && seen(dst.is_some(), to) {
                            Error::Forbidden
                        } else {
                            Error::NotFound
                        });
                    }
                };
                let existing: Option<(Uuid, String, String, String)> = sqlx::query_as(
                    "SELECT object_id, etag, component, uid FROM dav_objects
                     WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3 AND name = $4",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(from.as_uuid())
                .bind(name)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
                if existing.is_none() && !matches!(pre, Precondition::IfMatch(_)) {
                    return Err(Error::NotFound);
                }
                if from == to || src.kind != dst.kind {
                    return Err(Error::invalid(
                        "перенос — между разными коллекциями одного вида",
                    ));
                }
                if let Some((_, _, component, _)) = &existing {
                    let component: Component = component.parse()?;
                    if !dst.components.contains(&component) {
                        return Err(Error::invalid(format!(
                            "компонент {component} не поддерживается коллекцией"
                        )));
                    }
                }
                let Some((object, _, _, uid)) = existing.filter(|e| match pre {
                    Precondition::IfMatch(etag) => e.1 == *etag,
                    Precondition::Any | Precondition::IfNoneMatch => true,
                }) else {
                    return Ok((src.kind, MoveOutcome::PreconditionFailed));
                };
                // Занятое имя — отказ по условию; занятый UID — конфликт (имя проверяется первым).
                let taken: Vec<(String, bool)> = sqlx::query_as(
                    "SELECT name, name = $4 FROM dav_objects
                     WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3 AND (name = $4 OR uid = $5)
                     ORDER BY 2 DESC",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(to.as_uuid())
                .bind(name)
                .bind(&uid)
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?;
                match taken.into_iter().next() {
                    Some((_, true)) => return Ok((src.kind, MoveOutcome::PreconditionFailed)),
                    Some((name, false)) => {
                        return Ok((src.kind, MoveOutcome::UidConflict { name }));
                    }
                    None => {}
                }
                let count: i32 = sqlx::query_scalar(
                    "SELECT object_count FROM dav_collections
                     WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(to.as_uuid())
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
                if usize::try_from(count).unwrap_or(usize::MAX) >= limits.max_objects {
                    return Err(Error::QuotaExceeded);
                }
                let removed = cur.next();
                let ms = removed.next();
                let etag = ms.to_string();
                sqlx::query(
                    "INSERT INTO dav_tombstones (tenant_id, account_id, modseq, kind, collection_id, object_id, name)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(arg(removed)?)
                .bind(kind_str(src.kind))
                .bind(from.as_uuid())
                .bind(object)
                .bind(name)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                // Индекс занятости ссылается на коллекцию объекта: переносится вместе с ним.
                sqlx::query(
                    "UPDATE dav_busy SET collection_id = $4
                     WHERE tenant_id = $1 AND account_id = $2 AND object_id = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(object)
                .bind(to.as_uuid())
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                sqlx::query(
                    "UPDATE dav_objects SET collection_id = $4, etag = $5, placed_modseq = $6, modseq = $6
                     WHERE tenant_id = $1 AND account_id = $2 AND object_id = $3",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(object)
                .bind(to.as_uuid())
                .bind(&etag)
                .bind(arg(ms)?)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                for (collection, m, delta) in [(from, removed, -1_i32), (to, ms, 1)] {
                    sqlx::query(
                        "UPDATE dav_collections SET modseq = $4, object_count = object_count + $5
                         WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3",
                    )
                    .bind(scope.tenant.as_uuid())
                    .bind(scope.account.as_uuid())
                    .bind(collection.as_uuid())
                    .bind(arg(m)?)
                    .bind(delta)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                }
                advance(&mut tx, scope, src.kind, ms).await?;
                crate::commits::commit(tx).await?;
                Ok((
                    src.kind,
                    MoveOutcome::Moved(Moved {
                        id: DavObjectId::from_uuid(object),
                        etag,
                        removed,
                        modseq: ms,
                    }),
                ))
            })
            .await?;
        if let MoveOutcome::Moved(m) = &outcome {
            self.dav_signal(scope, kind, from, m.removed).await;
            self.dav_signal(scope, kind, to, m.modseq).await;
        }
        Ok(outcome)
    }

    async fn find_cards(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        needle: &str,
        limit: usize,
    ) -> Result<Vec<DavObject>> {
        let needle = &needle.trim().to_lowercase();
        self.idempotent(|| async move {
            let (mut tx, scope, _) = self.open_visible(scope, collection).await?;
            // Условие CardSearch::matches: вхождение подстроки (strpos — без шаблонов LIKE).
            let out = select_objects(
                &mut tx,
                scope,
                collection,
                "$4 = '' OR EXISTS (SELECT 1 FROM unnest(search) s WHERE strpos(s, $4) > 0)",
                Some(limit),
                |q| q.bind(needle.as_str()),
            )
            .await?;
            crate::commits::commit(tx).await?;
            Ok(out)
        })
        .await
    }

    async fn changes(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<String>> {
        self.idempotent(|| async move {
            let (mut tx, scope, c) = self.open_visible(scope, collection).await?;
            let mut rows: Vec<ChangeRow<String>> = Vec::new();
            if since <= ModSeq(i64::MAX as u64) {
                // Существующие ресурсы после since и последние надгробия имён, которых сейчас нет.
                rows = sqlx::query_as(
                    "SELECT name, modseq, placed_modseq > $4, false FROM dav_objects
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
            let out = object_changes(&mut tx, scope, kind, since, limit).await?;
            crate::commits::commit(tx).await?;
            Ok(out)
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

    async fn inbox_put(
        &self,
        scope: AccountScope,
        name: &str,
        message: InboxMessage,
    ) -> Result<Put> {
        if !valid_name(name) {
            return Err(Error::invalid(format!(
                "недопустимое имя ресурса: {name:?}"
            )));
        }
        message.validate()?;
        let limits = self.limits();
        if message.body.len() > limits.max_object_size {
            return Err(Error::QuotaExceeded);
        }
        let message = &message;
        let put = self
            .once(async move {
                let db = self.box_db(scope).await?;
                let mut tx = begin(&db.pool, scope.tenant).await?;
                if db.shard != ShardId::MAIN {
                    // Учётная запись есть в каталоге (шард найден), состояния ящика на шарде может ещё не быть.
                    ensure_state(&mut tx, scope).await?;
                }
                let cur = lock_modseq(&mut tx, scope).await?;
                let (existing, count): (Option<Uuid>, i64) = sqlx::query_as(
                    "SELECT (SELECT item_id FROM dav_inbox
                             WHERE tenant_id = $1 AND account_id = $2 AND name = $3),
                            (SELECT count(*) FROM dav_inbox WHERE tenant_id = $1 AND account_id = $2)",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(name)
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
                let id = existing.map_or_else(DavObjectId::new, DavObjectId::from_uuid);
                sqlx::query(
                    "INSERT INTO dav_inbox AS i (tenant_id, account_id, item_id, name, method, uid, sender, body,
                                                 etag, modseq, created_modseq)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $10)
                     -- Перезапись сохраняет received_at и created_modseq (CTR-65).
                     ON CONFLICT (tenant_id, account_id, item_id) DO UPDATE
                     SET method = excluded.method, uid = excluded.uid, sender = excluded.sender,
                         body = excluded.body, etag = excluded.etag, modseq = excluded.modseq",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(id.as_uuid())
                .bind(name)
                .bind(message.method.as_str())
                .bind(&message.uid)
                .bind(&message.from)
                .bind(&message.body)
                .bind(&etag)
                .bind(arg(ms)?)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                advance_inbox(&mut tx, scope, ms).await?;
                crate::commits::commit(tx).await?;
                Ok(Put {
                    id,
                    etag,
                    modseq: ms,
                    created: existing.is_none(),
                })
            })
            .await?;
        self.signal(schedule_inbox_changed(scope, put.modseq)).await;
        Ok(put)
    }

    async fn inbox_list(&self, scope: AccountScope) -> Result<Inbox> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            let rows: Vec<InboxRow> = sqlx::query_as(
                "SELECT item_id, name, method, uid, sender, body, etag, modseq, received_at FROM dav_inbox
                 WHERE tenant_id = $1 AND account_id = $2 ORDER BY modseq",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            let state = inbox_state(&mut tx, scope).await?;
            crate::commits::commit(tx).await?;
            Ok(Inbox {
                items: rows
                    .into_iter()
                    .map(InboxRow::into_item)
                    .collect::<Result<_>>()?,
                state,
            })
        })
        .await
    }

    /// Удаление и надгробие — одним запросом: ресурса нет — нет и надгробия.
    async fn inbox_delete(&self, scope: AccountScope, name: &str) -> Result<ModSeq> {
        let ms = self
            .once(async move {
                let mut tx = begin(&self.box_pool(scope).await?, scope.tenant).await?;
                let ms = lock_modseq(&mut tx, scope).await?.next();
                let gone: Option<Uuid> = sqlx::query_scalar(
                    "WITH gone AS (
                         DELETE FROM dav_inbox WHERE tenant_id = $1 AND account_id = $2 AND name = $3
                         RETURNING item_id
                     )
                     INSERT INTO dav_inbox_tombstones (tenant_id, account_id, modseq, item_id)
                     SELECT $1, $2, $4, item_id FROM gone
                     RETURNING item_id",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(name)
                .bind(arg(ms)?)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
                if gone.is_none() {
                    return Err(Error::NotFound);
                }
                advance_inbox(&mut tx, scope, ms).await?;
                crate::commits::commit(tx).await?;
                Ok(ms)
            })
            .await?;
        self.signal(schedule_inbox_changed(scope, ms)).await;
        Ok(ms)
    }

    async fn inbox_state(&self, scope: AccountScope) -> Result<ModSeq> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            let s = inbox_state(&mut tx, scope).await?;
            crate::commits::commit(tx).await?;
            Ok(s)
        })
        .await
    }

    /// Живые ресурсы с `modseq > since` (созданы — `created_modseq > since`) и надгробия после `since` (при
    /// `since = 0` — без надгробий) одним запросом по `ModSeq`; границу проверяет [`check_since`] после выборки.
    async fn inbox_changes(
        &self,
        scope: AccountScope,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<DavObjectId>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(scope).await?.pool, scope.tenant).await?;
            let mut rows: Vec<ChangeRow<Uuid>> = Vec::new();
            if since <= ModSeq(i64::MAX as u64) {
                rows = sqlx::query_as(
                    "SELECT item_id, modseq, created_modseq > $3, false FROM dav_inbox
                     WHERE tenant_id = $1 AND account_id = $2 AND modseq > $3
                     UNION ALL
                     SELECT item_id, modseq, false, true FROM dav_inbox_tombstones
                     WHERE $3 > 0 AND tenant_id = $1 AND account_id = $2 AND modseq > $3
                     ORDER BY 2 LIMIT $4",
                )
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(arg(since)?)
                .bind(fetch_limit(limit))
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?;
            }
            let complete = inbox_state(&mut tx, scope).await?;
            check_since(&mut tx, scope, since, Floor::Inbox).await?;
            crate::commits::commit(tx).await?;
            let rows = rows
                .into_iter()
                .map(|(id, m, c, d)| (DavObjectId::from_uuid(id), m, c, d))
                .collect();
            page(rows, since, limit, complete)
        })
        .await
    }

    async fn sync_gal(
        &self,
        tenant: TenantId,
        display_name: &str,
        mut cards: Vec<GalCard>,
    ) -> Result<ModSeq> {
        validate_gal(display_name, &cards, &self.limits())?;
        // Номера изменений — по порядку имён (побайтно), как у надгробий.
        cards.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        let cards = &cards;
        let count = i32::try_from(cards.len()).map_err(|_| Error::QuotaExceeded)?;
        // Полный набор по значению: повтор после обрыва во время COMMIT находит записанное и ничего не меняет.
        let (id, state, changed) = self
            .rewrite(|| async move {
                let scope = gal_scope(tenant);
                let mut tx = begin(&self.pool, tenant).await?;
                ensure_state(&mut tx, scope).await?;
                let before = lock_modseq(&mut tx, scope).await?;
                let mut ms = before;
                let book = match fetch_gal(&mut tx, tenant).await? {
                    Some(book) => book,
                    None => {
                        ms = ms.next();
                        let book = Collection {
                            display_name: display_name.to_owned(),
                            ..Collection::new(CollectionKind::AddressBook, GAL_COLLECTION_NAME)
                        };
                        insert_collection(&mut tx, scope, &book, ms).await?;
                        book
                    }
                };
                if book.display_name != display_name {
                    ms = ms.next();
                    sqlx::query(
                        "UPDATE dav_collections SET display_name = $4, props_modseq = $5, modseq = $5
                         WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3",
                    )
                    .bind(scope.tenant.as_uuid())
                    .bind(scope.account.as_uuid())
                    .bind(book.id.as_uuid())
                    .bind(display_name)
                    .bind(arg(ms)?)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                }
                let existing = select_objects(&mut tx, scope, book.id, "true", None, |q| q).await?;
                let wanted: HashSet<&str> = cards.iter().map(|c| c.name.as_str()).collect();
                // Исчезнувшие — в надгробия, каждая со своим ModSeq по порядку имён.
                let gone: Vec<&str> = existing
                    .iter()
                    .map(|o| o.name.as_str())
                    .filter(|n| !wanted.contains(n))
                    .collect();
                if !gone.is_empty() {
                    sqlx::query(
                        "WITH gone AS (
                             DELETE FROM dav_objects
                             WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3 AND name = ANY($4)
                             RETURNING object_id, name
                         )
                         INSERT INTO dav_tombstones (tenant_id, account_id, modseq, kind, collection_id, object_id,
                                                     name)
                         SELECT $1, $2, $5 + row_number() OVER (ORDER BY name COLLATE \"C\"), $6, $3, object_id,
                                name
                         FROM gone",
                    )
                    .bind(scope.tenant.as_uuid())
                    .bind(scope.account.as_uuid())
                    .bind(book.id.as_uuid())
                    .bind(&gone)
                    .bind(arg(ms)?)
                    .bind(kind_str(CollectionKind::AddressBook))
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                    ms = ModSeq(ms.0 + gone.len() as u64);
                }
                // Неизменные (то же тело и индекс) не трогаются: ETag и ModSeq прежние.
                let by_name: HashMap<&str, &DavObject> =
                    existing.iter().map(|o| (o.name.as_str(), o)).collect();
                let write: Vec<(&GalCard, Option<&DavObject>)> = cards
                    .iter()
                    .filter_map(|card| match by_name.get(card.name.as_str()) {
                        Some(o) if o.body == card.body && o.index == card.index => None,
                        prev => Some((card, prev.copied())),
                    })
                    .collect();
                // UID переходит между карточками набора: прежние UID перезаписываемых освобождаются заранее
                // (уникальный индекс проверяется построчно).
                let freed: Vec<Uuid> = write
                    .iter()
                    .filter_map(|(card, prev)| {
                        prev.filter(|o| o.index.uid != card.index.uid)
                            .map(|o| *o.id.as_uuid())
                    })
                    .collect();
                if !freed.is_empty() {
                    sqlx::query(
                        "UPDATE dav_objects SET uid = object_id::text
                         WHERE tenant_id = $1 AND account_id = $2 AND object_id = ANY($3)",
                    )
                    .bind(scope.tenant.as_uuid())
                    .bind(scope.account.as_uuid())
                    .bind(&freed)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                }
                for (card, prev) in write {
                    ms = ms.next();
                    store_object(
                        &mut tx,
                        scope,
                        book.id,
                        prev.map(|o| *o.id.as_uuid()),
                        &card.name,
                        &card.body,
                        &card.index,
                        ms,
                    )
                    .await?;
                }
                let changed = ms != before;
                if changed {
                    sqlx::query(
                        "UPDATE dav_collections SET modseq = $4, object_count = $5
                         WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3",
                    )
                    .bind(scope.tenant.as_uuid())
                    .bind(scope.account.as_uuid())
                    .bind(book.id.as_uuid())
                    .bind(arg(ms)?)
                    .bind(count)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                    advance(&mut tx, scope, CollectionKind::AddressBook, ms).await?;
                }
                crate::commits::commit(tx).await?;
                Ok((book.id, if changed { ms } else { book.state }, changed))
            })
            .await?;
        if changed {
            self.signal(gal_changed(tenant, id, state)).await;
        }
        Ok(state)
    }

    async fn gal_changes(
        &self,
        tenant: TenantId,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<DavObjectId>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let out = object_changes(
                &mut tx,
                gal_scope(tenant),
                CollectionKind::AddressBook,
                since,
                limit,
            )
            .await?;
            crate::commits::commit(tx).await?;
            Ok(out)
        })
        .await
    }
}

#[async_trait]
impl FreeBusy for PgMeta {
    async fn query(
        &self,
        tenant: TenantId,
        accounts: &[AccountId],
        range: Period,
    ) -> Result<Vec<(AccountId, Vec<BusyPeriod>)>> {
        range.validate()?;
        let mut order: Vec<AccountId> = Vec::with_capacity(accounts.len());
        for a in accounts {
            if !order.contains(a) {
                order.push(*a);
            }
        }
        if order.is_empty() {
            return Ok(Vec::new());
        }
        // Учётные записи по шардам: на шард — один запрос. Учётной записи нет в каталоге — занятости нет.
        let mut by_shard: BTreeMap<ShardId, Vec<Uuid>> = BTreeMap::new();
        for account in &order {
            match self
                .shard_of(AccountScope {
                    tenant,
                    account: *account,
                })
                .await
            {
                Ok(shard) => by_shard.entry(shard).or_default().push(*account.as_uuid()),
                Err(Error::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        let mut found: HashMap<Uuid, Vec<BusyPeriod>> = HashMap::new();
        let mut recurring: Vec<RecurringRow> = Vec::new();
        for (shard, ids) in by_shard {
            let ids = &ids;
            let (rows, bodies): (Vec<BusyRow>, Vec<RecurringRow>) = self
                .idempotent(|| async move {
                    let mut tx = begin(&self.read_pool(shard).await?, tenant).await?;
                    let rows = sqlx::query_as(
                        "SELECT b.account_id, b.busy_start, b.busy_end, b.kind
                         FROM dav_busy b JOIN dav_collections c USING (tenant_id, account_id, collection_id)
                         WHERE b.tenant_id = $1 AND b.account_id = ANY($2)
                           AND b.busy_end > $3 AND b.busy_start < $4
                           AND c.kind = 'calendar' AND NOT c.transparent",
                    )
                    .bind(tenant.as_uuid())
                    .bind(ids)
                    .bind(range.start)
                    .bind(range.end)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(db_err)?;
                    // Повторяющиеся события — кандидаты по интервалу объекта (span_end NULL — без конца).
                    let bodies = sqlx::query_as(
                        "SELECT o.account_id, o.body, c.timezone
                         FROM dav_objects o JOIN dav_collections c USING (tenant_id, account_id, collection_id)
                         WHERE o.tenant_id = $1 AND o.account_id = ANY($2) AND o.recurring
                           AND (o.span_start IS NULL OR o.span_start < $4)
                           AND (o.span_end IS NULL OR o.span_end > $3)
                           AND c.kind = 'calendar' AND NOT c.transparent",
                    )
                    .bind(tenant.as_uuid())
                    .bind(ids)
                    .bind(range.start)
                    .bind(range.end)
                    .fetch_all(&mut *tx)
                    .await
                    .map_err(db_err)?;
                    crate::commits::commit(tx).await?;
                    Ok((rows, bodies))
                })
                .await?;
            recurring.extend(bodies);
            for r in rows {
                let p = BusyPeriod {
                    start: r.busy_start,
                    end: r.busy_end,
                    kind: parse_busy_kind(&r.kind)?,
                };
                if let Some(p) = p.clip(&range) {
                    found.entry(r.account_id).or_default().push(p);
                }
            }
        }
        if !recurring.is_empty() {
            // Разворот повторов — вычисления вне транзакции и вне потоков async.
            let limit = self.cfg.dav.max_busy_instances;
            let expanded = tokio::task::spawn_blocking(move || {
                recurring
                    .into_iter()
                    .map(|r| {
                        let periods =
                            crate::busy::expand(&r.body, r.timezone.as_deref(), &range, limit);
                        (r.account_id, periods)
                    })
                    .collect::<Vec<_>>()
            })
            .await
            .map_err(|e| Error::internal(format!("разворот повторов: {e}")))?;
            for (account, periods) in expanded {
                match periods {
                    Some(p) => found.entry(account).or_default().extend(p),
                    None => tracing::warn!(
                        tenant = %tenant,
                        "тело повторяющегося события не разбирается — в занятости не учтено"
                    ),
                }
            }
        }
        Ok(order
            .into_iter()
            .map(|a| {
                let periods = found.remove(a.as_uuid()).unwrap_or_default();
                (a, merge_busy(periods))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_from_yaml() {
        let c: DavConfig = serde_yaml_ng::from_str(
            "max_object_size: 4096\nmax_objects: 4\nmax_collections: 3\nmax_busy_instances: 500\ntombstone_ttl: 7d\n",
        )
        .expect("yaml");
        assert_eq!(
            c.limits(),
            DavLimits {
                max_object_size: 4096,
                max_objects: 4,
                max_collections: 3,
                max_busy_instances: 500,
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
