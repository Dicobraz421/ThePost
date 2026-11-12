//! META-61: срок корзины уровня папки у ящика второго шарда — как у ящика основной БД.

mod common;

use std::time::Duration;

use time::OffsetDateTime;
use tp_contracts::meta::{
    AccountScope, MailboxStore, MessageStore, NewMailbox, NewMessage, Policy, PolicyStore,
    PolicyTarget, SpecialUse,
};
use tp_contracts::retention::{TrashItem, TrashObject, TrashStore};
use tp_contracts::{BlobHash, BlobKey, Gen, MailboxId, MessageId};
use tp_meta::PgMeta;
use tp_meta::shards::ShardId;

const DAY: Duration = Duration::from_secs(24 * 3600);

fn message(scope: AccountScope) -> NewMessage {
    let mut h = [0u8; 32];
    h[..16].copy_from_slice(uuid::Uuid::now_v7().as_bytes());
    NewMessage {
        skeleton: BlobKey {
            tenant: scope.tenant,
            hash: BlobHash(h),
            generation: Gen::INITIAL,
        },
        parts: Vec::new(),
        size: 100,
        received_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("ts"),
        flags: Default::default(),
        meta: Default::default(),
    }
}

async fn create(
    meta: &PgMeta,
    scope: AccountScope,
    parent: Option<MailboxId>,
    name: &str,
) -> MailboxId {
    meta.create(
        scope,
        NewMailbox {
            parent,
            name: name.into(),
            role: None,
        },
    )
    .await
    .expect("create")
    .id
}

async fn inbox(meta: &PgMeta, scope: AccountScope) -> MailboxId {
    MailboxStore::list(meta, scope)
        .await
        .expect("list")
        .into_iter()
        .find(|m| m.role == Some(SpecialUse::Inbox))
        .expect("inbox")
        .id
}

async fn set_trash(meta: &PgMeta, scope: AccountScope, target: PolicyTarget, days: u32) {
    meta.set_policy(
        scope.tenant,
        target,
        &Policy {
            trash_retention: Some(DAY * days),
            ..Policy::default()
        },
    )
    .await
    .expect("policy");
}

/// Срок элементов корзины в сутках, по письмам.
async fn days(meta: &PgMeta, scope: AccountScope, ids: &[MessageId]) -> Vec<i64> {
    let items: Vec<TrashItem> = TrashStore::list(meta, scope, 0, 100).await.expect("trash");
    ids.iter()
        .map(|id| {
            let i = items
                .iter()
                .find(|i| i.object == TrashObject::Message(*id))
                .expect("item");
            (i.expires_at - i.deleted_at).whole_days()
        })
        .collect()
}

/// Перенос в корзину (`to_trash`) и каскадное удаление папок (`delete`): срок папки, учётной записи и
/// по умолчанию — одинаково для ящика основной БД и второго шарда.
async fn check(meta: &PgMeta, scope: AccountScope) {
    let inbox = inbox(meta, scope).await;
    let work = create(meta, scope, None, "Projects").await;
    let parent = create(meta, scope, None, "Old").await;
    let child = create(meta, scope, Some(parent), "2025").await;
    set_trash(meta, scope, PolicyTarget::Mailbox(work), 1).await;
    set_trash(meta, scope, PolicyTarget::Mailbox(child), 3).await;

    let a = meta.insert(scope, work, message(scope)).await.expect("a");
    let b = meta.insert(scope, inbox, message(scope)).await.expect("b");
    meta.to_trash(scope, &[a.id, b.id]).await.expect("to_trash");
    assert_eq!(days(meta, scope, &[a.id, b.id]).await, vec![1, 30]);

    // Учётная запись: действует для папок без своего срока, папка — по-прежнему своя.
    set_trash(meta, scope, PolicyTarget::Account(scope.account), 7).await;
    let p = meta.insert(scope, parent, message(scope)).await.expect("p");
    let c = meta.insert(scope, child, message(scope)).await.expect("c");
    meta.delete(scope, parent, true).await.expect("delete");
    assert_eq!(days(meta, scope, &[p.id, c.id]).await, vec![7, 3]);
}

#[tokio::test]
async fn folder_trash_retention_on_second_shard() {
    let Some(t) = common::two_shards().await else {
        return;
    };
    let org = t.tenant().await;
    let a = t.account_on(org.clone(), ShardId::MAIN).await;
    let b = t.account_on(org, t.shard).await;
    check(&t.meta, a).await;
    check(&t.meta, b).await;
    t.drop_db().await;
}
