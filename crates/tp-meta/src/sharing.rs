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

use async_trait::async_trait;
use sqlx::PgConnection;
use sqlx::types::Json;
use tp_contracts::dav::CollectionId;
use tp_contracts::meta::AccountScope;
use tp_contracts::sharing::{
    BookingPolicy, CollectionAcl, CollectionAclEntry, CollectionRights, Delegate, DelegateLevel,
    DelegateStore, Delegation, ResourceStore, SharedCollection, SharingPolicy, SharingPolicyStore,
    validate_identifier,
};
use tp_contracts::{AccountId, Error, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::schema::{begin, db_err};
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
