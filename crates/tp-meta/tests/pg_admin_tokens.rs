//! META-43: локальные токены Admin API на PostgreSQL — общий набор `tp_testkit::suite::local_admin_tokens` и гонка
//! погашений одного токена.

mod common;

use std::time::Duration;

use time::OffsetDateTime;
use tp_contracts::admin_token::{LocalAdminToken, LocalAdminTokens};
use tp_contracts::{AccountId, Error};
use tp_testkit::suite;

#[tokio::test]
async fn passes_contract_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    suite::local_admin_tokens(&meta, tenant).await;
}

/// Параллельные погашения одного токена на разных соединениях: успешно ровно одно, остальные — `Forbidden`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_consume_succeeds_once() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    let now = OffsetDateTime::from_unix_timestamp(OffsetDateTime::now_utc().unix_timestamp())
        .expect("now");
    let (a, b) = (AccountId::new(), AccountId::new());
    let token = LocalAdminToken {
        id: format!("race-{a}"),
        tenant,
        account: AccountId::new(),
        hash: format!(
            "{:032x}{:032x}",
            a.as_uuid().as_u128(),
            b.as_uuid().as_u128()
        ),
        created_at: now,
        // Срок с запасом: очистка в параллельном наборе (`prune` до now + 10 мин) этот токен не удаляет.
        expires_at: now + Duration::from_secs(86_400),
        used_at: None,
        revoked_at: None,
    };
    meta.issue(token.clone()).await.expect("issue");

    let at = now + Duration::from_secs(1);
    let tasks: Vec<_> = (0..8)
        .map(|_| {
            let meta = meta.clone();
            let hash = token.hash.clone();
            tokio::spawn(async move { meta.consume(&hash, at).await })
        })
        .collect();
    let mut ok = 0;
    for t in tasks {
        match t.await.expect("join") {
            Ok(got) => {
                ok += 1;
                assert_eq!(got.used_at, Some(at));
                assert_eq!(got.id, token.id);
            }
            Err(e) => assert_eq!(e, Error::Forbidden),
        }
    }
    assert_eq!(ok, 1);

    let list = meta.list(tenant).await.expect("list");
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].used_at, Some(at));
}
