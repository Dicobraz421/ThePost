//! x/jmap-08: отправки JMAP на PostgreSQL — общие наборы `tp_testkit::suite::{submission_store, submission_query}`,
//! нижняя граница истории изменений после удаления надгробий, отправки учётной записи второго шарда.

mod common;

use std::time::Duration;

use time::OffsetDateTime;
use tp_contracts::meta::{AccountScope, AccountStore, AccountUpsert};
use tp_contracts::submission::{EmailSubmission, SubmissionStore, UndoStatus};
use tp_contracts::{DomainId, Error, MessageId, TenantId};
use tp_meta::PgMeta;
use tp_meta::schema;
use tp_testkit::suite::{submission_query, submission_store};

async fn account_in(meta: &PgMeta, tenant: TenantId, domain: (DomainId, &str)) -> AccountScope {
    let acc = meta
        .upsert(
            tenant,
            AccountUpsert {
                domain: domain.0,
                login: format!("u{}@{}", uuid::Uuid::now_v7().simple(), domain.1),
                display_name: "User".into(),
                external_id: None,
                enabled: true,
                aliases: Vec::new(),
                kind: tp_contracts::meta::AccountKind::User,
                hidden_from_gal: false,
            },
        )
        .await
        .expect("upsert");
    AccountScope {
        tenant,
        account: acc.id,
    }
}

async fn two_accounts(meta: &PgMeta) -> (AccountScope, AccountScope) {
    let tenant = common::tenant(meta.pool()).await;
    let (domain, name) = common::domain(meta.pool(), tenant).await;
    (
        account_in(meta, tenant, (domain, &name)).await,
        account_in(meta, tenant, (domain, &name)).await,
    )
}

fn submission(id: &str, send_at: OffsetDateTime, undo: UndoStatus) -> EmailSubmission {
    EmailSubmission {
        id: id.into(),
        identity: "work".into(),
        email: MessageId::new(),
        mail_from: "anna@example.org".into(),
        rcpt_to: vec!["b@example.com".into()],
        send_at,
        undo_status: undo,
        delivery: Vec::new(),
        queue: None,
    }
}

#[tokio::test]
async fn passes_contract_suites() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let (owner, peer) = two_accounts(&meta).await;
    submission_store(&meta, owner, peer).await;
    let (owner, peer) = two_accounts(&meta).await;
    submission_query(&meta, owner, peer).await;
}

/// Надгробия старше срока удаляются, изменения от состояния до удаления — `Conflict`; идентификатор удалённой
/// отправки можно занять снова. Чистая БД: удаление надгробий — по всем организациям.
#[tokio::test]
async fn tombstones_expire_and_raise_floor() {
    let Some(db) = common::fresh_meta().await else {
        return;
    };
    let meta = &db.meta;
    let (owner, _) = two_accounts(meta).await;
    let store: &dyn SubmissionStore = meta;
    let old = OffsetDateTime::UNIX_EPOCH + time::Duration::days(10);
    let s0 = store.state(owner).await.expect("s0");
    store
        .insert(owner, submission("a", old, UndoStatus::Final))
        .await
        .expect("a");
    store
        .insert(owner, submission("b", old, UndoStatus::Final))
        .await
        .expect("b");
    store.delete(owner, "a").await.expect("delete a");
    let s1 = store.state(owner).await.expect("s1");
    let ch = store.submission_changes(owner, s0, 10).await.expect("ch");
    assert_eq!((ch.created, ch.destroyed.len()), (vec!["b".to_owned()], 0));

    // Срок — после удаления `a`: удаляются `b` и надгробие `a`.
    tokio::time::sleep(Duration::from_millis(10)).await;
    let now = OffsetDateTime::now_utc();
    assert_eq!(store.expire_submissions(now, 100).await, Ok(1));
    assert_eq!(store.get(owner, "b").await.expect("b"), None);
    let s2 = store.state(owner).await.expect("s2");
    assert!(s2 > s1);
    let ch = store.submission_changes(owner, s1, 10).await.expect("s1");
    assert_eq!(ch.destroyed, ["b"]);
    assert_eq!(
        store.submission_changes(owner, s0, 10).await,
        Err(Error::Conflict)
    );

    // Повторная очистка снимает и надгробие `b`; занятый прежде идентификатор свободен.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert_eq!(
        store
            .expire_submissions(OffsetDateTime::now_utc(), 100)
            .await,
        Ok(0)
    );
    assert_eq!(
        store.submission_changes(owner, s1, 10).await,
        Err(Error::Conflict)
    );
    let s3 = meta
        .insert(owner, submission("a", old, UndoStatus::Pending))
        .await
        .expect("a again");
    let ch = store.submission_changes(owner, s2, 10).await.expect("s2");
    assert_eq!(ch.created, ["a"]);
    assert_eq!(ch.new_state, s3);
    db.drop_db().await;
}

/// Отправки учётной записи второго шарда — в БД этого шарда; наступившие и срок хранения обходят все шарды.
#[tokio::test]
async fn submissions_live_on_account_shard() {
    let Some(t) = common::two_shards().await else {
        return;
    };
    let scope = t.account_on(t.tenant().await, t.shard).await;
    let store: &dyn SubmissionStore = &t.meta;
    let tenant = scope.tenant;
    let old = OffsetDateTime::UNIX_EPOCH + time::Duration::days(10);
    store
        .insert(scope, submission("due", old, UndoStatus::Pending))
        .await
        .expect("insert");
    store
        .insert(scope, submission("done", old, UndoStatus::Final))
        .await
        .expect("insert done");
    let count = async |pool: &tp_meta::pool::DbPool| -> i64 {
        let mut tx = schema::begin_system(pool).await.expect("begin");
        let n = sqlx::query_scalar("SELECT count(*) FROM email_submissions WHERE tenant_id = $1")
            .bind(tenant.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .expect("count");
        tx.rollback().await.expect("rollback");
        n
    };
    assert_eq!(count(&t.shard_pool).await, 2);
    assert_eq!(count(t.main.meta.pool()).await, 0);
    let due = store.due(OffsetDateTime::now_utc(), 10).await.expect("due");
    assert_eq!(
        due.into_iter().map(|(s, d)| (s, d.id)).collect::<Vec<_>>(),
        [(scope, "due".to_owned())]
    );
    assert_eq!(
        store
            .expire_submissions(OffsetDateTime::now_utc(), 10)
            .await,
        Ok(1)
    );
    assert_eq!(store.state(scope).await.expect("state"), 3);
    t.drop_db().await;
}
