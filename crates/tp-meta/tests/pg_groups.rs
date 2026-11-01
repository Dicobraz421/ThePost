//! META-21: группы рассылки на PostgreSQL — общий набор `tp_testkit::suite::group_store` (вложенность, циклы,
//! изоляция организаций) и раскрытие глубокой вложенности одним запросом.

mod common;

use tp_contracts::groups::{DistributionGroup, GroupMember, GroupStore, SenderRestriction};
use tp_contracts::meta::{AccountStore, AccountUpsert};
use tp_contracts::{AccountId, Error, GroupId, TenantId};
use tp_meta::PgMeta;
use tp_testkit::suite::{GroupFixture, group_store};

async fn accounts(meta: &PgMeta, tenant: TenantId) -> [AccountId; 4] {
    let (domain, name) = common::domain(meta.pool(), tenant).await;
    let mut out = [AccountId::new(); 4];
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = meta
            .upsert(
                tenant,
                AccountUpsert {
                    domain,
                    login: format!("u{i}@{name}"),
                    display_name: format!("User {i}"),
                    external_id: None,
                    enabled: true,
                },
            )
            .await
            .expect("upsert")
            .id;
    }
    out
}

#[tokio::test]
async fn passes_contract_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    let fx = GroupFixture {
        tenant,
        accounts: accounts(&meta, tenant).await,
        foreign_tenant: common::tenant(meta.pool()).await,
    };
    group_store(&meta, &fx).await;
}

fn group(tenant: TenantId, members: Vec<GroupMember>) -> DistributionGroup {
    DistributionGroup {
        id: GroupId::new(),
        tenant,
        name: "chain".into(),
        address: None,
        external_id: None,
        members,
        senders: SenderRestriction::Anyone,
        moderators: Vec::new(),
        hidden_from_gal: false,
    }
}

#[tokio::test]
async fn deep_chain_with_cycle_and_foreign_tenant() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let t = common::tenant(meta.pool()).await;
    let foreign = common::tenant(meta.pool()).await;

    // Цепочка g0 → g1 → … → g49 → g0, у каждой группы своя учётная запись.
    let ids: Vec<GroupId> = (0..50).map(|_| GroupId::new()).collect();
    let mut expected: Vec<AccountId> = Vec::new();
    for (i, id) in ids.iter().enumerate() {
        let a = AccountId::new();
        expected.push(a);
        let next = ids[(i + 1) % ids.len()];
        let g = DistributionGroup {
            id: *id,
            ..group(t, vec![GroupMember::Account(a), GroupMember::Group(next)])
        };
        meta.put(g).await.expect("put");
    }
    expected.sort();
    for id in [ids[0], ids[17], ids[49]] {
        assert_eq!(meta.expand(t, id).await, Ok(expected.clone()));
    }

    // Группа другой организации с тем же членством не раскрывается в чужие группы.
    let alien = group(foreign, vec![GroupMember::Group(ids[0])]);
    meta.put(alien.clone()).await.expect("put foreign");
    assert_eq!(meta.expand(foreign, alien.id).await, Ok(Vec::new()));
    assert_eq!(meta.expand(foreign, ids[0]).await, Err(Error::NotFound));
    assert_eq!(GroupStore::get(&meta, foreign, ids[0]).await, Ok(None));
    // Тот же идентификатор группы в другой организации — отдельная группа.
    let twin = DistributionGroup {
        id: ids[0],
        ..group(foreign, Vec::new())
    };
    meta.put(twin.clone()).await.expect("twin");
    assert_eq!(
        GroupStore::get(&meta, foreign, ids[0]).await,
        Ok(Some(twin))
    );
    assert_eq!(meta.expand(t, ids[0]).await, Ok(expected));
}
