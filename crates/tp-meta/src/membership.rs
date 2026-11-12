//! Членство узлов на PostgreSQL (CONTRACTS.md#Membership, решения 2026-10-30, #45, и 2026-11-01; META-16, META-27).
//!
//! Одна строка `nodes` на узел: роли, адрес служебного канала, площадка ([`NodeInfo`]) и срок; каждая операция —
//! один оператор вне транзакции организации (таблица без RLS). Срок записи считает СУБД (`now()`): часы узлов
//! приложения не участвуют. Срок и период продления — [`crate::events::EventsConfig::node_ttl`] и
//! [`crate::events::EventsConfig::heartbeat_interval`].
//!
//! Сигнал `MembersChanged` ([`PgMeta::with_bus`]) — после новой записи (и после истечения), изменения ролей,
//! адреса или площадки и выхода живого узла; продление без изменений и истечение — без сигнала.

use std::collections::BTreeSet;
use std::time::Duration;

use async_trait::async_trait;
use sqlx::FromRow;
use time::OffsetDateTime;
use tp_contracts::bus::{Event, EventKind};
use tp_contracts::membership::{Membership, NodeId, NodeInfo, SiteId};
use tp_contracts::role::Role;
use tp_contracts::{Error, Result, TenantId};

use crate::PgMeta;
use crate::schema::db_err;

/// Запись узла (для администрирования и `thepost doctor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRecord {
    /// Узел.
    pub node: NodeId,
    /// Роли узла, объявленные при продлении (в том числе роли модулей); пусто — не объявлены.
    pub roles: Vec<String>,
    /// Адрес служебного канала; пусто — запросов не принимает.
    pub addr: String,
    /// Площадка узла.
    pub site: SiteId,
    /// Первое продление текущей записи.
    pub joined_at: OffsetDateTime,
    /// Срок записи по часам СУБД.
    pub expires_at: OffsetDateTime,
}

#[derive(FromRow)]
struct NodeRow {
    node_id: String,
    roles: Vec<String>,
    addr: String,
    site: String,
    joined_at: OffsetDateTime,
    expires_at: OffsetDateTime,
}

#[derive(FromRow)]
struct InfoRow {
    node_id: String,
    roles: Vec<String>,
    addr: String,
    site: String,
}

fn node_id(s: String) -> Result<NodeId> {
    NodeId::new(s).map_err(|e| Error::internal(format!("запись узла: {e}")))
}

fn site_id(s: String) -> Result<SiteId> {
    SiteId::new(s).map_err(|e| Error::internal(format!("запись узла: {e}")))
}

impl InfoRow {
    /// Роли ядра записи; прочие имена (роли модулей, роли более новой версии) не входят в [`NodeInfo`].
    fn into_info(self) -> Result<NodeInfo> {
        let roles: BTreeSet<Role> = self.roles.iter().filter_map(|r| r.parse().ok()).collect();
        Ok(NodeInfo {
            id: node_id(self.node_id)?,
            roles,
            addr: self.addr,
            site: site_id(self.site)?,
        })
    }
}

/// Срок записи в микросекундах (аргумент `$n * interval '1 microsecond'`); нулевой — `Invalid`.
fn ttl_arg(ttl: Duration) -> Result<i64> {
    if ttl.is_zero() {
        return Err(Error::invalid("срок записи узла не может быть нулевым"));
    }
    i64::try_from(ttl.as_micros()).map_err(|_| Error::invalid("срок записи узла слишком велик"))
}

/// Новые значения записи узла; `None` — оставить прежнее (новая запись — значение по умолчанию).
struct Upsert<'a> {
    roles: Option<Vec<String>>,
    addr: Option<&'a str>,
    site: Option<&'a str>,
}

impl PgMeta {
    /// Продление записи узла с ролями `roles` (роли ядра и модулей по имени); адрес и площадка записи не
    /// меняются. Путь приложения до перехода на [`Membership::heartbeat`] с [`NodeInfo`].
    pub async fn heartbeat_roles(
        &self,
        node: &NodeId,
        roles: &[String],
        ttl: Duration,
    ) -> Result<()> {
        let up = Upsert {
            roles: Some(roles.to_vec()),
            addr: None,
            site: None,
        };
        self.upsert_node(node, up, ttl).await
    }

    /// Все записи узлов, включая просроченные, по имени узла.
    pub async fn nodes(&self) -> Result<Vec<NodeRecord>> {
        self.idempotent(|| async move {
            let rows: Vec<NodeRow> = sqlx::query_as(
                r#"SELECT node_id, roles, addr, site, joined_at, expires_at FROM nodes
               ORDER BY node_id COLLATE "C""#,
            )
            .fetch_all(&mut *crate::pool::acquire(&self.pool).await?)
            .await
            .map_err(db_err)?;
            rows.into_iter()
                .map(|r| {
                    Ok(NodeRecord {
                        node: node_id(r.node_id)?,
                        roles: r.roles,
                        addr: r.addr,
                        site: site_id(r.site)?,
                        joined_at: r.joined_at,
                        expires_at: r.expires_at,
                    })
                })
                .collect()
        })
        .await
    }

    async fn upsert_node(&self, node: &NodeId, up: Upsert<'_>, ttl: Duration) -> Result<()> {
        let up = &up;
        self.rewrite(|| async move {
        if let Some(addr) = up.addr
            && (addr.len() > 2048 || addr.chars().any(|c| c.is_whitespace() || c.is_control()))
        {
            return Err(Error::invalid(
                "адрес узла: без пробелов, не длиннее 2048 байт",
            ));
        }
        // Просроченная запись начинается заново (joined_at — время возвращения узла). Имена узлов постоянны,
        // поэтому просроченные записи не копятся: их число не больше числа когда-либо запущенных узлов.
        // Возвращается признак изменения состава: записи не было, она истекла или изменились роли, адрес,
        // площадка (снимок `old` — до изменения).
        let changed: bool = sqlx::query_scalar(
            "WITH old AS (SELECT expires_at > now() AS alive, roles, addr, site FROM nodes WHERE node_id = $1)
             INSERT INTO nodes (node_id, roles, addr, site, expires_at)
             VALUES ($1, coalesce($2, '{}'::text[]), coalesce($3, ''), coalesce($4, 'default'),
                     now() + $5 * interval '1 microsecond')
             ON CONFLICT (node_id) DO UPDATE SET
                 roles = coalesce($2, nodes.roles),
                 addr = coalesce($3, nodes.addr),
                 site = coalesce($4, nodes.site),
                 joined_at = CASE WHEN nodes.expires_at > now() THEN nodes.joined_at ELSE now() END,
                 expires_at = EXCLUDED.expires_at
             RETURNING NOT coalesce((SELECT o.alive AND o.roles = nodes.roles AND o.addr = nodes.addr
                                            AND o.site = nodes.site FROM old o), false)",
        )
        .bind(node.as_str())
        .bind(&up.roles)
        .bind(up.addr)
        .bind(up.site)
        .bind(ttl_arg(ttl)?)
        .fetch_one(&mut *crate::pool::acquire(&self.pool).await?)
        .await
        .map_err(db_err)?;
        if changed {
            self.members_changed().await;
        }
        Ok(())
}).await
    }

    /// Сигнал `MembersChanged` (после фиксации).
    async fn members_changed(&self) {
        self.signal(Event {
            tenant: TenantId::DEFAULT,
            account: None,
            kind: EventKind::MembersChanged,
        })
        .await;
    }
}

#[async_trait]
impl Membership for PgMeta {
    async fn heartbeat(&self, node: &NodeInfo, ttl: Duration) -> Result<()> {
        // Роли ядра в порядке `Role` (как в `BTreeSet`): одинаковый набор — одинаковая строка.
        let up = Upsert {
            roles: Some(node.roles.iter().map(|r| r.as_str().to_owned()).collect()),
            addr: Some(&node.addr),
            site: Some(node.site.as_str()),
        };
        self.upsert_node(&node.id, up, ttl).await
    }

    async fn leave(&self, node: &NodeId) -> Result<()> {
        self.rewrite(|| async move {
            // Выход живого узла — сигнал; удаление истёкшей записи состав не меняет.
            let was_live: Option<bool> = sqlx::query_scalar(
                "DELETE FROM nodes WHERE node_id = $1 RETURNING expires_at > now()",
            )
            .bind(node.as_str())
            .fetch_optional(&mut *crate::pool::acquire(&self.pool).await?)
            .await
            .map_err(db_err)?;
            if was_live == Some(true) {
                self.members_changed().await;
            }
            Ok(())
        })
        .await
    }

    async fn live_info(&self) -> Result<Vec<NodeInfo>> {
        self.idempotent(|| async move {
            // Порядок `COLLATE "C"` — побайтовый, как `Ord` у `NodeId`.
            let rows: Vec<InfoRow> = sqlx::query_as(
                r#"SELECT node_id, roles, addr, site FROM nodes WHERE expires_at > now()
               ORDER BY node_id COLLATE "C""#,
            )
            .fetch_all(&mut *crate::pool::acquire(&self.pool).await?)
            .await
            .map_err(db_err)?;
            rows.into_iter().map(InfoRow::into_info).collect()
        })
        .await
    }

    async fn live(&self) -> Result<Vec<NodeId>> {
        self.idempotent(|| async move {
            let names: Vec<String> = sqlx::query_scalar(
            r#"SELECT node_id FROM nodes WHERE expires_at > now() ORDER BY node_id COLLATE "C""#,
        )
        .fetch_all(&mut *crate::pool::acquire(&self.pool).await?)
        .await
        .map_err(db_err)?;
            names.into_iter().map(node_id).collect()
        })
        .await
    }
}
