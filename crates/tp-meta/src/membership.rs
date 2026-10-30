//! Членство узлов на PostgreSQL (CONTRACTS.md#Membership, решение 2026-10-30, #45).
//!
//! Одна строка `nodes` на узел; каждая операция — один оператор вне транзакции организации (таблица без RLS).
//! Срок записи считает СУБД (`now()`): часы узлов приложения не участвуют. Срок и период продления —
//! [`crate::events::EventsConfig::node_ttl`] и [`crate::events::EventsConfig::heartbeat_interval`].

use std::time::Duration;

use async_trait::async_trait;
use sqlx::FromRow;
use time::OffsetDateTime;
use tp_contracts::membership::{Membership, NodeId};
use tp_contracts::{Error, Result};

use crate::PgMeta;
use crate::schema::db_err;

/// Запись узла (для администрирования и `thepost doctor`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRecord {
    /// Узел.
    pub node: NodeId,
    /// Роли узла, объявленные при продлении ([`PgMeta::heartbeat_roles`]); пусто — не объявлены.
    pub roles: Vec<String>,
    /// Первое продление текущей записи.
    pub joined_at: OffsetDateTime,
    /// Срок записи по часам СУБД.
    pub expires_at: OffsetDateTime,
}

#[derive(FromRow)]
struct NodeRow {
    node_id: String,
    roles: Vec<String>,
    joined_at: OffsetDateTime,
    expires_at: OffsetDateTime,
}

/// Срок записи в микросекундах (аргумент `$n * interval '1 microsecond'`); нулевой — `Invalid`.
fn ttl_arg(ttl: Duration) -> Result<i64> {
    if ttl.is_zero() {
        return Err(Error::invalid("срок записи узла не может быть нулевым"));
    }
    i64::try_from(ttl.as_micros()).map_err(|_| Error::invalid("срок записи узла слишком велик"))
}

impl PgMeta {
    /// Продление записи узла с его ролями (`Membership::heartbeat` роли не меняет).
    pub async fn heartbeat_roles(
        &self,
        node: &NodeId,
        roles: &[String],
        ttl: Duration,
    ) -> Result<()> {
        self.upsert_node(node, Some(roles), ttl).await
    }

    /// Все записи узлов, включая просроченные, по имени узла.
    pub async fn nodes(&self) -> Result<Vec<NodeRecord>> {
        let rows: Vec<NodeRow> = sqlx::query_as(
            r#"SELECT node_id, roles, joined_at, expires_at FROM nodes ORDER BY node_id COLLATE "C""#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.into_iter()
            .map(|r| {
                Ok(NodeRecord {
                    node: NodeId::new(r.node_id)
                        .map_err(|e| Error::internal(format!("запись узла: {e}")))?,
                    roles: r.roles,
                    joined_at: r.joined_at,
                    expires_at: r.expires_at,
                })
            })
            .collect()
    }

    async fn upsert_node(
        &self,
        node: &NodeId,
        roles: Option<&[String]>,
        ttl: Duration,
    ) -> Result<()> {
        // Просроченная запись начинается заново (joined_at — время возвращения узла). Имена узлов постоянны,
        // поэтому просроченные записи не копятся: их число не больше числа когда-либо запущенных узлов.
        sqlx::query(
            "INSERT INTO nodes (node_id, roles, expires_at)
             VALUES ($1, coalesce($2, '{}'::text[]), now() + $3 * interval '1 microsecond')
             ON CONFLICT (node_id) DO UPDATE SET
                 roles = coalesce($2, nodes.roles),
                 joined_at = CASE WHEN nodes.expires_at > now() THEN nodes.joined_at ELSE now() END,
                 expires_at = EXCLUDED.expires_at",
        )
        .bind(node.as_str())
        .bind(roles)
        .bind(ttl_arg(ttl)?)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }
}

#[async_trait]
impl Membership for PgMeta {
    async fn heartbeat(&self, node: &NodeId, ttl: Duration) -> Result<()> {
        self.upsert_node(node, None, ttl).await
    }

    async fn leave(&self, node: &NodeId) -> Result<()> {
        sqlx::query("DELETE FROM nodes WHERE node_id = $1")
            .bind(node.as_str())
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn live(&self) -> Result<Vec<NodeId>> {
        // Порядок `COLLATE "C"` — побайтовый, как `Ord` у `NodeId`.
        let names: Vec<String> = sqlx::query_scalar(
            r#"SELECT node_id FROM nodes WHERE expires_at > now() ORDER BY node_id COLLATE "C""#,
        )
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        names
            .into_iter()
            .map(|n| NodeId::new(n).map_err(|e| Error::internal(format!("запись узла: {e}"))))
            .collect()
    }
}
