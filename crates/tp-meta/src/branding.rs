//! Брендинг заказчика на PostgreSQL (META-84; CTR-91, CONTRACTS.md#Branding; решение 122): таблица `branding`
//! (миграция 0043) — по строке на уровень, установка — `tenant_id IS NULL`; заданные роли — `jsonb`.
//!
//! Набор организации читается и пишется в её транзакции (RLS), набор установки и наборы всех организаций —
//! системной транзакцией. Читать набор установки RLS разрешает всем, поэтому `branding_levels` организации — один
//! запрос в её транзакции.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::types::Json;
use sqlx::{FromRow, PgConnection};
use tp_contracts::branding::{
    BrandColor, BrandLogo, Branding, BrandingLevels, BrandingScope, BrandingStore, CliRole, Role,
};
use tp_contracts::{BlobHash, Error, Result, TenantId};

use crate::PgMeta;
use crate::pool::DbPool;
use crate::schema::{begin, begin_system, db_err, to_i64, to_u32, to_u64};

/// Столбцы набора в порядке [`Row`] (после `tenant_id`).
const COLUMNS: &str =
    "product_name, primary_color, roles, logo_hash, logo_size, logo_width, logo_height";

/// Заданные роли набора — столбец `roles`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Roles {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    light: BTreeMap<Role, BrandColor>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    dark: BTreeMap<Role, BrandColor>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    cli: BTreeMap<CliRole, BrandColor>,
}

#[derive(FromRow)]
struct Row {
    tenant_id: Option<uuid::Uuid>,
    product_name: Option<String>,
    primary_color: Option<String>,
    roles: Json<Roles>,
    logo_hash: Option<Vec<u8>>,
    logo_size: Option<i64>,
    logo_width: Option<i32>,
    logo_height: Option<i32>,
}

impl Row {
    fn into_branding(self) -> Result<Branding> {
        let bad = |what: &str| Error::internal(format!("branding: invalid stored {what}"));
        let logo = match (
            self.logo_hash,
            self.logo_size,
            self.logo_width,
            self.logo_height,
        ) {
            (Some(hash), Some(size), Some(width), Some(height)) => Some(BrandLogo {
                hash: BlobHash(hash.try_into().map_err(|_| bad("logo hash"))?),
                size: to_u64(size)?,
                width: to_u32(i64::from(width))?,
                height: to_u32(i64::from(height))?,
            }),
            (None, None, None, None) => None,
            _ => return Err(bad("logo")),
        };
        let primary = match self.primary_color {
            Some(c) => Some(c.parse().map_err(|_| bad("primary"))?),
            None => None,
        };
        let Json(roles) = self.roles;
        Ok(Branding {
            product_name: self.product_name,
            primary,
            light: roles.light,
            dark: roles.dark,
            cli: roles.cli,
            logo,
        })
    }
}

/// Значения столбцов знака: хэш, размер, ширина, высота.
type LogoColumns = (Option<Vec<u8>>, Option<i64>, Option<i32>, Option<i32>);

fn logo_columns(logo: Option<&BrandLogo>) -> Result<LogoColumns> {
    let Some(l) = logo else {
        return Ok((None, None, None, None));
    };
    let side = |v: u32| i32::try_from(v).map_err(|_| Error::invalid("brand logo: side too large"));
    Ok((
        Some(l.hash.as_bytes().to_vec()),
        Some(to_i64(l.size)?),
        Some(side(l.width)?),
        Some(side(l.height)?),
    ))
}

/// Транзакция уровня: организации — её контекст, установки — системная.
async fn begin_scope(
    pool: &DbPool,
    scope: BrandingScope,
) -> Result<sqlx::Transaction<'static, sqlx::Postgres>> {
    match scope {
        BrandingScope::Installation => begin_system(pool).await,
        BrandingScope::Org(tenant) => begin(pool, tenant).await,
    }
}

/// Условие строки уровня; `$1` — организация (у установки параметров нет).
fn scope_filter(scope: BrandingScope) -> &'static str {
    match scope {
        BrandingScope::Installation => "tenant_id IS NULL",
        BrandingScope::Org(_) => "tenant_id = $1",
    }
}

fn scope_tenant(scope: BrandingScope) -> Option<uuid::Uuid> {
    match scope {
        BrandingScope::Installation => None,
        BrandingScope::Org(tenant) => Some(*tenant.as_uuid()),
    }
}

/// Строки по запросу `sql`; `tenant` — параметр `$1`, если есть.
async fn fetch(
    conn: &mut PgConnection,
    sql: String,
    tenant: Option<uuid::Uuid>,
) -> Result<Vec<Row>> {
    let mut q = sqlx::query_as::<_, Row>(sqlx::AssertSqlSafe(sql));
    if let Some(t) = tenant {
        q = q.bind(t);
    }
    q.fetch_all(conn).await.map_err(db_err)
}

fn single(rows: Vec<Row>) -> Result<Option<Branding>> {
    rows.into_iter().next().map(Row::into_branding).transpose()
}

#[async_trait]
impl BrandingStore for PgMeta {
    async fn branding(&self, scope: BrandingScope) -> Result<Option<Branding>> {
        self.idempotent(|| async move {
            let mut tx = begin_scope(&self.pool, scope).await?;
            let sql = format!(
                "SELECT tenant_id, {COLUMNS} FROM branding WHERE {}",
                scope_filter(scope)
            );
            let out = single(fetch(&mut tx, sql, scope_tenant(scope)).await?)?;
            crate::commits::commit(tx).await?;
            Ok(out)
        })
        .await
    }

    /// Запись по значению (upsert) — повторяется после обрыва во время COMMIT ([`PgMeta::rewrite`]).
    async fn set_branding(&self, scope: BrandingScope, branding: &Branding) -> Result<()> {
        branding.validate()?;
        let (hash, size, width, height) = logo_columns(branding.logo.as_ref())?;
        let roles = Json(Roles {
            light: branding.light.clone(),
            dark: branding.dark.clone(),
            cli: branding.cli.clone(),
        });
        let conflict = match scope {
            BrandingScope::Installation => "((tenant_id IS NULL)) WHERE tenant_id IS NULL",
            BrandingScope::Org(_) => "(tenant_id)",
        };
        let sql = format!(
            "INSERT INTO branding (tenant_id, {COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
             ON CONFLICT {conflict} DO UPDATE SET
                 product_name = excluded.product_name, primary_color = excluded.primary_color,
                 roles = excluded.roles, logo_hash = excluded.logo_hash, logo_size = excluded.logo_size,
                 logo_width = excluded.logo_width, logo_height = excluded.logo_height, updated_at = now()"
        );
        let (sql, hash, roles) = (&sql, &hash, &roles);
        self.rewrite(|| async move {
            let mut tx = begin_scope(&self.pool, scope).await?;
            sqlx::query(sqlx::AssertSqlSafe(sql.clone()))
                .bind(scope_tenant(scope))
                .bind(&branding.product_name)
                .bind(branding.primary.map(|c| c.to_string()))
                .bind(roles)
                .bind(hash)
                .bind(size)
                .bind(width)
                .bind(height)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            crate::commits::commit(tx).await
        })
        .await
    }

    /// Удаление с возвратом удалённого — один раз ([`PgMeta::once`]): повтор вернул бы `None`.
    async fn reset_branding(&self, scope: BrandingScope) -> Result<Option<Branding>> {
        self.once(async move {
            let mut tx = begin_scope(&self.pool, scope).await?;
            let sql = format!(
                "DELETE FROM branding WHERE {} RETURNING tenant_id, {COLUMNS}",
                scope_filter(scope)
            );
            let out = single(fetch(&mut tx, sql, scope_tenant(scope)).await?)?;
            crate::commits::commit(tx).await?;
            Ok(out)
        })
        .await
    }

    /// Одним запросом: строки организации и установки (набор установки RLS показывает каждой организации).
    async fn branding_levels(&self, tenant: Option<TenantId>) -> Result<BrandingLevels> {
        self.idempotent(|| async move {
            let (mut tx, filter) = match tenant {
                Some(t) => (
                    begin(&self.pool, t).await?,
                    "tenant_id = $1 OR tenant_id IS NULL",
                ),
                None => (begin_system(&self.pool).await?, "tenant_id IS NULL"),
            };
            let sql = format!("SELECT tenant_id, {COLUMNS} FROM branding WHERE {filter}");
            let rows = fetch(&mut tx, sql, tenant.map(|t| *t.as_uuid())).await?;
            crate::commits::commit(tx).await?;
            let mut out = BrandingLevels::default();
            for row in rows {
                if row.tenant_id.is_some() {
                    out.org = Some(row.into_branding()?);
                } else {
                    out.installation = Some(row.into_branding()?);
                }
            }
            Ok(out)
        })
        .await
    }

    /// Системная транзакция: свои наборы всех организаций по возрастанию идентификатора.
    async fn org_brandings(&self) -> Result<Vec<(TenantId, Branding)>> {
        self.idempotent(|| async move {
            let mut tx = begin_system(&self.pool).await?;
            let sql = format!(
                "SELECT tenant_id, {COLUMNS} FROM branding WHERE tenant_id IS NOT NULL ORDER BY tenant_id"
            );
            let rows = fetch(&mut tx, sql, None).await?;
            crate::commits::commit(tx).await?;
            rows.into_iter()
                .map(|row| {
                    let tenant = row
                        .tenant_id
                        .map(TenantId::from_uuid)
                        .ok_or_else(|| Error::internal("branding: org row without tenant"))?;
                    Ok((tenant, row.into_branding()?))
                })
                .collect()
        })
        .await
    }

    /// По индексу `branding_logo`, по всем уровням (системная транзакция).
    async fn logo_in_use(&self, hash: &BlobHash) -> Result<bool> {
        let hash = *hash;
        self.idempotent(|| async move {
            let mut tx = begin_system(&self.pool).await?;
            let used: bool =
                sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM branding WHERE logo_hash = $1)")
                    .bind(hash.as_bytes().as_slice())
                    .fetch_one(&mut *tx)
                    .await
                    .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(used)
        })
        .await
    }
}
