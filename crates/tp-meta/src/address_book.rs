//! Адресная книга организации и личные списки отправителей на PostgreSQL (CONTRACTS.md#AddressBook,
//! #SenderLists; CTR-35, META-33): почтовые контакты, оргструктура, динамические группы, списки отправителей ящика.
//!
//! Данные — в основной БД (каталог организации, миграция `0030_address_book`). Проверки значений, дерево
//! подразделений, порядок членов и фильтр динамической группы — функции контракта (`check_unit`, `sort_members`,
//! `MemberFilter::matches`); состав динамической группы вычисляется при раскрытии по оргатрибутам и контактам.

use std::collections::BTreeMap;

use async_trait::async_trait;
use sqlx::types::Json;
use sqlx::{FromRow, PgConnection};
use tp_contracts::addressbook::{
    ContactStore, DynamicGroup, DynamicGroupStore, FilterTarget, MailContact, MemberFilter,
    Members, OrgAttributes, OrgMember, OrgStore, OrgUnit, check_unit, sort_members, unit_path,
};
use tp_contracts::meta::AccountScope;
use tp_contracts::senders::{SenderList, SenderLists};
use tp_contracts::{AccountId, ContactId, Error, GroupId, OrgUnitId, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::schema::{begin, db_err, to_u64};
use crate::shards::ShardId;

// ---------- Списки отправителей ----------

#[async_trait]
impl SenderLists for PgMeta {
    async fn sender_list(&self, scope: AccountScope) -> Result<SenderList> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, scope.tenant).await?;
            let row: Option<(Vec<String>, Vec<String>)> = sqlx::query_as(
                "SELECT safe, blocked FROM sender_lists WHERE tenant_id = $1 AND account_id = $2",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(row
                .map(|(safe, blocked)| SenderList { safe, blocked })
                .unwrap_or_default())
        })
        .await
    }

    /// Состояние растёт при каждой записи — повтор после обрыва дал бы другое, поэтому запись однократная.
    async fn put_sender_list(&self, scope: AccountScope, list: SenderList) -> Result<u64> {
        let list = list.normalized()?;
        self.once(async move {
            let mut tx = begin(&self.pool, scope.tenant).await?;
            let state: i64 = sqlx::query_scalar(
                "INSERT INTO sender_lists (tenant_id, account_id, state, safe, blocked) VALUES ($1, $2, 1, $3, $4)
                 ON CONFLICT (tenant_id, account_id) DO UPDATE
                     SET state = sender_lists.state + 1, safe = EXCLUDED.safe, blocked = EXCLUDED.blocked
                 RETURNING state",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(&list.safe)
            .bind(&list.blocked)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            to_u64(state)
        })
        .await
    }

    async fn sender_list_state(&self, scope: AccountScope) -> Result<u64> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, scope.tenant).await?;
            let state: Option<i64> = sqlx::query_scalar(
                "SELECT state FROM sender_lists WHERE tenant_id = $1 AND account_id = $2",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            state.map_or(Ok(0), to_u64)
        })
        .await
    }
}

// ---------- Контакты ----------

const CONTACT_COLUMNS: &str =
    "contact_id, tenant_id, display_name, email, external_id, attributes, groups, hidden_from_gal";

#[derive(FromRow)]
struct ContactRow {
    contact_id: Uuid,
    tenant_id: Uuid,
    display_name: String,
    email: String,
    external_id: Option<String>,
    attributes: Json<BTreeMap<String, String>>,
    groups: Vec<Uuid>,
    hidden_from_gal: bool,
}

impl From<ContactRow> for MailContact {
    fn from(r: ContactRow) -> Self {
        MailContact {
            id: ContactId::from_uuid(r.contact_id),
            tenant: TenantId::from_uuid(r.tenant_id),
            display_name: r.display_name,
            email: r.email,
            external_id: r.external_id,
            attributes: r.attributes.0,
            groups: r.groups.into_iter().map(GroupId::from_uuid).collect(),
            hidden_from_gal: r.hidden_from_gal,
        }
    }
}

impl PgMeta {
    /// Контакты организации `tenant` по условию `filter` (`$2` — его параметр, если есть), по `contact_id`.
    async fn contacts_where<'a>(
        &self,
        tenant: TenantId,
        filter: &'static str,
        arg: Option<ContactArg<'a>>,
    ) -> Result<Vec<MailContact>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let sql = format!(
                "SELECT {CONTACT_COLUMNS} FROM mail_contacts WHERE tenant_id = $1 {filter} ORDER BY contact_id"
            );
            let q = sqlx::query_as::<_, ContactRow>(sqlx::AssertSqlSafe(sql)).bind(tenant.as_uuid());
            let q = match arg {
                None => q,
                Some(ContactArg::Uuid(u)) => q.bind(u),
                Some(ContactArg::Text(t)) => q.bind(t),
            };
            let rows = q.fetch_all(&mut *tx).await.map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(rows.into_iter().map(MailContact::from).collect())
        })
        .await
    }
}

/// Параметр условия [`PgMeta::contacts_where`].
#[derive(Clone, Copy)]
enum ContactArg<'a> {
    Uuid(Uuid),
    Text(&'a str),
}

#[async_trait]
impl ContactStore for PgMeta {
    async fn contact(&self, tenant: TenantId, id: ContactId) -> Result<Option<MailContact>> {
        let found = self
            .contacts_where(
                tenant,
                "AND contact_id = $2",
                Some(ContactArg::Uuid(*id.as_uuid())),
            )
            .await?;
        Ok(found.into_iter().next())
    }

    async fn contact_by_address(
        &self,
        tenant: TenantId,
        email: &str,
    ) -> Result<Option<MailContact>> {
        let found = self
            .contacts_where(
                tenant,
                "AND lower(email) = lower($2)",
                Some(ContactArg::Text(email)),
            )
            .await?;
        Ok(found.into_iter().next())
    }

    async fn contact_by_external_id(
        &self,
        tenant: TenantId,
        external_id: &str,
    ) -> Result<Option<MailContact>> {
        let found = self
            .contacts_where(
                tenant,
                "AND external_id = $2",
                Some(ContactArg::Text(external_id)),
            )
            .await?;
        Ok(found.into_iter().next())
    }

    async fn contacts(&self, tenant: TenantId) -> Result<Vec<MailContact>> {
        self.contacts_where(tenant, "", None).await
    }

    /// Замена целиком по значению — повторяется. Занятый адрес или `external_id` — нарушение уникального
    /// индекса (`Conflict`).
    async fn put_contact(&self, mut contact: MailContact) -> Result<()> {
        contact.validate()?;
        contact.groups.sort_unstable();
        contact.groups.dedup();
        let contact = &contact;
        self.rewrite(|| async move {
            let mut tx = begin(&self.pool, contact.tenant).await?;
            let groups: Vec<Uuid> = contact.groups.iter().map(|g| *g.as_uuid()).collect();
            sqlx::query(
                "INSERT INTO mail_contacts (tenant_id, contact_id, display_name, email, external_id, attributes,
                                            groups, hidden_from_gal)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
                 ON CONFLICT (tenant_id, contact_id) DO UPDATE SET
                     display_name = EXCLUDED.display_name, email = EXCLUDED.email,
                     external_id = EXCLUDED.external_id, attributes = EXCLUDED.attributes,
                     groups = EXCLUDED.groups, hidden_from_gal = EXCLUDED.hidden_from_gal",
            )
            .bind(contact.tenant.as_uuid())
            .bind(contact.id.as_uuid())
            .bind(&contact.display_name)
            .bind(&contact.email)
            .bind(&contact.external_id)
            .bind(Json(&contact.attributes))
            .bind(&groups)
            .bind(contact.hidden_from_gal)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(())
        })
        .await
    }

    async fn remove_contact(&self, tenant: TenantId, id: ContactId) -> Result<()> {
        self.once(async move {
            let mut tx = begin(&self.pool, tenant).await?;
            let n =
                sqlx::query("DELETE FROM mail_contacts WHERE tenant_id = $1 AND contact_id = $2")
                    .bind(tenant.as_uuid())
                    .bind(id.as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?
                    .rows_affected();
            crate::commits::commit(tx).await?;
            if n == 0 { Err(Error::NotFound) } else { Ok(()) }
        })
        .await
    }

    async fn group_contacts(&self, tenant: TenantId, group: GroupId) -> Result<Vec<MailContact>> {
        self.contacts_where(
            tenant,
            "AND groups @> ARRAY[$2::uuid]",
            Some(ContactArg::Uuid(*group.as_uuid())),
        )
        .await
    }
}

// ---------- Оргструктура ----------

#[derive(FromRow)]
struct UnitRow {
    unit_id: Uuid,
    tenant_id: Uuid,
    name: String,
    parent_id: Option<Uuid>,
    external_id: Option<String>,
}

impl From<UnitRow> for OrgUnit {
    fn from(r: UnitRow) -> Self {
        OrgUnit {
            id: OrgUnitId::from_uuid(r.unit_id),
            tenant: TenantId::from_uuid(r.tenant_id),
            name: r.name,
            parent: r.parent_id.map(OrgUnitId::from_uuid),
            external_id: r.external_id,
        }
    }
}

#[derive(FromRow)]
struct OrgRow {
    account_id: Uuid,
    department_id: Option<Uuid>,
    title: Option<String>,
    manager_id: Option<Uuid>,
    seniority: Option<i32>,
    attributes: Json<BTreeMap<String, String>>,
}

impl OrgRow {
    fn member(self) -> OrgMember {
        OrgMember {
            account: AccountId::from_uuid(self.account_id),
            org: OrgAttributes {
                department: self.department_id.map(OrgUnitId::from_uuid),
                title: self.title,
                manager: self.manager_id.map(AccountId::from_uuid),
                seniority: self.seniority,
                attributes: self.attributes.0,
            },
        }
    }
}

const ORG_COLUMNS: &str = "account_id, department_id, title, manager_id, seniority, attributes";

/// Подразделения организации по `unit_id`; `lock` — с блокировкой строк до конца транзакции.
async fn units(tx: &mut PgConnection, tenant: TenantId, lock: bool) -> Result<Vec<OrgUnit>> {
    let sql = format!(
        "SELECT unit_id, tenant_id, name, parent_id, external_id FROM org_units WHERE tenant_id = $1
         ORDER BY unit_id{}",
        if lock { " FOR UPDATE" } else { "" }
    );
    let rows: Vec<UnitRow> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
        .bind(tenant.as_uuid())
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
    Ok(rows.into_iter().map(OrgUnit::from).collect())
}

/// Оргатрибуты всех учётных записей организации.
async fn org_rows(tx: &mut PgConnection, tenant: TenantId) -> Result<Vec<OrgMember>> {
    let rows: Vec<OrgRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {ORG_COLUMNS} FROM org_attributes WHERE tenant_id = $1 ORDER BY account_id"
    )))
    .bind(tenant.as_uuid())
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(rows.into_iter().map(OrgRow::member).collect())
}

/// Подразделение и все вышестоящие, начиная со своего (`FilterTarget::units`).
fn chain(units: &[OrgUnit], unit: Option<OrgUnitId>) -> Vec<OrgUnitId> {
    unit.map(|u| unit_path(units, u).iter().rev().map(|u| u.id).collect())
        .unwrap_or_default()
}

#[async_trait]
impl OrgStore for PgMeta {
    async fn org_units(&self, tenant: TenantId) -> Result<Vec<OrgUnit>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let out = units(&mut tx, tenant, false).await?;
            crate::commits::commit(tx).await?;
            Ok(out)
        })
        .await
    }

    /// Дерево проверяется под блокировкой подразделений организации: параллельная запись не создаст цикл.
    async fn put_org_unit(&self, unit: OrgUnit) -> Result<()> {
        let unit = &unit;
        self.rewrite(|| async move {
            let mut tx = begin(&self.pool, unit.tenant).await?;
            check_unit(&units(&mut tx, unit.tenant, true).await?, unit)?;
            sqlx::query(
                "INSERT INTO org_units (tenant_id, unit_id, name, parent_id, external_id) VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (tenant_id, unit_id) DO UPDATE SET
                     name = EXCLUDED.name, parent_id = EXCLUDED.parent_id, external_id = EXCLUDED.external_id",
            )
            .bind(unit.tenant.as_uuid())
            .bind(unit.id.as_uuid())
            .bind(&unit.name)
            .bind(unit.parent.map(|p| *p.as_uuid()))
            .bind(&unit.external_id)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(())
        })
        .await
    }

    /// `department` учётных записей сбрасывает внешний ключ; оставшиеся без полей строки удаляются.
    async fn remove_org_unit(&self, tenant: TenantId, id: OrgUnitId) -> Result<()> {
        self.once(async move {
            let mut tx = begin(&self.pool, tenant).await?;
            let all = units(&mut tx, tenant, true).await?;
            if !all.iter().any(|u| u.id == id) {
                return Err(Error::NotFound);
            }
            if all.iter().any(|u| u.parent == Some(id)) {
                return Err(Error::Conflict);
            }
            sqlx::query("DELETE FROM org_units WHERE tenant_id = $1 AND unit_id = $2")
                .bind(tenant.as_uuid())
                .bind(id.as_uuid())
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            sqlx::query(
                "DELETE FROM org_attributes WHERE tenant_id = $1 AND department_id IS NULL AND title IS NULL
                     AND manager_id IS NULL AND seniority IS NULL AND attributes = '{}'",
            )
            .bind(tenant.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(())
        })
        .await
    }

    async fn org(&self, tenant: TenantId, account: AccountId) -> Result<OrgAttributes> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let row: Option<OrgRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {ORG_COLUMNS} FROM org_attributes WHERE tenant_id = $1 AND account_id = $2"
            )))
            .bind(tenant.as_uuid())
            .bind(account.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(row.map(|r| r.member().org).unwrap_or_default())
        })
        .await
    }

    async fn set_org(
        &self,
        tenant: TenantId,
        account: AccountId,
        org: OrgAttributes,
    ) -> Result<()> {
        org.validate()?;
        if org.manager == Some(account) {
            return Err(Error::invalid(
                "org attributes: account cannot be its own manager",
            ));
        }
        let org = &org;
        self.rewrite(|| async move {
            let mut tx = begin(&self.pool, tenant).await?;
            if *org == OrgAttributes::default() {
                sqlx::query("DELETE FROM org_attributes WHERE tenant_id = $1 AND account_id = $2")
                    .bind(tenant.as_uuid())
                    .bind(account.as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                crate::commits::commit(tx).await?;
                return Ok(());
            }
            // Ссылки проверяются до записи: нарушение внешнего ключа — не `Invalid`.
            let (unit_ok, manager_ok): (bool, bool) = sqlx::query_as(
                "SELECT $2::uuid IS NULL OR EXISTS (SELECT 1 FROM org_units WHERE tenant_id = $1 AND unit_id = $2),
                        $3::uuid IS NULL OR EXISTS (SELECT 1 FROM accounts WHERE tenant_id = $1 AND account_id = $3)",
            )
            .bind(tenant.as_uuid())
            .bind(org.department.map(|d| *d.as_uuid()))
            .bind(org.manager.map(|m| *m.as_uuid()))
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            if !unit_ok {
                return Err(Error::invalid("org attributes: unknown department"));
            }
            if !manager_ok {
                return Err(Error::invalid("org attributes: unknown manager"));
            }
            sqlx::query(
                "INSERT INTO org_attributes (tenant_id, account_id, department_id, title, manager_id, seniority,
                                             attributes)
                 VALUES ($1, $2, $3, $4, $5, $6, $7)
                 ON CONFLICT (tenant_id, account_id) DO UPDATE SET
                     department_id = EXCLUDED.department_id, title = EXCLUDED.title,
                     manager_id = EXCLUDED.manager_id, seniority = EXCLUDED.seniority,
                     attributes = EXCLUDED.attributes",
            )
            .bind(tenant.as_uuid())
            .bind(account.as_uuid())
            .bind(org.department.map(|d| *d.as_uuid()))
            .bind(&org.title)
            .bind(org.manager.map(|m| *m.as_uuid()))
            .bind(org.seniority)
            .bind(Json(&org.attributes))
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(())
        })
        .await
    }

    async fn org_members(
        &self,
        tenant: TenantId,
        unit: Option<OrgUnitId>,
        subtree: bool,
    ) -> Result<Vec<OrgMember>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let all = units(&mut tx, tenant, false).await?;
            let members = org_rows(&mut tx, tenant).await?;
            crate::commits::commit(tx).await?;
            if unit.is_some_and(|u| !all.iter().any(|x| x.id == u)) {
                return Err(Error::NotFound);
            }
            let mut out: Vec<OrgMember> = members
                .into_iter()
                .filter(|m| match unit {
                    None => true,
                    Some(u) if subtree => chain(&all, m.org.department).contains(&u),
                    Some(u) => m.org.department == Some(u),
                })
                .collect();
            sort_members(&mut out);
            Ok(out)
        })
        .await
    }
}

// ---------- Динамические группы ----------

#[derive(FromRow)]
struct DynamicRow {
    group_id: Uuid,
    tenant_id: Uuid,
    filter: Json<MemberFilter>,
    contacts: bool,
}

impl From<DynamicRow> for DynamicGroup {
    fn from(r: DynamicRow) -> Self {
        DynamicGroup {
            group: GroupId::from_uuid(r.group_id),
            tenant: TenantId::from_uuid(r.tenant_id),
            filter: r.filter.0,
            contacts: r.contacts,
        }
    }
}

async fn dynamic_rows(
    tx: &mut PgConnection,
    tenant: TenantId,
    group: Option<GroupId>,
) -> Result<Vec<DynamicGroup>> {
    let rows: Vec<DynamicRow> = sqlx::query_as(
        "SELECT group_id, tenant_id, filter, contacts FROM dynamic_groups
         WHERE tenant_id = $1 AND ($2::uuid IS NULL OR group_id = $2) ORDER BY group_id",
    )
    .bind(tenant.as_uuid())
    .bind(group.map(|g| *g.as_uuid()))
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(rows.into_iter().map(DynamicGroup::from).collect())
}

#[async_trait]
impl DynamicGroupStore for PgMeta {
    async fn dynamic(&self, tenant: TenantId, group: GroupId) -> Result<Option<DynamicGroup>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let out = dynamic_rows(&mut tx, tenant, Some(group)).await?;
            crate::commits::commit(tx).await?;
            Ok(out.into_iter().next())
        })
        .await
    }

    async fn dynamic_groups(&self, tenant: TenantId) -> Result<Vec<DynamicGroup>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let out = dynamic_rows(&mut tx, tenant, None).await?;
            crate::commits::commit(tx).await?;
            Ok(out)
        })
        .await
    }

    /// Группа рассылки должна существовать (`GroupStore::put`), иначе — `NotFound`.
    async fn put_dynamic(&self, group: DynamicGroup) -> Result<()> {
        group.filter.validate()?;
        let group = &group;
        self.rewrite(|| async move {
            let mut tx = begin(&self.pool, group.tenant).await?;
            let n = sqlx::query(
                "INSERT INTO dynamic_groups (tenant_id, group_id, filter, contacts)
                 SELECT $1, $2, $3, $4 WHERE EXISTS (SELECT 1 FROM groups WHERE tenant_id = $1 AND group_id = $2)
                 ON CONFLICT (tenant_id, group_id) DO UPDATE SET filter = EXCLUDED.filter, contacts = EXCLUDED.contacts",
            )
            .bind(group.tenant.as_uuid())
            .bind(group.group.as_uuid())
            .bind(Json(&group.filter))
            .bind(group.contacts)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?
            .rows_affected();
            crate::commits::commit(tx).await?;
            if n == 0 { Err(Error::NotFound) } else { Ok(()) }
        })
        .await
    }

    async fn remove_dynamic(&self, tenant: TenantId, group: GroupId) -> Result<()> {
        self.once(async move {
            let mut tx = begin(&self.pool, tenant).await?;
            let n =
                sqlx::query("DELETE FROM dynamic_groups WHERE tenant_id = $1 AND group_id = $2")
                    .bind(tenant.as_uuid())
                    .bind(group.as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?
                    .rows_affected();
            crate::commits::commit(tx).await?;
            if n == 0 { Err(Error::NotFound) } else { Ok(()) }
        })
        .await
    }

    /// Фильтр, подразделения, оргатрибуты и контакты читаются одной транзакцией; отбор — `MemberFilter::matches`.
    async fn expand_dynamic(&self, tenant: TenantId, group: GroupId) -> Result<Members> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let g = dynamic_rows(&mut tx, tenant, Some(group))
                .await?
                .into_iter()
                .next()
                .ok_or(Error::NotFound)?;
            let all = units(&mut tx, tenant, false).await?;
            let members = org_rows(&mut tx, tenant).await?;
            let contacts: Vec<ContactRow> = if g.contacts {
                sqlx::query_as(sqlx::AssertSqlSafe(format!(
                    "SELECT {CONTACT_COLUMNS} FROM mail_contacts WHERE tenant_id = $1 ORDER BY contact_id"
                )))
                .bind(tenant.as_uuid())
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?
            } else {
                Vec::new()
            };
            crate::commits::commit(tx).await?;
            let accounts = members
                .iter()
                .filter(|m| {
                    let attributes = m.org.filter_attributes();
                    let units = chain(&all, m.org.department);
                    g.filter.matches(FilterTarget {
                        attributes: &attributes,
                        units: &units,
                    })
                })
                .map(|m| m.account)
                .collect();
            let contacts = contacts
                .into_iter()
                .filter(|c| {
                    g.filter.matches(FilterTarget {
                        attributes: &c.attributes.0,
                        units: &[],
                    })
                })
                .map(|c| ContactId::from_uuid(c.contact_id))
                .collect();
            Ok(Members { accounts, contacts })
        })
        .await
    }
}
