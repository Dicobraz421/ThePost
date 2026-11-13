//! Каталог организации на PostgreSQL (CONTRACTS.md#Directory; META-65): источник книги «Организация» узла без LDAP.
//! Учётные записи, группы и подразделения — строки основной БД, заведённые в консоли или через Admin API
//! (`AccountStore`, `GroupStore`, `OrgStore`).
//!
//! Идентификатор записи каталога (`external_id`) — идентификатор строки: `AccountId`, `GroupId`, `OrgUnitId`
//! (UUID). На них же ссылаются `DirOrg::department`, `DirOrg::manager` и `DirGroup::members` (учётные записи и
//! вложенные группы). Атрибуты адресной книги — `OrgAttributes::attributes`, их имена — [`Directory::gal_attributes`].
//! Учётная запись, скрытая из книги «Организация» (`Account::hidden_from_gal`, CTR-83), отдаётся с признаком
//! `DirAccount::hidden_from_gal`, как из LDAP: в книгу её не берёт построение GAL, ссылки на неё (`DirOrg::manager`,
//! участники групп) остаются. Группа, скрытая из GAL (`DistributionGroup::hidden_from_gal`), в каталог не попадает —
//! у `DirGroup` признака нет, а книга строится из каталога. Почтовых контактов каталог не отдаёт (`contacts_since` —
//! по умолчанию).
//!
//! Курсор — версия выгрузки: `md5` строк каталога организации (всё, что отдаёт каталог), снятая в начале полной
//! выгрузки. В выгрузке — `p:<версия>:<последний id>` (порции по возрастанию id), после неё — `v:<версия>`.
//! Запрос с `v:` при неизменной версии — пустая порция с прежним курсором; каталог изменился (создание, изменение,
//! удаление строк, в том числе во время выгрузки) — `Conflict`: повторить полную выгрузку. Журнала удалений у
//! таблиц нет, поэтому удалённую запись другим способом не узнать. Нераспознанный курсор (чужой каталог) —
//! тоже `Conflict`.

use std::collections::BTreeMap;

use async_trait::async_trait;
use sqlx::postgres::PgRow;
use sqlx::types::Json;
use sqlx::{AssertSqlSafe, FromRow};
use tp_contracts::auth::{
    DirAccount, DirAlias, DirChange, DirCursor, DirDomain, DirGroup, DirOrg, DirPage, DirUnit,
    Directory,
};
use tp_contracts::meta::account_addresses;
use tp_contracts::{Error, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::schema::{begin, db_err, to_i64};
use crate::shards::ShardId;

/// Учётные записи каталога с оргатрибутами: `$1` — организация, ключ порции — `account_id`.
const ACCOUNTS_SQL: &str = "SELECT a.account_id, a.login, a.display_name, a.enabled, a.aliases, a.hidden_from_gal,
        o.department_id, o.title, o.manager_id, o.seniority, coalesce(o.attributes, '{}'::jsonb) AS attributes
    FROM accounts a
    LEFT JOIN org_attributes o ON o.tenant_id = a.tenant_id AND o.account_id = a.account_id
    WHERE a.tenant_id = $1";

/// Группы каталога без скрытых из GAL, участники — в порядке записи: `$1` — организация, ключ — `group_id`.
const GROUPS_SQL: &str = "SELECT g.group_id, g.name, g.address,
        coalesce((SELECT array_agg(coalesce(m.account_id, m.member_group_id) ORDER BY m.pos)
                  FROM group_members m WHERE m.tenant_id = g.tenant_id AND m.group_id = g.group_id),
                 '{}'::uuid[]) AS members
    FROM groups g
    WHERE g.tenant_id = $1 AND NOT g.hidden_from_gal";

#[derive(FromRow)]
struct AccountRow {
    account_id: Uuid,
    login: String,
    display_name: String,
    enabled: bool,
    aliases: Vec<String>,
    hidden_from_gal: bool,
    department_id: Option<Uuid>,
    title: Option<String>,
    manager_id: Option<Uuid>,
    seniority: Option<i32>,
    attributes: Json<BTreeMap<String, String>>,
}

impl From<AccountRow> for DirAccount {
    fn from(r: AccountRow) -> Self {
        DirAccount {
            external_id: r.account_id.to_string(),
            emails: account_addresses(&r.login, r.aliases.iter().map(String::as_str)),
            login: r.login,
            display_name: r.display_name,
            enabled: r.enabled,
            attributes: r.attributes.0,
            org: DirOrg {
                department: r.department_id.map(|u| u.to_string()),
                title: r.title,
                manager: r.manager_id.map(|u| u.to_string()),
                seniority: r.seniority,
            },
            hidden_from_gal: r.hidden_from_gal,
        }
    }
}

#[derive(FromRow)]
struct GroupRow {
    group_id: Uuid,
    name: String,
    address: Option<String>,
    members: Vec<Uuid>,
}

impl From<GroupRow> for DirGroup {
    fn from(r: GroupRow) -> Self {
        DirGroup {
            external_id: r.group_id.to_string(),
            name: r.name,
            email: r.address,
            members: r.members.iter().map(Uuid::to_string).collect(),
        }
    }
}

/// Позиция курсора ([`parse`]).
enum Position {
    /// Полная выгрузка с начала.
    Start,
    /// Продолжение выгрузки версии `version` после `last`.
    After { version: String, last: Uuid },
    /// Выгрузка версии завершена.
    Done(String),
}

fn is_version(v: &str) -> bool {
    v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Курсор → позиция; нераспознанный — `Conflict` (полная выгрузка).
fn parse(cursor: Option<&DirCursor>) -> Result<Position> {
    let Some(DirCursor(c)) = cursor else {
        return Ok(Position::Start);
    };
    if let Some(v) = c.strip_prefix("v:")
        && is_version(v)
    {
        return Ok(Position::Done(v.to_owned()));
    }
    if let Some((v, last)) = c.strip_prefix("p:").and_then(|rest| rest.split_once(':'))
        && is_version(v)
        && let Ok(last) = Uuid::parse_str(last)
    {
        return Ok(Position::After {
            version: v.to_owned(),
            last,
        });
    }
    Err(Error::Conflict)
}

impl PgMeta {
    /// Порция записей `rows` (запрос с организацией `$1`) по возрастанию `key` после позиции `cursor`.
    async fn dir_page<R, T>(
        &self,
        tenant: TenantId,
        cursor: Option<&DirCursor>,
        limit: usize,
        rows: &str,
        key: (&str, fn(&R) -> Uuid),
    ) -> Result<DirPage<T>>
    where
        R: for<'r> FromRow<'r, PgRow> + Send + Unpin,
        T: From<R> + Send,
    {
        let position = parse(cursor)?;
        let limit = limit.max(1);
        let fetch = to_i64(limit as u64 + 1)?;
        let (column, key_of) = key;
        let version_sql = format!(
            "SELECT md5(coalesce(string_agg(d::text, E'\\n' ORDER BY d.{column}), '')) FROM ({rows}) d"
        );
        let page_sql =
            format!("SELECT * FROM ({rows}) d WHERE d.{column} > $2 ORDER BY d.{column} LIMIT $3");
        let (position, version_sql, page_sql) = (&position, &version_sql, &page_sql);
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            // Версия снимается до чтения строк: изменение после неё даст другую версию у следующего запроса.
            let (version, after) = match position {
                Position::After { version, last } => (version.clone(), *last),
                Position::Start | Position::Done(_) => {
                    let current: String = sqlx::query_scalar(AssertSqlSafe(version_sql.as_str()))
                        .bind(tenant.as_uuid())
                        .fetch_one(&mut *tx)
                        .await
                        .map_err(db_err)?;
                    if let Position::Done(seen) = position {
                        crate::commits::commit(tx).await?;
                        return if *seen == current {
                            Ok(DirPage {
                                changes: Vec::new(),
                                next: DirCursor(format!("v:{current}")),
                                has_more: false,
                            })
                        } else {
                            Err(Error::Conflict)
                        };
                    }
                    (current, Uuid::nil())
                }
            };
            let mut found: Vec<R> = sqlx::query_as(AssertSqlSafe(page_sql.as_str()))
                .bind(tenant.as_uuid())
                .bind(after)
                .bind(fetch)
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            let has_more = found.len() > limit;
            found.truncate(limit);
            let next = match found.last() {
                Some(last) if has_more => format!("p:{version}:{}", key_of(last)),
                _ => format!("v:{version}"),
            };
            Ok(DirPage {
                changes: found
                    .into_iter()
                    .map(|r| DirChange::Upsert(T::from(r)))
                    .collect(),
                next: DirCursor(next),
                has_more,
            })
        })
        .await
    }
}

#[async_trait]
impl Directory for PgMeta {
    async fn accounts_since(
        &self,
        tenant: TenantId,
        cursor: Option<&DirCursor>,
        limit: usize,
    ) -> Result<DirPage<DirAccount>> {
        self.dir_page::<AccountRow, _>(
            tenant,
            cursor,
            limit,
            ACCOUNTS_SQL,
            ("account_id", |r| r.account_id),
        )
        .await
    }

    async fn groups_since(
        &self,
        tenant: TenantId,
        cursor: Option<&DirCursor>,
        limit: usize,
    ) -> Result<DirPage<DirGroup>> {
        self.dir_page::<GroupRow, _>(
            tenant,
            cursor,
            limit,
            GROUPS_SQL,
            ("group_id", |r| r.group_id),
        )
        .await
    }

    /// Псевдонимы учётных записей (`Account::aliases`), по учётной записи и в порядке записи.
    async fn aliases(&self, tenant: TenantId) -> Result<Vec<DirAlias>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let rows: Vec<(String, Uuid)> = sqlx::query_as(
                "SELECT a.alias, x.account_id FROM accounts x
                 CROSS JOIN LATERAL unnest(x.aliases) WITH ORDINALITY AS a(alias, pos)
                 WHERE x.tenant_id = $1 ORDER BY x.account_id, a.pos",
            )
            .bind(tenant.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(rows
                .into_iter()
                .map(|(address, target)| DirAlias {
                    address,
                    target: target.to_string(),
                })
                .collect())
        })
        .await
    }

    /// Домены организации; по умолчанию — заведённый первым.
    async fn domains(&self, tenant: TenantId) -> Result<Vec<DirDomain>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let names: Vec<String> = sqlx::query_scalar(
                "SELECT lower(name) FROM domains WHERE tenant_id = $1 ORDER BY created_at, domain_id",
            )
            .bind(tenant.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(names
                .into_iter()
                .enumerate()
                .map(|(i, name)| DirDomain {
                    name,
                    default: i == 0,
                })
                .collect())
        })
        .await
    }

    /// Имена атрибутов, заданных учётным записям организации (`OrgAttributes::attributes`), по возрастанию.
    async fn gal_attributes(&self, tenant: TenantId) -> Result<Vec<String>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let names: Vec<String> = sqlx::query_scalar(
                r#"SELECT DISTINCT k.name COLLATE "C" FROM org_attributes o
                   CROSS JOIN LATERAL jsonb_object_keys(o.attributes) AS k(name)
                   WHERE o.tenant_id = $1 ORDER BY 1"#,
            )
            .bind(tenant.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(names)
        })
        .await
    }

    /// Подразделения организации (`OrgStore`) по возрастанию идентификатора.
    async fn units(&self, tenant: TenantId) -> Result<Vec<DirUnit>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let rows: Vec<(Uuid, String, Option<Uuid>)> = sqlx::query_as(
                "SELECT unit_id, name, parent_id FROM org_units WHERE tenant_id = $1 ORDER BY unit_id",
            )
            .bind(tenant.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(rows
                .into_iter()
                .map(|(id, name, parent)| DirUnit {
                    external_id: id.to_string(),
                    name,
                    parent: parent.map(|p| p.to_string()),
                })
                .collect())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_positions() {
        let v = "0123456789abcdef0123456789abcdef";
        assert!(matches!(parse(None), Ok(Position::Start)));
        assert!(
            matches!(parse(Some(&DirCursor(format!("v:{v}")))), Ok(Position::Done(x)) if x == v)
        );
        let last = *tp_contracts::AccountId::new().as_uuid();
        assert!(matches!(
            parse(Some(&DirCursor(format!("p:{v}:{last}")))),
            Ok(Position::After { version, last: l }) if version == v && l == last
        ));
        for bad in ["", "v:", "v:xyz", "p:{v}", "p:{v}:not-a-uuid", "cookie"] {
            assert!(
                matches!(
                    parse(Some(&DirCursor(bad.replace("{v}", v)))),
                    Err(Error::Conflict)
                ),
                "{bad}"
            );
        }
    }
}
