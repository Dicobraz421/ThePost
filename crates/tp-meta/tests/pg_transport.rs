//! Настройки транспорта на PostgreSQL (META-28, META-39; CONTRACTS.md#Transport, CTR-24, CTR-40).
//!
//! Тесты записи уровня кластера — на чистой БД ([`common::fresh_meta`]): параллельные писатели версии кластера
//! дали бы `Conflict`.

mod common;

use std::sync::Arc;

use async_trait::async_trait;
use tp_contracts::bus::{Event, EventKind};
use tp_contracts::meta::{AccountKind, AccountStore, AccountUpsert};
use tp_contracts::transport::{
    AcceptedDomain, AddressPattern, ClusterTransport, DestinationPolicy, DomainKind, RewriteRule,
    SourceKind, SourceMatch, TenantTransport, TlsRequirement, TransportStore, TrustedSource,
};
use tp_contracts::{DomainId, Error, Result, TenantId};
use tp_meta::{PgMeta, schema};
use tp_testkit::suite;

/// Набор создаёт организации сам (`TenantId::new()`), а записи транспорта ссылаются на `tenants`: обёртка
/// заводит строку организации перед записью (как Admin API — организацию до её настроек).
struct WithTenants(PgMeta);

impl WithTenants {
    async fn ensure(&self, tenant: TenantId) {
        let mut tx = schema::begin(self.0.pool(), tenant).await.expect("begin");
        sqlx::query(
            "INSERT INTO tenants (tenant_id, name) VALUES ($1, 'transport') ON CONFLICT DO NOTHING",
        )
        .bind(tenant.as_uuid())
        .execute(&mut *tx)
        .await
        .expect("tenant");
        tx.commit().await.expect("commit");
    }
}

#[async_trait]
impl TransportStore for WithTenants {
    async fn cluster(&self) -> Result<ClusterTransport> {
        self.0.cluster().await
    }

    async fn tenant(&self, tenant: TenantId) -> Result<TenantTransport> {
        self.0.tenant(tenant).await
    }

    async fn put_domain(&self, expected: u64, domain: &AcceptedDomain) -> Result<u64> {
        self.ensure(domain.tenant).await;
        self.0.put_domain(expected, domain).await
    }

    async fn delete_domain(&self, expected: u64, id: DomainId) -> Result<u64> {
        self.0.delete_domain(expected, id).await
    }

    async fn put_source(&self, expected: u64, source: &TrustedSource) -> Result<u64> {
        if let Some(t) = source.tenant {
            self.ensure(t).await;
        }
        self.0.put_source(expected, source).await
    }

    async fn delete_source(&self, expected: u64, name: &str) -> Result<u64> {
        self.0.delete_source(expected, name).await
    }

    async fn put_route(
        &self,
        tenant: TenantId,
        expected: u64,
        policy: &DestinationPolicy,
    ) -> Result<u64> {
        self.ensure(tenant).await;
        self.0.put_route(tenant, expected, policy).await
    }

    async fn delete_route(&self, tenant: TenantId, expected: u64, pattern: &str) -> Result<u64> {
        self.0.delete_route(tenant, expected, pattern).await
    }

    async fn put_rewrite(
        &self,
        tenant: TenantId,
        expected: u64,
        rule: &RewriteRule,
    ) -> Result<u64> {
        self.ensure(tenant).await;
        self.0.put_rewrite(tenant, expected, rule).await
    }

    async fn delete_rewrite(
        &self,
        tenant: TenantId,
        expected: u64,
        from: &AddressPattern,
    ) -> Result<u64> {
        self.0.delete_rewrite(tenant, expected, from).await
    }
}

/// Транзакция организации от имени роли приложения `thepost_app` (суперпользователь тестов обходит RLS).
async fn app_tx(
    pool: &tp_meta::pool::DbPool,
    tenant: TenantId,
) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut tx = schema::begin(pool, tenant).await.expect("begin");
    sqlx::query("SET LOCAL ROLE thepost_app")
        .execute(&mut *tx)
        .await
        .expect("role");
    tx
}

fn policy(pattern: &str) -> DestinationPolicy {
    DestinationPolicy {
        pattern: pattern.into(),
        tls: TlsRequirement::Required,
        smart_host: None,
        egress_ip: Some("2001:db8::25".parse().expect("ip")),
    }
}

fn gateway(name: &str, tenant: Option<TenantId>) -> TrustedSource {
    TrustedSource {
        name: name.into(),
        tenant,
        matcher: SourceMatch::Networks(vec!["203.0.113.0/24".parse().expect("net")]),
        kind: SourceKind::Gateway {
            xclient: true,
            authserv_id: Some("gw.example".into()),
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_and_cluster_signals() {
    let Some(db) = common::fresh_meta().await else {
        return;
    };
    suite::transport_store(&WithTenants(db.meta.clone())).await;

    let rec = Arc::new(common::RecordingBus::default());
    let meta = db.meta.clone().with_bus(rec.clone());
    let tenant = common::tenant(meta.pool()).await;
    let v0 = meta.cluster().await.expect("cluster").version;
    assert!(v0 > 0, "набор менял уровень кластера");

    // Домены, созданные вне TransportStore (до миграции), — полномочные.
    let (old, old_name) = common::domain(meta.pool(), tenant).await;
    let snap = meta.cluster().await.expect("cluster");
    assert_eq!(snap.version, v0);
    let found = snap.domain(&old_name).expect("domain");
    assert_eq!((found.id, &found.kind), (old, &DomainKind::Authoritative));

    // Сигнал — после фиксации, только при изменении; уровень кластера — `tenant: None`.
    let relay = AcceptedDomain {
        kind: DomainKind::InternalRelay {
            next_hop: "legacy.example:2525".into(),
        },
        ..found.clone()
    };
    let v1 = meta.put_domain(v0, &relay).await.expect("relay");
    assert_eq!(
        rec.take(),
        vec![Event {
            tenant: TenantId::DEFAULT,
            account: None,
            kind: EventKind::TransportChanged { tenant: None },
        }]
    );
    assert_eq!(meta.put_domain(v0, &relay).await, Err(Error::Conflict));
    assert_eq!(
        meta.delete_domain(v1, DomainId::new()).await,
        Err(Error::NotFound)
    );
    assert_eq!(rec.take(), vec![]);
    assert_eq!(
        meta.cluster().await.expect("cluster").domain(&old_name),
        Some(&relay)
    );

    // Домен с учётными записями не удаляется; версия не меняется.
    meta.upsert(
        tenant,
        AccountUpsert {
            domain: old,
            login: format!("user@{old_name}"),
            display_name: "User".into(),
            external_id: None,
            enabled: true,
            aliases: Vec::new(),
            kind: AccountKind::User,
        },
    )
    .await
    .expect("account");
    assert!(matches!(
        meta.delete_domain(v1, old).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(meta.cluster().await.expect("cluster").version, v1);

    // Источник кластера и организации: снимок кластера видит оба; JSON полей — без потерь.
    let shared = gateway("edge", None);
    let app = TrustedSource {
        name: "mfu".into(),
        tenant: Some(tenant),
        matcher: SourceMatch::ClientCert {
            subject: "mfu.example".into(),
        },
        kind: SourceKind::Application {
            senders: vec![AddressPattern::new(&format!("@{old_name}")).expect("p")],
            external: true,
        },
    };
    let v2 = meta.put_source(v1, &shared).await.expect("shared");
    let v3 = meta.put_source(v2, &app).await.expect("app");
    let snap = meta.cluster().await.expect("cluster");
    assert_eq!(snap.version, v3);
    assert_eq!(snap.sources, vec![shared.clone(), app.clone()]);
    let changed = |events: Vec<Event>| {
        events
            .into_iter()
            .filter(|e| matches!(e.kind, EventKind::TransportChanged { .. }))
            .count()
    };
    assert_eq!(changed(rec.take()), 2);

    // RLS: транзакция организации видит источник кластера и свой, но не чужой; источник кластера ей не записать.
    let other = common::tenant(meta.pool()).await;
    let visible = |t: TenantId| {
        let pool = meta.pool().clone();
        async move {
            let mut tx = app_tx(&pool, t).await;
            let names: Vec<String> =
                sqlx::query_scalar("SELECT name FROM transport_sources ORDER BY name")
                    .fetch_all(&mut *tx)
                    .await
                    .expect("select");
            tx.commit().await.expect("commit");
            names
        }
    };
    assert_eq!(visible(tenant).await, vec!["edge", "mfu"]);
    assert_eq!(visible(other).await, vec!["edge"]);
    let mut tx = app_tx(meta.pool(), other).await;
    let err =
        sqlx::query("INSERT INTO transport_sources (name, matcher, kind) VALUES ('x', '{}', '{}')")
            .execute(&mut *tx)
            .await
            .expect_err("RLS");
    assert!(err.to_string().contains("row-level security"), "{err}");
    drop(tx);

    let v4 = meta.delete_source(v3, "mfu").await.expect("delete");
    meta.delete_source(v4, "edge").await.expect("delete");
    db.drop_db().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tenant_routes_signals_and_isolation() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let rec = Arc::new(common::RecordingBus::default());
    let meta = PgMeta::new(pool, tp_meta::MetaConfig::default()).with_bus(rec.clone());
    let (tenant, other) = (
        common::tenant(meta.pool()).await,
        common::tenant(meta.pool()).await,
    );

    let t1 = meta
        .put_route(tenant, 0, &policy("example.net"))
        .await
        .expect("route");
    assert_eq!(
        rec.take(),
        vec![Event {
            tenant,
            account: None,
            kind: EventKind::TransportChanged {
                tenant: Some(tenant)
            },
        }]
    );
    let snap = meta.tenant(tenant).await.expect("tenant");
    assert_eq!(snap.version, t1);
    assert_eq!(snap.routes, vec![policy("example.net")]);

    // Устаревшая версия и неверные поля — без записи и сигнала.
    assert_eq!(
        meta.put_route(tenant, 0, &policy("*")).await,
        Err(Error::Conflict)
    );
    assert!(matches!(
        meta.put_route(tenant, t1, &policy("Example.net")).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        meta.delete_route(tenant, t1, "*").await,
        Err(Error::NotFound)
    );
    assert_eq!(rec.take(), vec![]);
    assert_eq!(meta.tenant(tenant).await.expect("tenant").version, t1);

    // Параллельная первая запись организации: одна успешна, другая — Conflict.
    let (pa, pb) = (policy("a.example"), policy("b.example"));
    let (a, b) = tokio::join!(meta.put_route(other, 0, &pa), meta.put_route(other, 0, &pb),);
    assert!(
        matches!(
            (&a, &b),
            (Ok(1), Err(Error::Conflict)) | (Err(Error::Conflict), Ok(1))
        ),
        "{a:?} {b:?}"
    );
    assert_eq!(meta.tenant(other).await.expect("other").routes.len(), 1);

    // RLS: политики организации не видны другой организации.
    let mut tx = app_tx(meta.pool(), other).await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM transport_routes WHERE tenant_id = $1")
        .bind(tenant.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    tx.commit().await.expect("commit");
    assert_eq!(n, 0);

    // Организации нет — Invalid (внешний ключ).
    assert!(matches!(
        meta.put_route(TenantId::new(), 0, &policy("*")).await,
        Err(Error::Invalid(_))
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn conformance_rewrites() {
    let Some(meta) = common::meta().await else {
        return;
    };
    suite::transport_rewrites(&WithTenants(meta)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rewrites_signals_and_isolation() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let rec = Arc::new(common::RecordingBus::default());
    let meta = PgMeta::new(pool, tp_meta::MetaConfig::default()).with_bus(rec.clone());
    let (tenant, other) = (
        common::tenant(meta.pool()).await,
        common::tenant(meta.pool()).await,
    );
    let rule = RewriteRule {
        from: AddressPattern::new("@old.example").expect("p"),
        to: AddressPattern::new("@new.example").expect("p"),
        sender: true,
        recipient: false,
    };
    let changed = Event {
        tenant,
        account: None,
        kind: EventKind::TransportChanged {
            tenant: Some(tenant),
        },
    };

    // Сигнал организации — после фиксации записи и удаления, без него — при Conflict и NotFound.
    let v1 = meta.put_rewrite(tenant, 0, &rule).await.expect("put");
    assert_eq!(rec.take(), vec![changed.clone()]);
    assert_eq!(
        meta.put_rewrite(tenant, 0, &rule).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        meta.delete_rewrite(
            tenant,
            v1,
            &AddressPattern::new("x@old.example").expect("p")
        )
        .await,
        Err(Error::NotFound)
    );
    assert_eq!(rec.take(), vec![]);
    assert_eq!(
        meta.tenant(tenant).await.expect("tenant").rewrites,
        vec![rule.clone()]
    );

    // RLS: правила организации не видны другой организации; чужое правило не записать.
    let mut tx = app_tx(meta.pool(), other).await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM transport_rewrites WHERE tenant_id = $1")
        .bind(tenant.as_uuid())
        .fetch_one(&mut *tx)
        .await
        .expect("count");
    assert_eq!(n, 0);
    let err = sqlx::query(
        "INSERT INTO transport_rewrites (tenant_id, from_addr, to_addr, sender, recipient)
         VALUES ($1, '@a.example', '@b.example', true, true)",
    )
    .bind(tenant.as_uuid())
    .execute(&mut *tx)
    .await
    .expect_err("RLS");
    assert!(err.to_string().contains("row-level security"), "{err}");
    drop(tx);

    let v2 = meta
        .delete_rewrite(tenant, v1, &rule.from)
        .await
        .expect("delete");
    assert!(v2 > v1);
    assert_eq!(rec.take(), vec![changed]);

    // Организации нет — Invalid (внешний ключ).
    assert!(matches!(
        meta.put_rewrite(TenantId::new(), 0, &rule).await,
        Err(Error::Invalid(_))
    ));
}
