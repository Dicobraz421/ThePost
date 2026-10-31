//! META-10: корзина, квоты, политики на PostgreSQL.

mod common;

use std::time::Duration;

use time::OffsetDateTime;
use tp_contracts::meta::{
    AccountScope, AccountStore, MailboxStore, MessageStore, NewMailbox, NewMessage, Policy,
    PolicyResolver, SpecialUse,
};
use tp_contracts::retention::{Quota, Restored, TrashObject, TrashStore};
use tp_contracts::{BlobHash, BlobKey, Error, Gen, MailboxId};
use tp_meta::PgMeta;
use tp_meta::retention::PolicyTarget;

const DAY: Duration = Duration::from_secs(24 * 3600);

fn key(scope: AccountScope) -> BlobKey {
    let mut h = [0u8; 32];
    h[..16].copy_from_slice(uuid::Uuid::now_v7().as_bytes());
    BlobKey {
        tenant: scope.tenant,
        hash: BlobHash(h),
        generation: Gen::INITIAL,
    }
}

fn message(scope: AccountScope, size: u64) -> NewMessage {
    NewMessage {
        skeleton: key(scope),
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

async fn domain_of(meta: &PgMeta, scope: AccountScope) -> tp_contracts::DomainId {
    AccountStore::get(meta, scope.tenant, scope.account)
        .await
        .expect("get")
        .expect("account")
        .domain
}

#[tokio::test]
async fn policy_chain_resolution() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let t = scope.tenant;
    let domain = domain_of(&meta, scope).await;
    let inbox = folder(&meta, scope, SpecialUse::Inbox).await;

    assert_eq!(
        meta.resolve(t, Some(domain), Some(scope.account), Some(inbox))
            .await
            .expect("resolve"),
        Policy::default()
    );
    let set = |target, p: Policy| {
        let meta = meta.clone();
        async move { meta.set_policy(t, target, &p).await.expect("set") }
    };
    set(
        PolicyTarget::Tenant,
        Policy {
            quota_bytes: Some(1000),
            trash_retention: Some(DAY * 10),
            dkim_selector: Some("s1".into()),
            ..Policy::default()
        },
    )
    .await;
    set(
        PolicyTarget::Domain(domain),
        Policy {
            quota_bytes: Some(500),
            send_per_hour: Some(100),
            ..Policy::default()
        },
    )
    .await;
    set(
        PolicyTarget::Account(scope.account),
        Policy {
            trash_retention: Some(DAY * 5),
            max_recipients: Some(50),
            ..Policy::default()
        },
    )
    .await;
    set(
        PolicyTarget::Mailbox(inbox),
        Policy {
            retention: Some(DAY * 365),
            trash_retention: Some(DAY),
            milter_profile: Some("strict".into()),
            ..Policy::default()
        },
    )
    .await;

    let tenant_only = meta.resolve(t, None, None, None).await.expect("tenant");
    assert_eq!(tenant_only.quota_bytes, Some(1000));
    assert_eq!(tenant_only.send_per_hour, None);
    let dom = meta
        .resolve(t, Some(domain), None, None)
        .await
        .expect("domain");
    assert_eq!(
        (dom.quota_bytes, dom.trash_retention, dom.send_per_hour),
        (Some(500), Some(DAY * 10), Some(100))
    );
    let acc = meta
        .resolve(t, Some(domain), Some(scope.account), None)
        .await
        .expect("account");
    assert_eq!(acc.trash_retention, Some(DAY * 5));
    assert_eq!(acc.max_recipients, Some(50));
    assert_eq!(acc.dkim_selector.as_deref(), Some("s1"));
    let all = meta
        .resolve(t, Some(domain), Some(scope.account), Some(inbox))
        .await
        .expect("mailbox");
    assert_eq!(
        all,
        Policy {
            quota_bytes: Some(500),
            trash_retention: Some(DAY),
            retention: Some(DAY * 365),
            send_per_hour: Some(100),
            max_recipients: Some(50),
            dkim_selector: Some("s1".into()),
            milter_profile: Some("strict".into()),
        }
    );

    // Уровень без заданных полей удаляется; политика уровня читается без наложения.
    assert_eq!(
        meta.policy(t, PolicyTarget::Domain(domain))
            .await
            .expect("policy")
            .quota_bytes,
        Some(500)
    );
    set(PolicyTarget::Domain(domain), Policy::default()).await;
    assert_eq!(
        meta.policy(t, PolicyTarget::Domain(domain))
            .await
            .expect("policy"),
        Policy::default()
    );
    let acc = meta
        .resolve(t, Some(domain), Some(scope.account), None)
        .await
        .expect("account");
    assert_eq!((acc.quota_bytes, acc.send_per_hour), (Some(1000), None));

    // Политики другой организации не видны.
    let other = common::account(&meta).await;
    assert_eq!(
        meta.resolve(other.tenant, None, None, None)
            .await
            .expect("other"),
        Policy::default()
    );
}

#[tokio::test]
async fn quota_counts_messages_and_trash() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let inbox = folder(&meta, scope, SpecialUse::Inbox).await;
    let u = meta.usage(scope).await.expect("usage");
    assert_eq!((u.used_bytes, u.messages, u.limit_bytes), (0, 0, None));
    meta.check(scope, u64::MAX).await.expect("без лимита");

    let a = meta
        .insert(scope, inbox, message(scope, 300))
        .await
        .expect("insert");
    meta.insert(scope, inbox, message(scope, 200))
        .await
        .expect("insert");
    meta.to_trash(scope, &[a.id]).await.expect("trash");
    meta.set_policy(
        scope.tenant,
        PolicyTarget::Account(scope.account),
        &Policy {
            quota_bytes: Some(600),
            ..Policy::default()
        },
    )
    .await
    .expect("policy");
    let u = meta.usage(scope).await.expect("usage");
    assert_eq!(
        (u.used_bytes, u.messages, u.limit_bytes),
        (500, 2, Some(600))
    );
    meta.check(scope, 100).await.expect("в пределах квоты");
    assert_eq!(meta.check(scope, 101).await, Err(Error::QuotaExceeded));

    let missing = AccountScope {
        tenant: scope.tenant,
        account: tp_contracts::AccountId::new(),
    };
    assert_eq!(meta.usage(missing).await, Err(Error::NotFound));
}

#[tokio::test]
async fn quota_default_from_config() {
    let Some(pool) = common::pool().await else {
        return;
    };
    let mut cfg = tp_meta::MetaConfig::default();
    cfg.retention.quota_bytes = Some(10);
    let meta = PgMeta::new(pool, cfg);
    let scope = common::account(&meta).await;
    assert_eq!(
        meta.usage(scope).await.expect("usage").limit_bytes,
        Some(10)
    );
    assert_eq!(meta.check(scope, 11).await, Err(Error::QuotaExceeded));
}

#[tokio::test]
async fn trash_expiry_restore_and_defer() {
    let Some(db) = common::fresh_meta().await else {
        return;
    };
    let meta = &db.meta;
    let scope = common::account(meta).await;
    let inbox = folder(meta, scope, SpecialUse::Inbox).await;
    let work = meta
        .create(
            scope,
            NewMailbox {
                parent: None,
                name: "Work".into(),
                role: None,
            },
        )
        .await
        .expect("create")
        .id;
    // Срок корзины для писем из Work — сутки (папка), для прочих — по умолчанию (30 суток).
    meta.set_policy(
        scope.tenant,
        PolicyTarget::Mailbox(work),
        &Policy {
            trash_retention: Some(DAY),
            ..Policy::default()
        },
    )
    .await
    .expect("policy");
    let a = meta
        .insert(scope, work, message(scope, 10))
        .await
        .expect("a");
    let b = meta
        .insert(scope, inbox, message(scope, 20))
        .await
        .expect("b");
    let c = meta
        .insert(scope, inbox, message(scope, 30))
        .await
        .expect("c");
    meta.to_trash(scope, &[a.id, b.id]).await.expect("trash");

    let items = TrashStore::list(meta, scope, 0, 10).await.expect("list");
    assert_eq!(items.len(), 2);
    let item = |id| {
        items
            .iter()
            .find(|i| i.object == TrashObject::Message(id))
            .cloned()
            .expect("item")
    };
    let ia = item(a.id);
    let ib = item(b.id);
    assert_eq!((ia.origin, ia.size), (work, 10));
    assert_eq!(ib.origin, inbox);
    assert_eq!(ia.expires_at - ia.deleted_at, time::Duration::days(1));
    assert_eq!(ib.expires_at - ib.deleted_at, time::Duration::days(30));
    assert_eq!(ia.received_at, message(scope, 0).received_at);
    // Постранично: новые первыми.
    assert_eq!(
        TrashStore::list(meta, scope, 1, 10)
            .await
            .expect("page")
            .len(),
        1
    );

    // Политика изменилась после удаления — срок удалённых не меняется, новых — меняется.
    meta.set_policy(
        scope.tenant,
        PolicyTarget::Account(scope.account),
        &Policy {
            trash_retention: Some(DAY * 7),
            ..Policy::default()
        },
    )
    .await
    .expect("policy");
    meta.to_trash(scope, &[c.id]).await.expect("trash c");
    let items = TrashStore::list(meta, scope, 0, 10).await.expect("list");
    let ic = items
        .iter()
        .find(|i| i.object == TrashObject::Message(c.id))
        .expect("c");
    assert_eq!(ic.expires_at - ic.deleted_at, time::Duration::days(7));
    assert!(items.contains(&ib));

    // Истёкшие — по всем организациям, по возрастанию срока.
    let now = ia.deleted_at + time::Duration::days(1) + time::Duration::seconds(1);
    let exp = meta.expired(now, 10).await.expect("expired");
    assert_eq!(exp, vec![ia.clone()]);
    let far = ib.expires_at + time::Duration::seconds(1);
    let exp = meta.expired(far, 10).await.expect("expired");
    let ids: Vec<_> = exp.iter().map(|i| i.object).collect();
    assert_eq!(ids, [a.id, c.id, b.id].map(TrashObject::Message).to_vec());
    assert_eq!(meta.expired(far, 1).await.expect("limit").len(), 1);

    // Отказ хука — отложить.
    let until = far + time::Duration::days(1);
    meta.defer(scope, TrashObject::Message(a.id), until)
        .await
        .expect("defer");
    let exp = meta.expired(now, 10).await.expect("expired");
    assert!(exp.is_empty());
    assert_eq!(
        meta.defer(
            scope,
            TrashObject::Message(tp_contracts::MessageId::new()),
            until
        )
        .await,
        Err(Error::NotFound)
    );

    // Восстановление — в исходную папку; повторно — элемента нет.
    let Restored::Message { mailbox, inserted } =
        TrashStore::restore(meta, scope, TrashObject::Message(a.id))
            .await
            .expect("restore")
    else {
        panic!("письмо");
    };
    assert_eq!((mailbox, inserted.id), (work, a.id));
    assert_eq!(
        TrashStore::restore(meta, scope, TrashObject::Message(a.id)).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        meta.defer(scope, TrashObject::Message(a.id), until).await,
        Err(Error::NotFound)
    );
    let left: Vec<_> = TrashStore::list(meta, scope, 0, 10)
        .await
        .expect("list")
        .into_iter()
        .map(|i| i.object)
        .collect();
    assert_eq!(left, [c.id, b.id].map(TrashObject::Message).to_vec());
    let exp = meta.expired(far, 10).await.expect("expired");
    assert!(exp.iter().all(|i| i.object != TrashObject::Message(a.id)));

    // Удалённая папка с письмами: срок по политике исходной папки.
    let d = meta
        .insert(scope, work, message(scope, 5))
        .await
        .expect("d");
    MailboxStore::delete(meta, scope, work, true)
        .await
        .expect("delete");
    let id = TrashStore::list(meta, scope, 0, 10)
        .await
        .expect("list")
        .into_iter()
        .find(|i| i.object == TrashObject::Message(d.id))
        .expect("d в корзине");
    assert_eq!(id.expires_at - id.deleted_at, time::Duration::days(1));

    db.drop_db().await;
}
