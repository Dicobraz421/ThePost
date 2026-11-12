//! x/jmap-08: аудит доступа к ящику на PostgreSQL — общий набор `tp_testkit::suite::mailbox_audit`, удаление
//! секций по сроку хранения, отказ UPDATE и DELETE.

mod common;

use time::OffsetDateTime;
use tp_contracts::audit::{
    AuditAction, AuditActorKind, AuditEntry, AuditObject, AuditQuery, MailboxAudit,
};
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, Error, TenantId};
use tp_meta::schema::{self, db_err};
use tp_testkit::suite::mailbox_audit;

#[tokio::test]
async fn passes_contract_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    let foreign = common::tenant(meta.pool()).await;
    mailbox_audit(&meta, tenant, foreign).await;
}

fn entry(tenant: TenantId, at: OffsetDateTime) -> AuditEntry {
    AuditEntry {
        scope: AccountScope {
            tenant,
            account: AccountId::new(),
        },
        at,
        actor: "delegate@example.org".into(),
        actor_account: Some(AccountId::new()),
        actor_kind: AuditActorKind::Delegate,
        action: AuditAction::Read,
        object: AuditObject::default(),
        client: None,
    }
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
    for d in [100, 91, 0] {
        assert!(
            meta.record_audit(entry(t, now - day * d))
                .await
                .expect("record")
        );
    }
    let count = async || {
        meta.search_audit(t, &AuditQuery::all(100))
            .await
            .expect("search")
            .entries
            .len()
    };
    assert_eq!(count().await, 3);
    // Срок по умолчанию — 90 суток.
    meta.purge_expired_mailbox_audit().await.expect("purge");
    assert_eq!(count().await, 1);
    db.drop_db().await;
}

#[tokio::test]
async fn update_and_delete_are_rejected() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let t = common::tenant(meta.pool()).await;
    assert!(
        meta.record_audit(entry(t, OffsetDateTime::now_utc()))
            .await
            .expect("record")
    );
    for sql in [
        "UPDATE mailbox_audit SET detail = 'x' WHERE tenant_id = $1",
        "DELETE FROM mailbox_audit WHERE tenant_id = $1",
    ] {
        let mut tx = schema::begin(meta.pool(), t).await.expect("begin");
        let r = sqlx::query(sql)
            .bind(t.as_uuid())
            .execute(&mut *tx)
            .await
            .map_err(db_err);
        assert_eq!(r.map(|_| ()), Err(Error::Forbidden), "{sql}");
    }
}
