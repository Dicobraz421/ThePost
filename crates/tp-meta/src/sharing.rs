//! Общий доступ к календарям и адресным книгам, представители, ресурсы на PostgreSQL (META-52; CTR-48,
//! CONTRACTS.md#Sharing; SRS 4.4, 4.5).
//!
//! - [`CollectionAcl`] — таблица ящика `collection_acl` на шарде владельца рядом с `dav_collections` (удаление
//!   коллекции удаляет записи внешним ключом); [`CollectionAcl::shared_with`] — один запрос на шард: записи
//!   идентификаторов субъекта (имя входа, группы с вложенными, `anyone` — как у
//!   [`Principal`](tp_contracts::auth::Principal)), сведённые по коллекции.
//! - [`DelegateStore`] — `account_delegates` каталога (внешние ключи к обеим учётным записям).
//! - [`ResourceStore`] — `accounts.booking_policy` (JSON контракта); смену вида на пользователя снимает триггер.
//! - [`SharingPolicyStore`] — `sharing_policies` (JSON контракта); нет строки — политика по умолчанию.
//! - [`ShareNotificationStore`] — каталог (META-83, CTR-86, миграция 0042): `share_notifications`, состояние
//!   получателя `share_notification_state` и журнал состояний `share_notification_log` для `changes`; удаление
//!   учётной записи-получателя удаляет их внешними ключами. Срок хранения убирает и историю удалённых уведомлений
//!   старше срока (граница `floor`: `since` ниже — `Conflict`). Сверх `MAX_NOTICES` запись вытесняет старшие
//!   (CTR-90) без новой миграции: выборка по первичному ключу получателя.

use std::collections::BTreeMap;

use async_trait::async_trait;
use sqlx::types::Json;
use sqlx::{FromRow, PgConnection};
use time::OffsetDateTime;
use tp_contracts::dav::CollectionId;
use tp_contracts::meta::AccountScope;
use tp_contracts::sharing::{
    BookingPolicy, CollectionAcl, CollectionAclEntry, CollectionRights, Delegate, DelegateLevel,
    DelegateStore, Delegation, MAX_NOTICES, ResourceStore, ShareChangedBy, ShareNotification,
    ShareNotificationChanges, ShareNotificationStore, SharedCollection, SharedObjectType,
    SharingPolicy, SharingPolicyStore, validate_identifier,
};
use tp_contracts::{AccountId, Error, Result, ShareNotificationId, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::schema::{begin, begin_system, db_err, to_u64};
use crate::shards::ShardId;

/// Права строки `collection_acl`: `free_busy`, `read`, `write`, `schedule`, `admin`.
type RightsRow = (bool, bool, bool, bool, bool);

fn rights((free_busy, read, write, schedule, admin): RightsRow) -> CollectionRights {
    CollectionRights {
        free_busy,
        read,
        write,
        schedule,
        admin,
    }
}

fn validate_identifiers(identifiers: &[String]) -> Result<()> {
    identifiers
        .iter()
        .try_for_each(|id| validate_identifier(id, false))
}

async fn collection_exists(
    tx: &mut PgConnection,
    owner: AccountScope,
    collection: CollectionId,
) -> Result<bool> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM dav_collections WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3",
    )
    .bind(owner.tenant.as_uuid())
    .bind(owner.account.as_uuid())
    .bind(collection.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(found.is_some())
}

/// Сколько из учётных записей `ids` организации `tenant` есть в каталоге.
async fn accounts_found(tx: &mut PgConnection, tenant: TenantId, ids: &[Uuid]) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT count(*) FROM accounts WHERE tenant_id = $1 AND account_id = ANY($2)",
    )
    .bind(tenant.as_uuid())
    .bind(ids)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err)
}

fn level_str(level: DelegateLevel) -> &'static str {
    match level {
        DelegateLevel::Read => "read",
        DelegateLevel::Write => "write",
    }
}

fn parse_level(s: &str) -> Result<DelegateLevel> {
    match s {
        "read" => Ok(DelegateLevel::Read),
        "write" => Ok(DelegateLevel::Write),
        other => Err(Error::internal(format!("account_delegates.level: {other}"))),
    }
}

#[async_trait]
impl CollectionAcl for PgMeta {
    async fn collection_acl(
        &self,
        owner: AccountScope,
        collection: CollectionId,
    ) -> Result<Vec<CollectionAclEntry>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(owner).await?.pool, owner.tenant).await?;
            if !collection_exists(&mut tx, owner, collection).await? {
                return Err(Error::NotFound);
            }
            let rows: Vec<(String, bool, bool, bool, bool, bool)> = sqlx::query_as(
                "SELECT identifier, free_busy, read, write, schedule, admin FROM collection_acl
                 WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3
                 ORDER BY identifier COLLATE \"C\"",
            )
            .bind(owner.tenant.as_uuid())
            .bind(owner.account.as_uuid())
            .bind(collection.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(rows
                .into_iter()
                .map(|(identifier, f, r, w, s, a)| CollectionAclEntry {
                    identifier,
                    rights: rights((f, r, w, s, a)),
                })
                .collect())
        })
        .await
    }

    async fn set_collection_acl(
        &self,
        owner: AccountScope,
        collection: CollectionId,
        identifier: &str,
        rights: CollectionRights,
    ) -> Result<()> {
        validate_identifier(identifier, false)?;
        self.rewrite(|| async move {
            let mut tx = begin(&self.box_pool(owner).await?, owner.tenant).await?;
            if !collection_exists(&mut tx, owner, collection).await? {
                return Err(Error::NotFound);
            }
            if rights.is_empty() {
                sqlx::query(
                    "DELETE FROM collection_acl
                     WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3 AND identifier = $4",
                )
                .bind(owner.tenant.as_uuid())
                .bind(owner.account.as_uuid())
                .bind(collection.as_uuid())
                .bind(identifier)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            } else {
                sqlx::query(
                    "INSERT INTO collection_acl
                         (tenant_id, account_id, collection_id, identifier, free_busy, read, write, schedule, admin)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
                     ON CONFLICT (tenant_id, account_id, collection_id, identifier)
                     DO UPDATE SET free_busy = EXCLUDED.free_busy, read = EXCLUDED.read, write = EXCLUDED.write,
                                   schedule = EXCLUDED.schedule, admin = EXCLUDED.admin, updated_at = now()",
                )
                .bind(owner.tenant.as_uuid())
                .bind(owner.account.as_uuid())
                .bind(collection.as_uuid())
                .bind(identifier)
                .bind(rights.free_busy)
                .bind(rights.read)
                .bind(rights.write)
                .bind(rights.schedule)
                .bind(rights.admin)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            }
            crate::commits::commit(tx).await?;
            Ok(())
        })
        .await
    }

    async fn collection_rights(
        &self,
        owner: AccountScope,
        collection: CollectionId,
        identifiers: &[String],
    ) -> Result<CollectionRights> {
        validate_identifiers(identifiers)?;
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_db(owner).await?.pool, owner.tenant).await?;
            let row: RightsRow = sqlx::query_as(
                "SELECT coalesce(bool_or(free_busy), false), coalesce(bool_or(read), false),
                        coalesce(bool_or(write), false), coalesce(bool_or(schedule), false),
                        coalesce(bool_or(admin), false)
                 FROM collection_acl
                 WHERE tenant_id = $1 AND account_id = $2 AND collection_id = $3 AND identifier = ANY($4)",
            )
            .bind(owner.tenant.as_uuid())
            .bind(owner.account.as_uuid())
            .bind(collection.as_uuid())
            .bind(identifiers)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(rights(row).effective())
        })
        .await
    }

    async fn shared_with(
        &self,
        scope: AccountScope,
        identifiers: &[String],
    ) -> Result<Vec<SharedCollection>> {
        validate_identifiers(identifiers)?;
        self.idempotent(|| async move {
            let mut out: Vec<SharedCollection> = Vec::new();
            // Коллекции владельцев всех шардов: на каждом — один запрос с правами, сведёнными по коллекции.
            for db in self.all_shards().await? {
                let mut tx = begin(&db.pool, scope.tenant).await?;
                let rows: Vec<(Uuid, Uuid, bool, bool, bool, bool, bool)> = sqlx::query_as(
                    "SELECT account_id, collection_id, bool_or(free_busy), bool_or(read), bool_or(write),
                            bool_or(schedule), bool_or(admin)
                     FROM collection_acl
                     WHERE tenant_id = $1 AND identifier = ANY($2) AND account_id <> $3
                     GROUP BY account_id, collection_id",
                )
                .bind(scope.tenant.as_uuid())
                .bind(identifiers)
                .bind(scope.account.as_uuid())
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?;
                crate::commits::commit(tx).await?;
                out.extend(
                    rows.into_iter()
                        .map(|(owner, collection, f, r, w, s, a)| SharedCollection {
                            owner: AccountId::from_uuid(owner),
                            collection: CollectionId::from_uuid(collection),
                            rights: rights((f, r, w, s, a)).effective(),
                        }),
                );
            }
            out.sort_by_key(|c| (c.owner, c.collection));
            Ok(out)
        })
        .await
    }
}

#[async_trait]
impl DelegateStore for PgMeta {
    async fn delegates(&self, owner: AccountScope) -> Result<Vec<Delegate>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, owner.tenant).await?;
            if accounts_found(&mut tx, owner.tenant, &[*owner.account.as_uuid()]).await? == 0 {
                return Err(Error::NotFound);
            }
            let rows: Vec<(Uuid, String, bool)> = sqlx::query_as(
                "SELECT delegate_id, level, schedule_on_behalf FROM account_delegates
                 WHERE tenant_id = $1 AND account_id = $2 ORDER BY delegate_id",
            )
            .bind(owner.tenant.as_uuid())
            .bind(owner.account.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            rows.into_iter()
                .map(|(delegate, level, schedule_on_behalf)| {
                    Ok(Delegate {
                        delegate: AccountId::from_uuid(delegate),
                        level: parse_level(&level)?,
                        schedule_on_behalf,
                    })
                })
                .collect()
        })
        .await
    }

    async fn set_delegate(&self, owner: AccountScope, delegate: Delegate) -> Result<()> {
        if delegate.delegate == owner.account {
            return Err(Error::invalid("учётная запись — сама себе представитель"));
        }
        self.rewrite(|| async move {
            let mut tx = begin(&self.pool, owner.tenant).await?;
            let ids = [*owner.account.as_uuid(), *delegate.delegate.as_uuid()];
            if accounts_found(&mut tx, owner.tenant, &ids).await? != 2 {
                return Err(Error::NotFound);
            }
            sqlx::query(
                "INSERT INTO account_delegates (tenant_id, account_id, delegate_id, level, schedule_on_behalf)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (tenant_id, account_id, delegate_id)
                 DO UPDATE SET level = EXCLUDED.level, schedule_on_behalf = EXCLUDED.schedule_on_behalf,
                               updated_at = now()",
            )
            .bind(owner.tenant.as_uuid())
            .bind(owner.account.as_uuid())
            .bind(delegate.delegate.as_uuid())
            .bind(level_str(delegate.level))
            .bind(delegate.schedule_on_behalf)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(())
        })
        .await
    }

    async fn remove_delegate(&self, owner: AccountScope, delegate: AccountId) -> Result<()> {
        self.rewrite(|| async move {
            let mut tx = begin(&self.pool, owner.tenant).await?;
            if accounts_found(&mut tx, owner.tenant, &[*owner.account.as_uuid()]).await? == 0 {
                return Err(Error::NotFound);
            }
            sqlx::query(
                "DELETE FROM account_delegates WHERE tenant_id = $1 AND account_id = $2 AND delegate_id = $3",
            )
            .bind(owner.tenant.as_uuid())
            .bind(owner.account.as_uuid())
            .bind(delegate.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(())
        })
        .await
    }

    async fn delegations(&self, delegate: AccountScope) -> Result<Vec<Delegation>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, delegate.tenant).await?;
            let rows: Vec<(Uuid, String, bool)> = sqlx::query_as(
                "SELECT account_id, level, schedule_on_behalf FROM account_delegates
                 WHERE tenant_id = $1 AND delegate_id = $2 ORDER BY account_id",
            )
            .bind(delegate.tenant.as_uuid())
            .bind(delegate.account.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            rows.into_iter()
                .map(|(owner, level, schedule_on_behalf)| {
                    Ok(Delegation {
                        owner: AccountId::from_uuid(owner),
                        level: parse_level(&level)?,
                        schedule_on_behalf,
                    })
                })
                .collect()
        })
        .await
    }
}

#[async_trait]
impl ResourceStore for PgMeta {
    async fn booking_policy(&self, scope: AccountScope) -> Result<Option<BookingPolicy>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, scope.tenant).await?;
            let row: Option<(Option<Json<BookingPolicy>>,)> = sqlx::query_as(
                "SELECT booking_policy FROM accounts WHERE tenant_id = $1 AND account_id = $2",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            let (policy,) = row.ok_or(Error::NotFound)?;
            Ok(policy.map(|p| p.0))
        })
        .await
    }

    async fn set_booking_policy(
        &self,
        scope: AccountScope,
        policy: Option<BookingPolicy>,
    ) -> Result<()> {
        if let Some(p) = &policy {
            p.validate()?;
        }
        let policy = &policy;
        self.rewrite(|| async move {
            let mut tx = begin(&self.pool, scope.tenant).await?;
            let kind: Option<String> = sqlx::query_scalar(
                "SELECT kind FROM accounts WHERE tenant_id = $1 AND account_id = $2 FOR UPDATE",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            match kind.as_deref() {
                None => return Err(Error::NotFound),
                Some("resource") => {}
                Some(_) => {
                    return Err(Error::invalid(
                        "правила бронирования — только у учётной записи-ресурса",
                    ));
                }
            }
            sqlx::query(
                "UPDATE accounts SET booking_policy = $3, updated_at = now()
                 WHERE tenant_id = $1 AND account_id = $2",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(policy.as_ref().map(Json))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(())
        })
        .await
    }

    async fn resources(&self, tenant: TenantId) -> Result<Vec<AccountId>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let ids: Vec<Uuid> = sqlx::query_scalar(
                "SELECT account_id FROM accounts WHERE tenant_id = $1 AND kind = 'resource'
                 ORDER BY account_id",
            )
            .bind(tenant.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(ids.into_iter().map(AccountId::from_uuid).collect())
        })
        .await
    }
}

#[async_trait]
impl SharingPolicyStore for PgMeta {
    async fn sharing_policy(&self, tenant: TenantId) -> Result<SharingPolicy> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let policy: Option<Json<SharingPolicy>> =
                sqlx::query_scalar("SELECT policy FROM sharing_policies WHERE tenant_id = $1")
                    .bind(tenant.as_uuid())
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(policy.map(|p| p.0).unwrap_or_default())
        })
        .await
    }

    async fn set_sharing_policy(&self, tenant: TenantId, policy: SharingPolicy) -> Result<()> {
        let policy = &policy;
        self.rewrite(|| async move {
            let mut tx = begin(&self.pool, tenant).await?;
            sqlx::query(
                "INSERT INTO sharing_policies (tenant_id, policy) VALUES ($1, $2)
                 ON CONFLICT (tenant_id) DO UPDATE SET policy = EXCLUDED.policy, updated_at = now()",
            )
            .bind(tenant.as_uuid())
            .bind(Json(policy))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(())
        })
        .await
    }
}

// ---------- Уведомления о доступе (META-83, CTR-86) ----------

/// Права уведомления → битовая маска столбцов `old_rights`, `new_rights` (миграция 0042).
fn rights_bits(r: CollectionRights) -> i16 {
    i16::from(r.free_busy)
        | (i16::from(r.read) << 1)
        | (i16::from(r.write) << 2)
        | (i16::from(r.schedule) << 3)
        | (i16::from(r.admin) << 4)
}

fn bits_rights(b: i16) -> CollectionRights {
    CollectionRights {
        free_busy: b & 1 != 0,
        read: b & 2 != 0,
        write: b & 4 != 0,
        schedule: b & 8 != 0,
        admin: b & 16 != 0,
    }
}

fn object_type_str(t: SharedObjectType) -> &'static str {
    match t {
        SharedObjectType::Calendar => "calendar",
        SharedObjectType::TaskList => "task_list",
        SharedObjectType::AddressBook => "address_book",
    }
}

fn parse_object_type(s: &str) -> Result<SharedObjectType> {
    match s {
        "calendar" => Ok(SharedObjectType::Calendar),
        "task_list" => Ok(SharedObjectType::TaskList),
        "address_book" => Ok(SharedObjectType::AddressBook),
        other => Err(Error::internal(format!(
            "share_notifications.object_type: {other}"
        ))),
    }
}

#[derive(FromRow)]
struct NoticeRow {
    notification_id: Uuid,
    created: OffsetDateTime,
    changed_by_name: String,
    changed_by_email: Option<String>,
    changed_by_account: Option<Uuid>,
    object_type: String,
    owner_id: Uuid,
    collection_id: Uuid,
    old_rights: i16,
    new_rights: i16,
    name: String,
}

impl NoticeRow {
    fn into_notice(self) -> Result<ShareNotification> {
        Ok(ShareNotification {
            id: ShareNotificationId::from_uuid(self.notification_id),
            created: self.created,
            changed_by: ShareChangedBy {
                name: self.changed_by_name,
                email: self.changed_by_email,
                account: self.changed_by_account.map(AccountId::from_uuid),
            },
            object_type: parse_object_type(&self.object_type)?,
            owner: AccountId::from_uuid(self.owner_id),
            collection: CollectionId::from_uuid(self.collection_id),
            old_rights: bits_rights(self.old_rights),
            new_rights: bits_rights(self.new_rights),
            name: self.name,
        })
    }
}

/// Следующее состояние уведомлений получателя `scope` (строка состояния блокируется до фиксации: шаги журнала
/// получателя идут подряд).
async fn bump_notice_state(tx: &mut PgConnection, scope: AccountScope) -> Result<i64> {
    sqlx::query_scalar(
        "INSERT INTO share_notification_state (tenant_id, account_id, state) VALUES ($1, $2, 1)
         ON CONFLICT (tenant_id, account_id) DO UPDATE SET state = share_notification_state.state + 1
         RETURNING state",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err)
}

/// Шаг `state` журнала получателя `scope`: запись (`created`) или удаление уведомления `id`.
async fn log_notice(
    tx: &mut PgConnection,
    scope: AccountScope,
    state: i64,
    id: ShareNotificationId,
    created: bool,
    notice_created: OffsetDateTime,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO share_notification_log (tenant_id, account_id, state, notification_id, created, notice_created)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(state)
    .bind(id.as_uuid())
    .bind(created)
    .bind(notice_created)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Вытеснение сверх [`MAX_NOTICES`] (CTR-90) в транзакции записи после шага `state`: старшие по (`created`, `id`)
/// удаляются шагами журнала, как при удалении. Состояние получателя уже заблокировано, поэтому уведомления,
/// заблокированные удалением или сроком хранения (они ждут это состояние), пропускаются (`SKIP LOCKED`) и не
/// считаются: их удалит тот, кто держит; ожидание здесь замкнуло бы круг. Возвращает последнее состояние.
async fn evict_notices(tx: &mut PgConnection, scope: AccountScope, state: i64) -> Result<i64> {
    let keep = i64::try_from(MAX_NOTICES).unwrap_or(i64::MAX);
    let mut evicted: Vec<(Uuid, OffsetDateTime)> = sqlx::query_as(
        "DELETE FROM share_notifications n USING (
             SELECT notification_id FROM share_notifications
             WHERE tenant_id = $1 AND account_id = $2
             ORDER BY created DESC, notification_id DESC
             OFFSET $3
             FOR UPDATE SKIP LOCKED
         ) o
         WHERE n.tenant_id = $1 AND n.account_id = $2 AND n.notification_id = o.notification_id
         RETURNING n.notification_id, n.created",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(keep)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    if evicted.is_empty() {
        return Ok(state);
    }
    evicted.sort_unstable_by_key(|(id, created)| (*created, *id));
    let mut state = state;
    for (id, created) in evicted {
        state = bump_notice_state(tx, scope).await?;
        log_notice(
            tx,
            scope,
            state,
            ShareNotificationId::from_uuid(id),
            false,
            created,
        )
        .await?;
    }
    Ok(state)
}

/// Таблицы миграции 0042. Запись — состояние получателя первым (блокировка строки), затем уведомление (вытеснение
/// не ждёт чужих блокировок, [`evict_notices`]); удаление и срок хранения — уведомление первым, затем состояние:
/// порядок блокировок без взаимного ожидания.
#[async_trait]
impl ShareNotificationStore for PgMeta {
    /// Повтор после обрыва во время COMMIT ответил бы `Conflict` — один раз.
    async fn record(
        &self,
        recipient: AccountScope,
        notification: ShareNotification,
    ) -> Result<u64> {
        notification.validate()?;
        let n = &notification;
        self.once(async move {
            let mut tx = begin(&self.pool, recipient.tenant).await?;
            let state = bump_notice_state(&mut tx, recipient).await?;
            let inserted = sqlx::query(
                "INSERT INTO share_notifications (tenant_id, account_id, notification_id, created,
                     changed_by_name, changed_by_email, changed_by_account, object_type, owner_id, collection_id,
                     old_rights, new_rights, name)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
                 ON CONFLICT (tenant_id, account_id, notification_id) DO NOTHING",
            )
            .bind(recipient.tenant.as_uuid())
            .bind(recipient.account.as_uuid())
            .bind(n.id.as_uuid())
            .bind(n.created)
            .bind(&n.changed_by.name)
            .bind(&n.changed_by.email)
            .bind(n.changed_by.account.map(|a| *a.as_uuid()))
            .bind(object_type_str(n.object_type))
            .bind(n.owner.as_uuid())
            .bind(n.collection.as_uuid())
            .bind(rights_bits(n.old_rights))
            .bind(rights_bits(n.new_rights))
            .bind(&n.name)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?
            .rows_affected();
            if inserted == 0 {
                // Откат: состояние не растёт.
                return Err(Error::Conflict);
            }
            log_notice(&mut tx, recipient, state, n.id, true, n.created).await?;
            let state = evict_notices(&mut tx, recipient, state).await?;
            crate::commits::commit(tx).await?;
            to_u64(state)
        })
        .await
    }

    async fn notifications(&self, scope: AccountScope) -> Result<Vec<ShareNotification>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, scope.tenant).await?;
            let rows: Vec<NoticeRow> = sqlx::query_as(
                "SELECT notification_id, created, changed_by_name, changed_by_email, changed_by_account,
                        object_type, owner_id, collection_id, old_rights, new_rights, name
                 FROM share_notifications WHERE tenant_id = $1 AND account_id = $2
                 ORDER BY created, notification_id",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            rows.into_iter().map(NoticeRow::into_notice).collect()
        })
        .await
    }

    async fn notification(
        &self,
        scope: AccountScope,
        id: ShareNotificationId,
    ) -> Result<Option<ShareNotification>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, scope.tenant).await?;
            let row: Option<NoticeRow> = sqlx::query_as(
                "SELECT notification_id, created, changed_by_name, changed_by_email, changed_by_account,
                        object_type, owner_id, collection_id, old_rights, new_rights, name
                 FROM share_notifications WHERE tenant_id = $1 AND account_id = $2 AND notification_id = $3",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            row.map(NoticeRow::into_notice).transpose()
        })
        .await
    }

    /// Удаление с ответом `NotFound` об отсутствии — один раз (повтор ответил бы `NotFound`).
    async fn destroy_notification(
        &self,
        scope: AccountScope,
        id: ShareNotificationId,
    ) -> Result<u64> {
        self.once(async move {
            let mut tx = begin(&self.pool, scope.tenant).await?;
            let created: Option<OffsetDateTime> = sqlx::query_scalar(
                "DELETE FROM share_notifications WHERE tenant_id = $1 AND account_id = $2 AND notification_id = $3
                 RETURNING created",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            let Some(created) = created else {
                return Err(Error::NotFound);
            };
            let state = bump_notice_state(&mut tx, scope).await?;
            log_notice(&mut tx, scope, state, id, false, created).await?;
            crate::commits::commit(tx).await?;
            to_u64(state)
        })
        .await
    }

    async fn notification_state(&self, scope: AccountScope) -> Result<u64> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, scope.tenant).await?;
            let state: Option<i64> = sqlx::query_scalar(
                "SELECT state FROM share_notification_state WHERE tenant_id = $1 AND account_id = $2",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            to_u64(state.unwrap_or(0))
        })
        .await
    }

    /// У уведомления после `since` не больше двух шагов журнала (запись и удаление): `2 * limit + 1` строк хватает,
    /// чтобы набрать `limit` уведомлений и узнать о следующем.
    async fn notification_changes(
        &self,
        scope: AccountScope,
        since: u64,
        limit: usize,
    ) -> Result<ShareNotificationChanges> {
        if limit == 0 {
            return Err(Error::invalid("изменения уведомлений о доступе: limit = 0"));
        }
        let since_i = i64::try_from(since).unwrap_or(i64::MAX);
        let rows_limit = i64::try_from(limit)
            .unwrap_or(i64::MAX)
            .saturating_mul(2)
            .saturating_add(1);
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, scope.tenant).await?;
            let (current, floor): (i64, i64) = sqlx::query_as(
                "SELECT state, floor FROM share_notification_state WHERE tenant_id = $1 AND account_id = $2",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?
            .unwrap_or((0, 0));
            if since_i > current || since_i < floor {
                return Err(Error::Conflict);
            }
            let rows: Vec<(i64, Uuid, bool)> = sqlx::query_as(
                "SELECT state, notification_id, created FROM share_notification_log
                 WHERE tenant_id = $1 AND account_id = $2 AND state > $3
                 ORDER BY state
                 LIMIT $4",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(since_i)
            .bind(rows_limit)
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            // Первое и последнее изменение каждого уведомления после `since`, не более `limit` уведомлений.
            let mut seen: BTreeMap<ShareNotificationId, (bool, bool)> = BTreeMap::new();
            let mut out = ShareNotificationChanges {
                new_state: since,
                ..ShareNotificationChanges::default()
            };
            for (state, id, created) in rows {
                let id = ShareNotificationId::from_uuid(id);
                if !seen.contains_key(&id) && seen.len() == limit {
                    out.has_more = true;
                    break;
                }
                seen.entry(id)
                    .and_modify(|(_, last)| *last = created)
                    .or_insert((created, created));
                out.new_state = to_u64(state)?;
            }
            for (id, (first, last)) in seen {
                match (first, last) {
                    (true, true) => out.created.push(id),
                    (false, false) => out.destroyed.push(id),
                    // Записано и удалено после `since` — не входит.
                    _ => {}
                }
            }
            Ok(out)
        })
        .await
    }

    /// Одна системная транзакция. Сначала журнал: история уведомлений, удалённых до этого вызова и созданных
    /// раньше `before`, убирается (не более `limit` шагов), `floor` получателей поднимается. Затем до `limit`
    /// старших уведомлений удаляются, их удаление — шаги журнала получателей (видны в `changes` до следующего
    /// прохода). Число удалённых при повторе было бы другим — один раз.
    async fn expire_notifications(&self, before: OffsetDateTime, limit: usize) -> Result<usize> {
        if limit == 0 {
            return Ok(0);
        }
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.once(async move {
            let mut tx = begin_system(&self.pool).await?;
            sqlx::query(
                "WITH gone AS (
                     DELETE FROM share_notification_log l USING (
                         SELECT g.tenant_id, g.account_id, g.state FROM share_notification_log g
                         WHERE g.notice_created < $1 AND NOT EXISTS (
                             SELECT 1 FROM share_notifications n
                             WHERE n.tenant_id = g.tenant_id AND n.account_id = g.account_id
                               AND n.notification_id = g.notification_id)
                         ORDER BY g.notice_created
                         LIMIT $2
                     ) t
                     WHERE l.tenant_id = t.tenant_id AND l.account_id = t.account_id AND l.state = t.state
                     RETURNING l.tenant_id, l.account_id, l.state
                 )
                 UPDATE share_notification_state s SET floor = greatest(s.floor, g.floor)
                 FROM (SELECT tenant_id, account_id, max(state) AS floor FROM gone GROUP BY tenant_id, account_id) g
                 WHERE s.tenant_id = g.tenant_id AND s.account_id = g.account_id",
            )
            .bind(before)
            .bind(limit)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            let removed = sqlx::query(
                "WITH old AS (
                     DELETE FROM share_notifications n USING (
                         SELECT tenant_id, account_id, notification_id FROM share_notifications
                         WHERE created < $1
                         ORDER BY created, notification_id
                         LIMIT $2
                     ) o
                     WHERE n.tenant_id = o.tenant_id AND n.account_id = o.account_id
                       AND n.notification_id = o.notification_id
                     RETURNING n.tenant_id, n.account_id, n.notification_id, n.created
                 ), numbered AS (
                     SELECT tenant_id, account_id, notification_id, created,
                            row_number() OVER (PARTITION BY tenant_id, account_id
                                               ORDER BY created, notification_id) AS k
                     FROM old
                 ), bumped AS (
                     UPDATE share_notification_state s SET state = s.state + c.cnt
                     FROM (SELECT tenant_id, account_id, count(*) AS cnt FROM old GROUP BY tenant_id, account_id) c
                     WHERE s.tenant_id = c.tenant_id AND s.account_id = c.account_id
                     RETURNING s.tenant_id, s.account_id, s.state - c.cnt AS base
                 )
                 INSERT INTO share_notification_log (tenant_id, account_id, state, notification_id, created,
                                                     notice_created)
                 SELECT n.tenant_id, n.account_id, b.base + n.k, n.notification_id, false, n.created
                 FROM numbered n JOIN bumped b ON b.tenant_id = n.tenant_id AND b.account_id = n.account_id",
            )
            .bind(before)
            .bind(limit)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?
            .rows_affected();
            crate::commits::commit(tx).await?;
            Ok(usize::try_from(removed).unwrap_or(usize::MAX))
        })
        .await
    }
}
