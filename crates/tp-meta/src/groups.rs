//! Группы рассылки и очередь модерации на PostgreSQL (META-21; CONTRACTS.md#Groups; SRS 4.5).
//!
//! Группа — строка `groups` и списки `group_members`, `group_senders`, `group_moderators` с позицией: группа
//! читается такой, какой записана; [`GroupStore::put`] заменяет её целиком одной транзакцией.
//! Раскрытие — один запрос с рекурсивным CTE: `UNION` (без `ALL`) посещает каждую группу один раз, поэтому
//! циклы не зацикливают; неизвестные вложенные группы не дают участников.

use std::collections::BTreeMap;

use async_trait::async_trait;
use sqlx::{FromRow, PgConnection};
use time::OffsetDateTime;
use tp_contracts::groups::{
    DistributionGroup, GroupMember, GroupStore, HeldMessage, SenderRestriction,
};
use tp_contracts::{AccountId, BlobHash, BlobKey, Error, Gen, GroupId, HeldId, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::pool::with_tenant;
use crate::schema::{db_err, to_i64, to_u32, to_u64};

#[derive(FromRow)]
struct GroupRow {
    group_id: Uuid,
    name: String,
    address: Option<String>,
    external_id: Option<String>,
    senders: String,
    hidden_from_gal: bool,
}

/// Элемент списка группы: `list` — `member`, `sender`, `moderator`; ровно одно из `account_id`, `group_ref`.
#[derive(FromRow)]
struct LinkRow {
    group_id: Uuid,
    list: String,
    account_id: Option<Uuid>,
    group_ref: Option<Uuid>,
}

#[derive(FromRow)]
struct HeldRow {
    held_id: Uuid,
    group_id: Uuid,
    sender: String,
    sender_account: Option<Uuid>,
    message_id: Option<String>,
    blob_hash: Vec<u8>,
    blob_gen: i64,
    size: i64,
    held_at: OffsetDateTime,
}

const HELD_COLUMNS: &str =
    "held_id, group_id, sender, sender_account, message_id, blob_hash, blob_gen, size, held_at";

impl HeldRow {
    fn into_message(self, tenant: TenantId) -> Result<HeldMessage> {
        let hash: [u8; 32] = self
            .blob_hash
            .try_into()
            .map_err(|_| Error::internal("длина хэша в group_held"))?;
        Ok(HeldMessage {
            id: HeldId::from_uuid(self.held_id),
            group: GroupId::from_uuid(self.group_id),
            sender: self.sender,
            sender_account: self.sender_account.map(AccountId::from_uuid),
            message_id: self.message_id,
            blob: BlobKey {
                tenant,
                hash: BlobHash(hash),
                generation: Gen(to_u32(self.blob_gen)?),
            },
            size: to_u64(self.size)?,
            held_at: self.held_at,
        })
    }
}

fn senders_kind(s: &SenderRestriction) -> &'static str {
    match s {
        SenderRestriction::Anyone => "anyone",
        SenderRestriction::Internal => "internal",
        SenderRestriction::Only { .. } => "only",
    }
}

/// Группы организации: `ids` — только эти (`None` — все), по возрастанию идентификатора.
async fn load(
    tx: &mut PgConnection,
    tenant: TenantId,
    ids: Option<Vec<Uuid>>,
) -> Result<Vec<DistributionGroup>> {
    let rows: Vec<GroupRow> = sqlx::query_as(
        "SELECT group_id, name, address, external_id, senders, hidden_from_gal FROM groups
         WHERE tenant_id = $1 AND ($2::uuid[] IS NULL OR group_id = ANY($2))
         ORDER BY group_id",
    )
    .bind(tenant.as_uuid())
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let links: Vec<LinkRow> = sqlx::query_as(
        "SELECT group_id, 'member' AS list, pos, account_id, member_group_id AS group_ref
         FROM group_members WHERE tenant_id = $1 AND ($2::uuid[] IS NULL OR group_id = ANY($2))
         UNION ALL
         SELECT group_id, 'sender', pos, account_id, sender_group_id
         FROM group_senders WHERE tenant_id = $1 AND ($2::uuid[] IS NULL OR group_id = ANY($2))
         UNION ALL
         SELECT group_id, 'moderator', pos, account_id, NULL::uuid
         FROM group_moderators WHERE tenant_id = $1 AND ($2::uuid[] IS NULL OR group_id = ANY($2))
         ORDER BY group_id, list, pos",
    )
    .bind(tenant.as_uuid())
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;

    let mut groups: BTreeMap<Uuid, (DistributionGroup, Vec<AccountId>, Vec<GroupId>)> =
        BTreeMap::new();
    for r in rows {
        let senders = match r.senders.as_str() {
            "anyone" => SenderRestriction::Anyone,
            "internal" => SenderRestriction::Internal,
            "only" => SenderRestriction::Only {
                accounts: Vec::new(),
                groups: Vec::new(),
            },
            other => {
                return Err(Error::internal(format!(
                    "неизвестное ограничение отправителей: {other}"
                )));
            }
        };
        let group = DistributionGroup {
            id: GroupId::from_uuid(r.group_id),
            tenant,
            name: r.name,
            address: r.address,
            external_id: r.external_id,
            members: Vec::new(),
            senders,
            moderators: Vec::new(),
            hidden_from_gal: r.hidden_from_gal,
        };
        groups.insert(r.group_id, (group, Vec::new(), Vec::new()));
    }
    for l in links {
        let Some((group, accounts, sender_groups)) = groups.get_mut(&l.group_id) else {
            continue; // список группы, созданной после чтения строк групп
        };
        let account = l.account_id.map(AccountId::from_uuid);
        let nested = l.group_ref.map(GroupId::from_uuid);
        match (l.list.as_str(), account, nested) {
            ("member", Some(a), _) => group.members.push(GroupMember::Account(a)),
            ("member", None, Some(g)) => group.members.push(GroupMember::Group(g)),
            ("sender", Some(a), _) => accounts.push(a),
            ("sender", None, Some(g)) => sender_groups.push(g),
            ("moderator", Some(a), _) => group.moderators.push(a),
            _ => return Err(Error::internal("некорректный элемент списка группы")),
        }
    }
    Ok(groups
        .into_values()
        .map(|(mut group, accounts, sender_groups)| {
            if let SenderRestriction::Only {
                accounts: a,
                groups: g,
            } = &mut group.senders
            {
                *a = accounts;
                *g = sender_groups;
            }
            group
        })
        .collect())
}

/// Группа по условию на строку `groups` (`$1` — организация, `$2` — значение).
async fn find(
    meta: &PgMeta,
    tenant: TenantId,
    condition: &'static str,
    value: String,
) -> Result<Option<DistributionGroup>> {
    with_tenant(&meta.pool, tenant, move |tx| {
        Box::pin(async move {
            let id: Option<Uuid> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT group_id FROM groups WHERE tenant_id = $1 AND {condition}"
            )))
            .bind(tenant.as_uuid())
            .bind(value)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            match id {
                None => Ok(None),
                Some(id) => Ok(load(tx, tenant, Some(vec![id])).await?.into_iter().next()),
            }
        })
    })
    .await
}

/// Позиции списка: `(account, group)` — ровно одно задано.
fn split(
    items: impl Iterator<Item = (Option<Uuid>, Option<Uuid>)>,
) -> (Vec<Option<Uuid>>, Vec<Option<Uuid>>) {
    items.unzip()
}

#[async_trait]
impl GroupStore for PgMeta {
    async fn get(&self, tenant: TenantId, id: GroupId) -> Result<Option<DistributionGroup>> {
        with_tenant(&self.pool, tenant, move |tx| {
            Box::pin(async move {
                Ok(load(tx, tenant, Some(vec![*id.as_uuid()]))
                    .await?
                    .into_iter()
                    .next())
            })
        })
        .await
    }

    async fn by_address(
        &self,
        tenant: TenantId,
        address: &str,
    ) -> Result<Option<DistributionGroup>> {
        find(
            self,
            tenant,
            "address IS NOT NULL AND lower(address) = lower($2)",
            address.to_owned(),
        )
        .await
    }

    async fn by_external_id(
        &self,
        tenant: TenantId,
        external_id: &str,
    ) -> Result<Option<DistributionGroup>> {
        find(self, tenant, "external_id = $2", external_id.to_owned()).await
    }

    async fn list(&self, tenant: TenantId) -> Result<Vec<DistributionGroup>> {
        with_tenant(&self.pool, tenant, move |tx| {
            Box::pin(async move { load(tx, tenant, None).await })
        })
        .await
    }

    async fn put(&self, group: DistributionGroup) -> Result<()> {
        group.validate()?;
        let tenant = group.tenant;
        with_tenant(&self.pool, tenant, move |tx| {
            Box::pin(async move {
                let t = *tenant.as_uuid();
                let g = *group.id.as_uuid();
                sqlx::query(
                    "INSERT INTO groups (tenant_id, group_id, name, address, external_id, senders, hidden_from_gal)
                     VALUES ($1, $2, $3, $4, $5, $6, $7)
                     ON CONFLICT (tenant_id, group_id) DO UPDATE SET
                         name = EXCLUDED.name, address = EXCLUDED.address, external_id = EXCLUDED.external_id,
                         senders = EXCLUDED.senders, hidden_from_gal = EXCLUDED.hidden_from_gal,
                         updated_at = now()",
                )
                .bind(t)
                .bind(g)
                .bind(&group.name)
                .bind(&group.address)
                .bind(&group.external_id)
                .bind(senders_kind(&group.senders))
                .bind(group.hidden_from_gal)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                for table in ["group_members", "group_senders", "group_moderators"] {
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "DELETE FROM {table} WHERE tenant_id = $1 AND group_id = $2"
                    )))
                    .bind(t)
                    .bind(g)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                }

                let (accounts, nested) = split(group.members.iter().map(|m| match m {
                    GroupMember::Account(a) => (Some(*a.as_uuid()), None),
                    GroupMember::Group(n) => (None, Some(*n.as_uuid())),
                }));
                let senders: Vec<(Option<Uuid>, Option<Uuid>)> = match &group.senders {
                    SenderRestriction::Only { accounts, groups } => accounts
                        .iter()
                        .map(|a| (Some(*a.as_uuid()), None))
                        .chain(groups.iter().map(|g| (None, Some(*g.as_uuid()))))
                        .collect(),
                    _ => Vec::new(),
                };
                let (sender_accounts, sender_groups) = split(senders.into_iter());
                let moderators: Vec<Uuid> = group.moderators.iter().map(|a| *a.as_uuid()).collect();
                for (table, column, a, n) in [
                    ("group_members", "member_group_id", accounts, nested),
                    ("group_senders", "sender_group_id", sender_accounts, sender_groups),
                ] {
                    if a.is_empty() {
                        continue;
                    }
                    sqlx::query(sqlx::AssertSqlSafe(format!(
                        "INSERT INTO {table} (tenant_id, group_id, pos, account_id, {column})
                         SELECT $1, $2, (n - 1)::int, a, g FROM unnest($3::uuid[], $4::uuid[])
                             WITH ORDINALITY AS u(a, g, n)"
                    )))
                    .bind(t)
                    .bind(g)
                    .bind(a)
                    .bind(n)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                }
                if !moderators.is_empty() {
                    sqlx::query(
                        "INSERT INTO group_moderators (tenant_id, group_id, pos, account_id)
                         SELECT $1, $2, (n - 1)::int, a FROM unnest($3::uuid[]) WITH ORDINALITY AS u(a, n)",
                    )
                    .bind(t)
                    .bind(g)
                    .bind(moderators)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                }
                Ok(())
            })
        })
        .await
    }

    async fn remove(&self, tenant: TenantId, id: GroupId) -> Result<Vec<HeldMessage>> {
        with_tenant(&self.pool, tenant, move |tx| {
            Box::pin(async move {
                let rows: Vec<HeldRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "WITH h AS (DELETE FROM group_held WHERE tenant_id = $1 AND group_id = $2
                                RETURNING {HELD_COLUMNS})
                     SELECT * FROM h ORDER BY held_at, held_id"
                )))
                .bind(tenant.as_uuid())
                .bind(id.as_uuid())
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?;
                sqlx::query("DELETE FROM groups WHERE tenant_id = $1 AND group_id = $2")
                    .bind(tenant.as_uuid())
                    .bind(id.as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                rows.into_iter().map(|r| r.into_message(tenant)).collect()
            })
        })
        .await
    }

    async fn expand(&self, tenant: TenantId, id: GroupId) -> Result<Vec<AccountId>> {
        let (found, accounts): (bool, Vec<Uuid>) = with_tenant(&self.pool, tenant, move |tx| {
            Box::pin(async move {
                sqlx::query_as(
                    "WITH RECURSIVE tree(g) AS (
                         SELECT group_id FROM groups WHERE tenant_id = $1 AND group_id = $2
                         UNION
                         SELECT m.member_group_id FROM tree
                         JOIN group_members m ON m.tenant_id = $1 AND m.group_id = tree.g
                         WHERE m.member_group_id IS NOT NULL)
                     SELECT EXISTS (SELECT 1 FROM tree),
                            ARRAY(SELECT DISTINCT m.account_id FROM tree
                                  JOIN group_members m ON m.tenant_id = $1 AND m.group_id = tree.g
                                  WHERE m.account_id IS NOT NULL ORDER BY 1)",
                )
                .bind(tenant.as_uuid())
                .bind(id.as_uuid())
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)
            })
        })
        .await?;
        if !found {
            return Err(Error::NotFound);
        }
        let mut accounts: Vec<AccountId> = accounts.into_iter().map(AccountId::from_uuid).collect();
        // Порядок uuid в PostgreSQL совпадает с порядком байтов; сортировка — страховка контракта.
        accounts.sort();
        accounts.dedup();
        Ok(accounts)
    }

    async fn hold(&self, tenant: TenantId, message: HeldMessage) -> Result<()> {
        if message.blob.tenant != tenant {
            return Err(Error::invalid("объект письма другой организации"));
        }
        let size = to_i64(message.size)?;
        let inserted = with_tenant(&self.pool, tenant, move |tx| {
            Box::pin(async move {
                Ok(sqlx::query(
                    "INSERT INTO group_held (tenant_id, held_id, group_id, sender, sender_account, message_id,
                                             blob_hash, blob_gen, size, held_at)
                     SELECT $1, $2, $3, $4, $5, $6, $7, $8, $9, $10
                     WHERE EXISTS (SELECT 1 FROM groups WHERE tenant_id = $1 AND group_id = $3)",
                )
                .bind(tenant.as_uuid())
                .bind(message.id.as_uuid())
                .bind(message.group.as_uuid())
                .bind(&message.sender)
                .bind(message.sender_account.map(|a| *a.as_uuid()))
                .bind(&message.message_id)
                .bind(message.blob.hash.as_bytes().as_slice())
                .bind(i64::from(message.blob.generation.0))
                .bind(size)
                .bind(message.held_at)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?
                .rows_affected())
            })
        })
        .await?;
        if inserted == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    async fn held(
        &self,
        tenant: TenantId,
        group: Option<GroupId>,
        limit: usize,
    ) -> Result<Vec<HeldMessage>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows: Vec<HeldRow> = with_tenant(&self.pool, tenant, move |tx| {
            Box::pin(async move {
                sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "SELECT {HELD_COLUMNS} FROM group_held
                     WHERE tenant_id = $1 AND ($2::uuid IS NULL OR group_id = $2)
                     ORDER BY held_at, held_id LIMIT $3"
                )))
                .bind(tenant.as_uuid())
                .bind(group.map(|g| *g.as_uuid()))
                .bind(limit)
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)
            })
        })
        .await?;
        rows.into_iter().map(|r| r.into_message(tenant)).collect()
    }

    async fn take_held(&self, tenant: TenantId, id: HeldId) -> Result<HeldMessage> {
        let row: Option<HeldRow> = with_tenant(&self.pool, tenant, move |tx| {
            Box::pin(async move {
                sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "DELETE FROM group_held WHERE tenant_id = $1 AND held_id = $2 RETURNING {HELD_COLUMNS}"
                )))
                .bind(tenant.as_uuid())
                .bind(id.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)
            })
        })
        .await?;
        row.ok_or(Error::NotFound)?.into_message(tenant)
    }
}
