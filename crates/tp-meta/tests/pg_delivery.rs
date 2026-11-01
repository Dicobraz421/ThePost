//! META-22: журнал доставки на PostgreSQL — общий набор `tp_testkit::suite::delivery_log`, удаление секций
//! по сроку хранения, отказ UPDATE и DELETE в журналах доставки и аудита.

mod common;

use time::OffsetDateTime;
use tp_contracts::delivery::{
    DeliveryDecision, DeliveryEvent, DeliveryEventKind, DeliveryLog, DeliveryQuery,
};
use tp_contracts::membership::NodeId;
use tp_contracts::{Error, TenantId};
use tp_meta::schema::{self, db_err};
use tp_testkit::suite::{DeliveryFixture, delivery_log};

#[tokio::test]
async fn passes_contract_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let fx = DeliveryFixture {
        tenant: common::tenant(meta.pool()).await,
        foreign_tenant: common::tenant(meta.pool()).await,
    };
    delivery_log(&meta, &fx).await;
}

fn event(tenant: TenantId, at: OffsetDateTime, tag: &str) -> DeliveryEvent {
    DeliveryEvent {
        tenant,
        at,
        node: NodeId::new("purge-node").expect("node"),
        kind: DeliveryEventKind::Deliver,
        sender: format!("{tag}@example.org"),
        recipients: vec!["bob@example.org".into()],
        message_id: Some(format!("{tag}@example.org")),
        queue_id: None,
        decision: DeliveryDecision::Accept,
        reason: None,
    }
}

async fn sections(pool: &sqlx::PgPool) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT c.relname::text FROM pg_inherits i JOIN pg_class c ON c.oid = i.inhrelid
         WHERE i.inhparent = 'delivery_log'::regclass ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .expect("sections")
}

#[tokio::test]
async fn purge_drops_expired_sections() {
    let Some(db) = common::fresh_meta().await else {
        return;
    };
    let meta = &db.meta;
    let t = common::tenant(meta.pool()).await;
    let now = OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("time");
    let day = time::Duration::days(1);
    let events: Vec<_> = [40, 31, 29, 0]
        .iter()
        .map(|d| event(t, now - day * *d, &format!("d{d}")))
        .collect();
    meta.record_batch(events.clone()).await.expect("batch");
    // Повторная запись в существующие секции не создаёт новых.
    meta.record(events[3].clone()).await.expect("record");
    assert_eq!(sections(meta.pool()).await.len(), 4);

    let all = async || {
        meta.search(t, &DeliveryQuery::all(100))
            .await
            .expect("search")
            .events
    };
    assert_eq!(all().await.len(), 5);

    // Срок по умолчанию — 30 суток: секции 40 и 31 сутки назад удалены целиком.
    meta.purge_expired_delivery().await.expect("purge");
    assert_eq!(sections(meta.pool()).await.len(), 2);
    let left = all().await;
    assert_eq!(
        left,
        vec![events[2].clone(), events[3].clone(), events[3].clone()]
    );

    // Сутки, на которые приходится граница, остаются; повтор — без изменений.
    meta.purge(now).await.expect("purge");
    meta.purge(now).await.expect("purge again");
    assert_eq!(all().await, vec![events[3].clone(), events[3].clone()]);
    assert_eq!(sections(meta.pool()).await.len(), 1);
    db.drop_db().await;
}

#[tokio::test]
async fn update_and_delete_are_rejected() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let t = common::tenant(meta.pool()).await;
    let now = OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("time");
    meta.record(event(
        t,
        now,
        &format!("ao-{}", uuid::Uuid::now_v7().simple()),
    ))
    .await
    .expect("record");

    let mut tx = schema::begin(meta.pool(), t).await.expect("begin");
    sqlx::query(
        "INSERT INTO audit_log (at, tenant_id, actor_tenant, actor, record) VALUES (now(), $1, $1, $1, '{}')",
    )
    .bind(t.as_uuid())
    .execute(&mut *tx)
    .await
    .expect("audit insert");
    tx.commit().await.expect("commit");

    for sql in [
        "UPDATE delivery_log SET reason = 'x' WHERE tenant_id = $1",
        "DELETE FROM delivery_log WHERE tenant_id = $1",
        "UPDATE audit_log SET record = '{}' WHERE tenant_id = $1",
        "DELETE FROM audit_log WHERE tenant_id = $1",
    ] {
        let mut tx = schema::begin(meta.pool(), t).await.expect("begin");
        let r = sqlx::query(sql)
            .bind(t.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(db_err);
        assert_eq!(r.map(|_| ()), Err(Error::Forbidden), "{sql}");
    }
    let n = meta
        .search(t, &DeliveryQuery::all(10))
        .await
        .expect("search")
        .events
        .len();
    assert_eq!(n, 1);
}
