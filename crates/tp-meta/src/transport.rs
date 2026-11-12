//! Настройки транспорта на PostgreSQL (META-28; CONTRACTS.md#Transport, CTR-24; SRS 4.1, 2.6;
//! решение 2026-11-01-transport-scope-external-mta).
//!
//! Уровень кластера: принятые домены — таблица `domains` (вид — столбцы `kind`, `next_hop`), доверенные источники —
//! `transport_sources`, версия снимка — строка `transport_cluster`. Запись — системная транзакция (домены всех
//! организаций: имя уникально в кластере), версия блокируется `FOR UPDATE` до фиксации; снимок читается под
//! `FOR SHARE` — версия и записи согласованы. Уровень организации: политики доменов назначения `transport_routes`,
//! таблица переписывания адресов `transport_rewrites` (META-39, CTR-40; ключ — канонический `from`) и общая для них
//! версия `transport_versions` — транзакция организации (RLS). После фиксации — сигнал
//! [`transport_changed`] ([`PgMeta::with_bus`]). Записи — CAS по версии, выполняются один раз ([`PgMeta::once`]):
//! повтор применённой записи ответил бы `Conflict`.

use std::net::IpAddr;

use async_trait::async_trait;
use sqlx::types::Json;
use sqlx::{FromRow, Postgres, Transaction};
use tp_contracts::transport::{
    AcceptedDomain, AddressPattern, ClusterTransport, DestinationPolicy, DomainKind, RewriteRule,
    SourceKind, SourceMatch, TenantTransport, TlsRequirement, TransportStore, TrustedSource,
    transport_changed,
};
use tp_contracts::{DomainId, Error, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::schema::{begin, begin_system, db_err, to_u64};

const AUTHORITATIVE: &str = "authoritative";
const INTERNAL_RELAY: &str = "internal_relay";

#[derive(FromRow)]
struct DomainRow {
    tenant_id: Uuid,
    domain_id: Uuid,
    name: String,
    kind: String,
    next_hop: Option<String>,
}

impl DomainRow {
    fn into_domain(self) -> Result<AcceptedDomain> {
        let kind = match (self.kind.as_str(), self.next_hop) {
            (AUTHORITATIVE, None) => DomainKind::Authoritative,
            (INTERNAL_RELAY, Some(next_hop)) => DomainKind::InternalRelay { next_hop },
            (kind, _) => {
                return Err(Error::internal(format!(
                    "домен {}: неверный вид {kind}",
                    self.name
                )));
            }
        };
        Ok(AcceptedDomain {
            id: DomainId::from_uuid(self.domain_id),
            tenant: TenantId::from_uuid(self.tenant_id),
            name: self.name,
            kind,
        })
    }
}

#[derive(FromRow)]
struct SourceRow {
    name: String,
    tenant_id: Option<Uuid>,
    matcher: Json<SourceMatch>,
    kind: Json<SourceKind>,
}

impl From<SourceRow> for TrustedSource {
    fn from(r: SourceRow) -> Self {
        TrustedSource {
            name: r.name,
            tenant: r.tenant_id.map(TenantId::from_uuid),
            matcher: r.matcher.0,
            kind: r.kind.0,
        }
    }
}

#[derive(FromRow)]
struct RouteRow {
    pattern: String,
    tls: Json<TlsRequirement>,
    smart_host: Option<String>,
    egress_ip: Option<String>,
}

impl RouteRow {
    fn into_policy(self) -> Result<DestinationPolicy> {
        let egress_ip = self
            .egress_ip
            .map(|ip| {
                ip.parse::<IpAddr>().map_err(|_| {
                    Error::internal(format!("политика {}: неверный адрес {ip}", self.pattern))
                })
            })
            .transpose()?;
        Ok(DestinationPolicy {
            pattern: self.pattern,
            tls: self.tls.0,
            smart_host: self.smart_host,
            egress_ip,
        })
    }
}

#[derive(FromRow)]
struct RewriteRow {
    from_addr: String,
    to_addr: String,
    sender: bool,
    recipient: bool,
}

impl RewriteRow {
    fn into_rule(self) -> Result<RewriteRule> {
        let pattern = |p: &str| {
            AddressPattern::new(p)
                .map_err(|_| Error::internal(format!("правило переписывания: неверный шаблон {p}")))
        };
        Ok(RewriteRule {
            from: pattern(&self.from_addr)?,
            to: pattern(&self.to_addr)?,
            sender: self.sender,
            recipient: self.recipient,
        })
    }
}

/// Версия кластера под блокировкой записи; устаревшая `expected` — `Conflict`.
async fn lock_cluster(tx: &mut Transaction<'static, Postgres>, expected: u64) -> Result<()> {
    let version: i64 = sqlx::query_scalar("SELECT version FROM transport_cluster FOR UPDATE")
        .fetch_one(&mut **tx)
        .await
        .map_err(db_err)?;
    if to_u64(version)? == expected {
        Ok(())
    } else {
        Err(Error::Conflict)
    }
}

/// Новая версия кластера (в той же транзакции).
async fn bump_cluster(tx: &mut Transaction<'static, Postgres>) -> Result<u64> {
    let version: i64 =
        sqlx::query_scalar("UPDATE transport_cluster SET version = version + 1 RETURNING version")
            .fetch_one(&mut **tx)
            .await
            .map_err(db_err)?;
    to_u64(version)
}

/// Версия организации под блокировкой записи (нет строки — 0); устаревшая `expected` — `Conflict`.
async fn lock_tenant(
    tx: &mut Transaction<'static, Postgres>,
    tenant: TenantId,
    expected: u64,
) -> Result<()> {
    let version: Option<i64> = sqlx::query_scalar(
        "SELECT version FROM transport_versions WHERE tenant_id = $1 FOR UPDATE",
    )
    .bind(tenant.as_uuid())
    .fetch_optional(&mut **tx)
    .await
    .map_err(db_err)?;
    if to_u64(version.unwrap_or(0))? == expected {
        Ok(())
    } else {
        Err(Error::Conflict)
    }
}

/// Новая версия организации. Первая запись — вставка строки: параллельная первая запись получит `Conflict`
/// (повтор ключа), а не версию поверх чужого изменения.
async fn bump_tenant(
    tx: &mut Transaction<'static, Postgres>,
    tenant: TenantId,
    expected: u64,
) -> Result<u64> {
    let version: i64 = if expected == 0 {
        sqlx::query_scalar(
            "INSERT INTO transport_versions (tenant_id, version) VALUES ($1, 1) RETURNING version",
        )
    } else {
        sqlx::query_scalar(
            "UPDATE transport_versions SET version = version + 1 WHERE tenant_id = $1
             RETURNING version",
        )
    }
    .bind(tenant.as_uuid())
    .fetch_one(&mut **tx)
    .await
    .map_err(db_err)?;
    to_u64(version)
}

impl PgMeta {
    /// Начало изменения уровня кластера: системная транзакция, проверка версии под блокировкой.
    async fn cluster_begin(&self, expected: u64) -> Result<Transaction<'static, Postgres>> {
        let mut tx = begin_system(&self.pool).await?;
        lock_cluster(&mut tx, expected).await?;
        Ok(tx)
    }

    /// Конец изменения уровня кластера: новая версия, фиксация, сигнал.
    async fn cluster_commit(&self, mut tx: Transaction<'static, Postgres>) -> Result<u64> {
        let version = bump_cluster(&mut tx).await?;
        crate::commits::commit(tx).await?;
        self.signal(transport_changed(None)).await;
        Ok(version)
    }

    /// Начало изменения политик организации: транзакция организации, проверка версии под блокировкой.
    async fn tenant_begin(
        &self,
        tenant: TenantId,
        expected: u64,
    ) -> Result<Transaction<'static, Postgres>> {
        let mut tx = begin(&self.pool, tenant).await?;
        lock_tenant(&mut tx, tenant, expected).await?;
        Ok(tx)
    }

    /// Конец изменения политик организации: новая версия, фиксация, сигнал.
    async fn tenant_commit(
        &self,
        mut tx: Transaction<'static, Postgres>,
        tenant: TenantId,
        expected: u64,
    ) -> Result<u64> {
        let version = bump_tenant(&mut tx, tenant, expected).await?;
        crate::commits::commit(tx).await?;
        self.signal(transport_changed(Some(tenant))).await;
        Ok(version)
    }
}

/// Удалена ли строка: нет — `NotFound`.
fn removed(done: sqlx::postgres::PgQueryResult) -> Result<()> {
    if done.rows_affected() == 0 {
        Err(Error::NotFound)
    } else {
        Ok(())
    }
}

#[async_trait]
impl TransportStore for PgMeta {
    async fn cluster(&self) -> Result<ClusterTransport> {
        self.idempotent(|| async move {
        let mut tx = begin_system(&self.pool).await?;
        // FOR SHARE ждёт фиксации писателя; следующие запросы видят все изменения до этой версии и только их.
        let version: i64 = sqlx::query_scalar("SELECT version FROM transport_cluster FOR SHARE")
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
        let domains: Vec<DomainRow> = sqlx::query_as(
            "SELECT tenant_id, domain_id, name, kind, next_hop FROM domains ORDER BY name COLLATE \"C\"",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        let sources: Vec<SourceRow> = sqlx::query_as(
            "SELECT name, tenant_id, matcher, kind FROM transport_sources ORDER BY name COLLATE \"C\"",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        crate::commits::commit(tx).await?;
        Ok(ClusterTransport {
            version: to_u64(version)?,
            domains: domains
                .into_iter()
                .map(DomainRow::into_domain)
                .collect::<Result<_>>()?,
            sources: sources.into_iter().map(TrustedSource::from).collect(),
        })
}).await
    }

    async fn tenant(&self, tenant: TenantId) -> Result<TenantTransport> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.pool, tenant).await?;
            let version: Option<i64> = sqlx::query_scalar(
                "SELECT version FROM transport_versions WHERE tenant_id = $1 FOR SHARE",
            )
            .bind(tenant.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            let routes: Vec<RouteRow> = sqlx::query_as(
            "SELECT pattern, tls, smart_host, host(egress_ip) AS egress_ip FROM transport_routes
             WHERE tenant_id = $1 ORDER BY pattern COLLATE \"C\"",
        )
        .bind(tenant.as_uuid())
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
            let rewrites: Vec<RewriteRow> = sqlx::query_as(
                "SELECT from_addr, to_addr, sender, recipient FROM transport_rewrites
                 WHERE tenant_id = $1 ORDER BY from_addr COLLATE \"C\"",
            )
            .bind(tenant.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(TenantTransport {
                version: to_u64(version.unwrap_or(0))?,
                routes: routes
                    .into_iter()
                    .map(RouteRow::into_policy)
                    .collect::<Result<_>>()?,
                rewrites: rewrites
                    .into_iter()
                    .map(RewriteRow::into_rule)
                    .collect::<Result<_>>()?,
            })
        })
        .await
    }

    async fn put_domain(&self, expected: u64, domain: &AcceptedDomain) -> Result<u64> {
        self.once(async move {
        domain.validate()?;
        let (kind, next_hop) = match &domain.kind {
            DomainKind::Authoritative => (AUTHORITATIVE, None),
            DomainKind::InternalRelay { next_hop } => (INTERNAL_RELAY, Some(next_hop.as_str())),
        };
        let mut tx = self.cluster_begin(expected).await?;
        let owner: Option<Uuid> =
            sqlx::query_scalar("SELECT tenant_id FROM domains WHERE domain_id = $1")
                .bind(domain.id.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
        if owner.is_some_and(|t| t != *domain.tenant.as_uuid()) {
            return Err(Error::invalid("организацию домена менять нельзя"));
        }
        // Имя занято другим доменом — повтор ключа `domains_name` (`Conflict`); нет организации — `Invalid`.
        sqlx::query(
            "INSERT INTO domains (tenant_id, domain_id, name, kind, next_hop) VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT (tenant_id, domain_id) DO UPDATE
                 SET name = EXCLUDED.name, kind = EXCLUDED.kind, next_hop = EXCLUDED.next_hop",
        )
        .bind(domain.tenant.as_uuid())
        .bind(domain.id.as_uuid())
        .bind(&domain.name)
        .bind(kind)
        .bind(next_hop)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        self.cluster_commit(tx).await
}).await
    }

    async fn delete_domain(&self, expected: u64, id: DomainId) -> Result<u64> {
        self.once(async move {
            let mut tx = self.cluster_begin(expected).await?;
            // Домен с учётными записями не удаляется (внешний ключ — `Invalid`).
            removed(
                sqlx::query("DELETE FROM domains WHERE domain_id = $1")
                    .bind(id.as_uuid())
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?,
            )?;
            self.cluster_commit(tx).await
        })
        .await
    }

    async fn put_source(&self, expected: u64, source: &TrustedSource) -> Result<u64> {
        self.once(async move {
            source.validate()?;
            let tenant = source.tenant.map(|t| *t.as_uuid());
            let mut tx = self.cluster_begin(expected).await?;
            let owner: Option<Option<Uuid>> =
                sqlx::query_scalar("SELECT tenant_id FROM transport_sources WHERE name = $1")
                    .bind(&source.name)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(db_err)?;
            if owner.is_some_and(|t| t != tenant) {
                return Err(Error::invalid("организацию источника менять нельзя"));
            }
            sqlx::query(
            "INSERT INTO transport_sources (name, tenant_id, matcher, kind) VALUES ($1, $2, $3, $4)
             ON CONFLICT (name) DO UPDATE
                 SET matcher = EXCLUDED.matcher, kind = EXCLUDED.kind, updated_at = now()",
        )
        .bind(&source.name)
        .bind(tenant)
        .bind(Json(&source.matcher))
        .bind(Json(&source.kind))
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
            self.cluster_commit(tx).await
        })
        .await
    }

    async fn delete_source(&self, expected: u64, name: &str) -> Result<u64> {
        self.once(async move {
            let mut tx = self.cluster_begin(expected).await?;
            removed(
                sqlx::query("DELETE FROM transport_sources WHERE name = $1")
                    .bind(name)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?,
            )?;
            self.cluster_commit(tx).await
        })
        .await
    }

    async fn put_route(
        &self,
        tenant: TenantId,
        expected: u64,
        policy: &DestinationPolicy,
    ) -> Result<u64> {
        self.once(async move {
            policy.validate()?;
            let egress = policy.egress_ip.map(|ip| ip.to_string());
            let mut tx = self.tenant_begin(tenant, expected).await?;
            sqlx::query(
                "INSERT INTO transport_routes (tenant_id, pattern, tls, smart_host, egress_ip)
             VALUES ($1, $2, $3, $4, $5::inet)
             ON CONFLICT (tenant_id, pattern) DO UPDATE
                 SET tls = EXCLUDED.tls, smart_host = EXCLUDED.smart_host,
                     egress_ip = EXCLUDED.egress_ip, updated_at = now()",
            )
            .bind(tenant.as_uuid())
            .bind(&policy.pattern)
            .bind(Json(&policy.tls))
            .bind(policy.smart_host.as_deref())
            .bind(egress.as_deref())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            self.tenant_commit(tx, tenant, expected).await
        })
        .await
    }

    async fn delete_route(&self, tenant: TenantId, expected: u64, pattern: &str) -> Result<u64> {
        self.once(async move {
            let mut tx = self.tenant_begin(tenant, expected).await?;
            removed(
                sqlx::query("DELETE FROM transport_routes WHERE tenant_id = $1 AND pattern = $2")
                    .bind(tenant.as_uuid())
                    .bind(pattern)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?,
            )?;
            self.tenant_commit(tx, tenant, expected).await
        })
        .await
    }

    async fn put_rewrite(
        &self,
        tenant: TenantId,
        expected: u64,
        rule: &RewriteRule,
    ) -> Result<u64> {
        self.once(async move {
            rule.validate()?;
            let mut tx = self.tenant_begin(tenant, expected).await?;
            sqlx::query(
                "INSERT INTO transport_rewrites (tenant_id, from_addr, to_addr, sender, recipient)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (tenant_id, from_addr) DO UPDATE
                     SET to_addr = EXCLUDED.to_addr, sender = EXCLUDED.sender,
                         recipient = EXCLUDED.recipient, updated_at = now()",
            )
            .bind(tenant.as_uuid())
            .bind(rule.from.as_str())
            .bind(rule.to.as_str())
            .bind(rule.sender)
            .bind(rule.recipient)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            self.tenant_commit(tx, tenant, expected).await
        })
        .await
    }

    async fn delete_rewrite(
        &self,
        tenant: TenantId,
        expected: u64,
        from: &AddressPattern,
    ) -> Result<u64> {
        self.once(async move {
            let mut tx = self.tenant_begin(tenant, expected).await?;
            removed(
                sqlx::query(
                    "DELETE FROM transport_rewrites WHERE tenant_id = $1 AND from_addr = $2",
                )
                .bind(tenant.as_uuid())
                .bind(from.as_str())
                .execute(&mut *tx)
                .await
                .map_err(db_err)?,
            )?;
            self.tenant_commit(tx, tenant, expected).await
        })
        .await
    }
}
