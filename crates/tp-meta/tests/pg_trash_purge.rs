//! META-29: «очистить корзину» и скрытые письма на PostgreSQL — общий набор `tp_testkit::suite::trash_purge`,
//! хранение правила `trash.purge` в политиках, истечение срока скрытых и журнал изменений.

mod common;

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::ext::DecisionHooks;
use tp_contracts::meta::{
    AccountScope, ChangeLog, MailboxStore, MessageStore, NewMessage, Policy, SpecialUse,
};
use tp_contracts::retention::{Quota, TrashObject, TrashPurge, TrashPurgeRule, TrashStore};
use tp_contracts::{BlobHash, BlobKey, Gen, MailboxId, TenantId};
use tp_meta::PgMeta;
use tp_meta::retention::PolicyTarget;
use tp_testkit::suite::{TrashPurgeAdmin, TrashPurgeFixture, trash_purge};

/// Правило организации — через `PgMeta::set_policy`, прочие поля уровня сохраняются.
struct Admin<'a>(&'a PgMeta);

#[async_trait]
impl TrashPurgeAdmin for Admin<'_> {
    async fn set_tenant_rule(&self, tenant: TenantId, rule: Option<TrashPurgeRule>) {
        let mut p = self
            .0
            .policy(tenant, PolicyTarget::Tenant)
            .await
            .expect("policy");
        p.trash_purge = rule;
        self.0
            .set_policy(tenant, PolicyTarget::Tenant, &p)
            .await
            .expect("set policy");
    }
}

#[tokio::test]
async fn passes_contract_suite() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let fx = TrashPurgeFixture {
        scope: common::account(&meta).await,
    };
    trash_purge(&meta, &meta, &meta, &meta, &meta, &Admin(&meta), &fx).await;
}

fn message(scope: AccountScope, size: u64) -> NewMessage {
    let mut h = [0u8; 32];
    h[..16].copy_from_slice(uuid::Uuid::now_v7().as_bytes());
    NewMessage {
        skeleton: BlobKey {
            tenant: scope.tenant,
            hash: BlobHash(h),
            generation: Gen::INITIAL,
        },
        parts: Vec::new(),
        size,
        received_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("ts"),
        flags: Default::default(),
        meta: Default::default(),
    }
}

async fn folder(meta: &PgMeta, scope: AccountScope, role: SpecialUse) -> MailboxId {
    MailboxStore::list(meta, scope)
        .await
        .expect("list")
        .into_iter()
        .find(|m| m.role == Some(role))
        .expect("folder")
        .id
}

#[tokio::test]
async fn rule_is_stored_per_level() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let t = scope.tenant;
    let rule = TrashPurgeRule {
        mode: TrashPurge::Erase,
        user_can_choose: true,
    };
    let only_rule = Policy {
        trash_purge: Some(rule),
        ..Policy::default()
    };
    meta.set_policy(t, PolicyTarget::Account(scope.account), &only_rule)
        .await
        .expect("set");
    assert_eq!(
        meta.policy(t, PolicyTarget::Account(scope.account))
            .await
            .expect("policy"),
        only_rule,
        "уровень только с правилом trash.purge не считается пустым"
    );
    let setting = tp_contracts::meta::PolicyResolver::trash_purge(&meta, scope)
        .await
        .expect("setting");
    assert_eq!(setting.rule, rule);
    meta.set_policy(t, PolicyTarget::Account(scope.account), &Policy::default())
        .await
        .expect("clear");
    assert_eq!(
        tp_contracts::meta::PolicyResolver::trash_purge(&meta, scope)
            .await
            .expect("setting")
            .rule,
        TrashPurgeRule::default()
    );
    let missing = AccountScope {
        tenant: t,
        account: tp_contracts::AccountId::new(),
    };
    assert_eq!(
        tp_contracts::meta::PolicyResolver::trash_purge(&meta, missing).await,
        Err(tp_contracts::Error::NotFound)
    );
    assert_eq!(
        tp_contracts::meta::PolicyResolver::set_user_trash_purge(&meta, missing, None).await,
        Err(tp_contracts::Error::NotFound)
    );
}

#[tokio::test]
async fn hidden_changes_expiry_and_final_delete() {
    let Some(db) = common::fresh_meta().await else {
        return;
    };
    let meta = &db.meta;
    let scope = common::account(meta).await;
    let inbox = folder(meta, scope, SpecialUse::Inbox).await;
    let bin = folder(meta, scope, SpecialUse::Trash).await;
    let m = meta
        .insert(scope, inbox, message(scope, 40))
        .await
        .expect("insert")
        .id;
    let before = meta.to_trash(scope, &[m]).await.expect("trash");
    let out = meta
        .purge(scope, &[m], &DecisionHooks::default())
        .await
        .expect("purge");
    assert_eq!(out.hidden, [m]);
    assert!(out.modseq > before);

    // Журнал: письмо удалено из корзины.
    let ch = meta
        .message_changes(scope, Some(bin), before, 100)
        .await
        .expect("changes");
    assert_eq!(ch.destroyed.iter().map(|c| c.id).collect::<Vec<_>>(), [m]);
    assert_eq!(meta.usage(scope).await.expect("usage").used_bytes, 0);

    // Срок корзины истекает и для скрытого: отложить и окончательно удалить.
    let item = meta
        .list_hidden(scope, 0, 10)
        .await
        .expect("hidden")
        .pop()
        .expect("item");
    let far = item.expires_at + time::Duration::seconds(1);
    let exp = meta.expired(far, 10).await.expect("expired");
    assert_eq!(
        exp.iter().map(|i| i.object).collect::<Vec<_>>(),
        [TrashObject::Message(m)]
    );
    let until = far + time::Duration::days(1);
    meta.defer(scope, TrashObject::Message(m), until)
        .await
        .expect("defer hidden");
    assert!(meta.expired(far, 10).await.expect("expired").is_empty());
    let state = meta
        .status(scope, bin)
        .await
        .expect("status")
        .highest_modseq;
    meta.expunge_final(scope, &[m]).await.expect("expunge");
    assert!(
        meta.list_hidden(scope, 0, 10)
            .await
            .expect("hidden")
            .is_empty()
    );
    let ch = meta
        .message_changes(scope, Some(bin), state, 100)
        .await
        .expect("changes");
    assert!(
        ch.destroyed.is_empty(),
        "скрытое стирается без повторного удаления в журнале"
    );
    db.drop_db().await;
}
