//! META-03: AccountStore и MailboxStore на PostgreSQL.

mod common;

use tp_contracts::meta::{AccountStore, AccountUpsert, MailboxStore, NewMailbox, SpecialUse};
use tp_contracts::{AccountId, Error, MailboxId, ModSeq, Uid};

#[tokio::test]
async fn upsert_creates_system_folders_and_matches() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let boxes: &dyn MailboxStore = &meta;
    let tenant = common::tenant(meta.pool()).await;
    let (domain, dname) = common::domain(meta.pool(), tenant).await;
    let login = format!("Ivan@{dname}");
    let data = AccountUpsert {
        domain,
        login: login.clone(),
        display_name: "Иван".into(),
        external_id: Some("guid-1".into()),
        enabled: true,
    };
    let acc = meta.upsert(tenant, data.clone()).await.expect("upsert");
    assert_eq!(acc.tenant, tenant);
    assert_eq!(acc.login, login);

    let scope = tp_contracts::meta::AccountScope {
        tenant,
        account: acc.id,
    };
    let folders = boxes.list(scope).await.expect("list");
    let mut roles: Vec<_> = folders.iter().filter_map(|m| m.role).collect();
    roles.sort();
    assert_eq!(
        roles,
        vec![
            SpecialUse::Inbox,
            SpecialUse::Drafts,
            SpecialUse::Junk,
            SpecialUse::Sent,
            SpecialUse::Trash
        ]
    );
    assert!(folders.iter().any(|m| m.name == "INBOX"));
    let mut validities: Vec<_> = folders.iter().map(|m| m.uid_validity).collect();
    validities.sort();
    validities.dedup();
    assert_eq!(validities.len(), folders.len(), "UIDVALIDITY различны");

    // Повторная синхронизация: по external_id, с новым именем входа; папки не дублируются.
    let renamed = format!("ivan.petrov@{dname}");
    let again = meta
        .upsert(
            tenant,
            AccountUpsert {
                login: renamed.clone(),
                display_name: "Иван Петров".into(),
                ..data.clone()
            },
        )
        .await
        .expect("upsert");
    assert_eq!(again.id, acc.id);
    assert_eq!(again.display_name, "Иван Петров");
    assert_eq!(boxes.list(scope).await.expect("list").len(), folders.len());

    let found = meta
        .by_login(tenant, &renamed.to_uppercase())
        .await
        .expect("by_login");
    assert_eq!(found.map(|a| a.id), Some(acc.id));
    assert_eq!(
        AccountStore::get(&meta, tenant, acc.id)
            .await
            .expect("get")
            .map(|a| a.login),
        Some(renamed.clone())
    );
    assert_eq!(
        AccountStore::get(&meta, tenant, AccountId::new())
            .await
            .expect("get"),
        None
    );

    // Имя занято учётной записью с другим идентификатором каталога.
    let clash = meta
        .upsert(
            tenant,
            AccountUpsert {
                login: renamed,
                external_id: Some("guid-2".into()),
                ..data
            },
        )
        .await;
    assert_eq!(clash, Err(Error::Conflict));

    let quota = meta.quota(tenant, acc.id).await.expect("quota");
    assert_eq!((quota.used_bytes, quota.messages), (0, 0));
    assert_eq!(
        meta.quota(tenant, AccountId::new()).await,
        Err(Error::NotFound)
    );

    // Другая организация не видит учётную запись.
    let other = common::tenant(meta.pool()).await;
    assert_eq!(
        AccountStore::get(&meta, other, acc.id).await.expect("get"),
        None
    );
}

#[tokio::test]
async fn mailbox_crud_and_status() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let meta: &dyn MailboxStore = &meta;
    let folders = meta.list(scope).await.expect("list");
    let inbox = folders
        .iter()
        .find(|m| m.role == Some(SpecialUse::Inbox))
        .expect("inbox")
        .id;
    let trash = folders
        .iter()
        .find(|m| m.role == Some(SpecialUse::Trash))
        .expect("trash")
        .id;

    let new = |parent, name: &str| NewMailbox {
        parent,
        name: name.into(),
        role: None,
    };
    let projects = meta
        .create(scope, new(None, "Projects"))
        .await
        .expect("create");
    let child = meta
        .create(scope, new(Some(projects.id), "Alpha"))
        .await
        .expect("child");
    assert_eq!(
        meta.create(scope, new(None, "Projects")).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        meta.create(scope, new(Some(MailboxId::new()), "X")).await,
        Err(Error::NotFound)
    );
    assert!(matches!(
        meta.create(scope, new(None, "")).await,
        Err(Error::Invalid(_))
    ));
    let role_dup = NewMailbox {
        role: Some(SpecialUse::Trash),
        ..new(None, "Trash2")
    };
    assert_eq!(meta.create(scope, role_dup).await, Err(Error::Conflict));

    // Цикл в иерархии, переименование и перенос.
    assert!(matches!(
        meta.rename(scope, projects.id, Some(child.id), "Projects")
            .await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        meta.rename(scope, projects.id, Some(projects.id), "Projects")
            .await,
        Err(Error::Invalid(_))
    ));
    meta.rename(scope, child.id, None, "Alpha")
        .await
        .expect("rename");
    let moved = meta.get(scope, child.id).await.expect("get").expect("some");
    assert_eq!((moved.parent, moved.name.as_str()), (None, "Alpha"));
    assert_eq!(
        meta.rename(scope, inbox, None, "Other").await,
        Err(Error::Forbidden)
    );

    meta.set_subscribed(scope, child.id, false)
        .await
        .expect("subscribe");
    assert!(
        !meta
            .get(scope, child.id)
            .await
            .expect("get")
            .expect("some")
            .subscribed
    );

    let st = meta.status(scope, child.id).await.expect("status");
    assert_eq!((st.messages, st.unseen, st.size), (0, 0, 0));
    assert_eq!(st.uid_next, Uid(1));
    assert_eq!(st.uid_validity, child.uid_validity);
    assert!(st.highest_modseq > ModSeq::ZERO);
    let before = st.highest_modseq;
    meta.rename(scope, child.id, None, "Alpha2")
        .await
        .expect("rename");
    assert!(
        meta.status(scope, child.id)
            .await
            .expect("status")
            .highest_modseq
            > before
    );

    // Удаление: системные — нельзя; с вложенными — конфликт; пересоздание — новый UIDVALIDITY.
    assert_eq!(
        meta.delete(scope, trash, false).await,
        Err(Error::Forbidden)
    );
    meta.rename(scope, child.id, Some(projects.id), "Alpha")
        .await
        .expect("rename");
    assert_eq!(
        meta.delete(scope, projects.id, false).await,
        Err(Error::Conflict)
    );
    meta.delete(scope, child.id, false).await.expect("delete");
    assert_eq!(meta.get(scope, child.id).await.expect("get"), None);
    let again = meta
        .create(scope, new(Some(projects.id), "Alpha"))
        .await
        .expect("recreate");
    assert!(again.uid_validity > child.uid_validity);
    assert_eq!(
        meta.delete(scope, MailboxId::new(), false).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        meta.status(scope, MailboxId::new()).await,
        Err(Error::NotFound)
    );
}
