//! META-23: раздельные учётные записи PostgreSQL. Учётные записи узлов создаёт тест (нужно право CREATEROLE у роли
//! `TEST_DATABASE_URL`, она же — владелец схемы): член `thepost_app` (общая роль узлов) и член `thepost_reader`
//! (search, indexer).

mod common;

use std::str::FromStr;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Executor, PgPool};
use time::OffsetDateTime;
use tp_contracts::delivery::{
    DeliveryDecision, DeliveryEvent, DeliveryEventKind, DeliveryLog, DeliveryQuery,
};
use tp_contracts::groups::{DistributionGroup, GroupStore, SenderRestriction};
use tp_contracts::membership::NodeId;
use tp_contracts::{Error, GroupId, TenantId};
use tp_meta::doctor::{self, DoctorConfig, Status};
use tp_meta::schema::{self, db_err};
use tp_meta::{MetaConfig, PgMeta};

/// Учётная запись узла — член групповой роли `group`; удаляется [`NodeLogin::drop_role`].
struct NodeLogin {
    pool: PgPool,
    role: String,
}

impl NodeLogin {
    async fn create(admin: &PgPool, url: &str, group: &str) -> Self {
        let role = format!("tp_node_{}", uuid::Uuid::now_v7().simple());
        let password = format!("pw-{role}");
        admin
            .execute(AssertSqlSafe(format!(
                "CREATE ROLE {role} LOGIN PASSWORD '{password}' IN ROLE {group}"
            )))
            .await
            .expect("роль узла");
        let opts = PgConnectOptions::from_str(url)
            .expect("url")
            .username(&role)
            .password(&password);
        let pool = PgPoolOptions::new()
            .max_connections(4)
            .connect_with(opts)
            .await
            .expect("вход ролью узла");
        Self { pool, role }
    }

    async fn drop_role(self, admin: &PgPool) {
        self.pool.close().await;
        admin
            .execute(AssertSqlSafe(format!("DROP ROLE {}", self.role)))
            .await
            .expect("drop role");
    }
}

/// Выполняет `sql` в транзакции организации `tenant` ролью `pool`; `$1` в `sql` — организация.
async fn exec(pool: &PgPool, tenant: TenantId, sql: &str) -> Result<(), Error> {
    let mut tx = schema::begin(pool, tenant).await?;
    let q = sqlx::query(AssertSqlSafe(sql.to_owned()));
    let q = if sql.contains("$1") {
        q.bind(tenant.as_uuid())
    } else {
        q
    };
    q.execute(&mut *tx).await.map_err(db_err)?;
    tx.commit().await.map_err(db_err)
}

#[tokio::test]
async fn reader_does_not_modify_message_metadata() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let Some(admin) = common::meta().await else {
        return;
    };
    let scope = common::account(&admin).await;
    let t = scope.tenant;
    let reader = NodeLogin::create(admin.pool(), &url, "thepost_reader").await;

    // Чтение метаданных — под RLS организации.
    let mut tx = schema::begin(&reader.pool, t).await.expect("begin");
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM accounts")
        .fetch_one(&mut *tx)
        .await
        .expect("select");
    assert_eq!(n, 1);
    tx.rollback().await.expect("rollback");

    for sql in [
        "UPDATE messages SET flags = '{}' WHERE tenant_id = $1",
        "DELETE FROM messages WHERE tenant_id = $1",
        "UPDATE mailboxes SET subscribed = false WHERE tenant_id = $1",
        "UPDATE accounts SET display_name = 'x' WHERE tenant_id = $1",
        "DELETE FROM blobs WHERE tenant_id = $1",
        "INSERT INTO domains (tenant_id, domain_id, name) VALUES ($1, gen_random_uuid(), 'x.test')",
        "INSERT INTO delivery_log (tenant_id, at, node, kind, sender, sender_key, recipients, rcpt_keys, decision)
         VALUES ($1, now(), 'n', 'receive', '', '', '{}', '{}', 'accept')",
    ] {
        assert_eq!(exec(&reader.pool, t, sql).await, Err(Error::Forbidden), "{sql}");
    }
    // Служебные таблицы узла (аренды, членство, позиции подписчиков, манифесты) — доступны на запись.
    let mut tx = schema::begin(&reader.pool, t).await.expect("begin");
    sqlx::query("DELETE FROM leases WHERE name = $1")
        .bind(format!("tp-reader-{}", reader.role))
        .execute(&mut *tx)
        .await
        .expect("leases");
    tx.rollback().await.expect("rollback");

    reader.drop_role(admin.pool()).await;
}

#[tokio::test]
async fn app_role_works_but_cannot_migrate_or_rewrite_logs() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let Some(admin) = common::meta().await else {
        return;
    };
    let t = common::tenant(admin.pool()).await;
    let app = NodeLogin::create(admin.pool(), &url, "thepost_app").await;
    let meta = PgMeta::new(
        app.pool.clone(),
        MetaConfig {
            migrate_on_start: false,
            ..MetaConfig::default()
        },
    );

    // Обычная работа узла: группы, журнал доставки (секция суток создаётся функцией владельца схемы).
    let g = DistributionGroup {
        id: GroupId::new(),
        tenant: t,
        name: "app".into(),
        address: None,
        external_id: None,
        members: Vec::new(),
        senders: SenderRestriction::Anyone,
        moderators: Vec::new(),
        hidden_from_gal: false,
    };
    meta.put(g.clone()).await.expect("put");
    assert_eq!(GroupStore::get(&meta, t, g.id).await, Ok(Some(g)));
    let tag = uuid::Uuid::now_v7().simple().to_string();
    // Сутки далеко в будущем: секцию создаёт роль приложения, а не другой тест.
    let at = OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("time")
        + time::Duration::days(3650);
    let event = DeliveryEvent {
        tenant: t,
        at,
        node: NodeId::new("app-node").expect("node"),
        kind: DeliveryEventKind::Receive,
        sender: format!("{tag}@example.org"),
        recipients: vec!["bob@example.org".into()],
        message_id: Some(format!("{tag}@example.org")),
        queue_id: None,
        decision: DeliveryDecision::Accept,
        reason: None,
    };
    meta.record(event.clone()).await.expect("record");
    let mut q = DeliveryQuery::all(10);
    q.message_id = event.message_id.clone();
    assert_eq!(
        meta.search(t, &q).await.expect("search").events,
        vec![event]
    );

    // Журналы — только добавление; секции напрямую недоступны; DDL и миграции — нет.
    let day = at.date();
    let part = format!(
        "delivery_log_{:04}{:02}{:02}",
        day.year(),
        u8::from(day.month()),
        day.day()
    );
    for sql in [
        "UPDATE delivery_log SET reason = 'x' WHERE tenant_id = $1".to_owned(),
        "DELETE FROM delivery_log WHERE tenant_id = $1".to_owned(),
        "UPDATE audit_log SET record = '{}' WHERE tenant_id = $1".to_owned(),
        format!("SELECT count(*) FROM {part} WHERE tenant_id = $1"),
        "CREATE TABLE tp_app_probe (id int)".to_owned(),
        "ALTER TABLE messages ADD COLUMN probe int".to_owned(),
        "SELECT count(*) FROM _sqlx_migrations WHERE $1::uuid IS NOT NULL".to_owned(),
    ] {
        assert_eq!(
            exec(&app.pool, t, &sql).await,
            Err(Error::Forbidden),
            "{sql}"
        );
    }
    assert!(
        schema::migrate(&app.pool).await.is_err(),
        "миграции от роли приложения отклоняются"
    );

    // doctor: роль узла — не владелец схемы; владелец (роль теста) — не соответствует.
    let report = doctor::check(&app.pool, &DoctorConfig::default())
        .await
        .expect("doctor");
    assert_eq!(report.get("node_role").map(|c| c.status), Some(Status::Ok));
    assert_eq!(report.get("rls").map(|c| c.status), Some(Status::Ok));
    let report = doctor::check(admin.pool(), &DoctorConfig::default())
        .await
        .expect("doctor");
    assert_eq!(
        report.get("node_role").map(|c| c.status),
        Some(Status::Fail)
    );

    app.drop_role(admin.pool()).await;
}
