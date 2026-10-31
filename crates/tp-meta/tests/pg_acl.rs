//! META-11: ACL папок и делегирование отправки на PostgreSQL.

mod common;

use tp_contracts::auth::{Rights, SendRight};
use tp_contracts::meta::{MailboxStore, NewMailbox, SpecialUse};
use tp_contracts::{AccountId, Error, MailboxId};
use tp_meta::PgMeta;
use tp_meta::acl::{AclEntry, SharedMailbox};

fn r(s: &str) -> Rights {
    s.parse().expect("rights")
}

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

async fn inbox(meta: &PgMeta, scope: tp_contracts::meta::AccountScope) -> MailboxId {
    MailboxStore::list(meta, scope)
        .await
        .expect("list")
        .into_iter()
        .find(|m| m.role == Some(SpecialUse::Inbox))
        .expect("inbox")
        .id
}

#[tokio::test]
async fn acl_crud_and_rights() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let owner = common::account(&meta).await;
    let (t, a) = (owner.tenant, owner.account);
    let inbox = inbox(&meta, owner).await;
    let shared = meta
        .create(
            owner,
            NewMailbox {
                parent: None,
                name: "Shared".into(),
                role: None,
            },
        )
        .await
        .expect("create")
        .id;

    assert!(meta.acl(t, a, inbox).await.expect("acl").is_empty());
    meta.set_acl(t, a, inbox, "fred", r("lrs"))
        .await
        .expect("fred");
    meta.set_acl(t, a, inbox, "sales", r("lrswi"))
        .await
        .expect("group");
    meta.set_acl(t, a, inbox, "anyone", r("l"))
        .await
        .expect("anyone");
    meta.set_acl(t, a, inbox, "-fred", r("w"))
        .await
        .expect("negative");
    // Замена прав целиком.
    meta.set_acl(t, a, inbox, "fred", r("lrsp"))
        .await
        .expect("update");
    assert_eq!(
        meta.acl(t, a, inbox).await.expect("acl"),
        vec![
            AclEntry {
                identifier: "-fred".into(),
                rights: r("w")
            },
            AclEntry {
                identifier: "anyone".into(),
                rights: r("l")
            },
            AclEntry {
                identifier: "fred".into(),
                rights: r("lrsp")
            },
            AclEntry {
                identifier: "sales".into(),
                rights: r("lrswi")
            },
        ]
    );

    // fred в группе sales: объединение, минус отрицательные права fred.
    assert_eq!(
        meta.acl_rights(t, a, inbox, &ids(&["fred", "sales"]))
            .await
            .expect("rights"),
        r("lrsip")
    );
    assert_eq!(
        meta.acl_rights(t, a, inbox, &ids(&["bob"]))
            .await
            .expect("anyone"),
        r("l")
    );
    assert_eq!(
        meta.acl_rights(t, a, shared, &ids(&["fred"]))
            .await
            .expect("none"),
        Rights::NONE
    );
    assert!(matches!(
        meta.acl_rights(t, a, inbox, &ids(&["-fred"])).await,
        Err(Error::Invalid(_))
    ));

    // Общие папки субъекта: только с правом l.
    meta.set_acl(t, a, shared, "fred", r("r"))
        .await
        .expect("no l");
    assert_eq!(
        meta.shared_mailboxes(t, &ids(&["fred"]))
            .await
            .expect("shared"),
        vec![SharedMailbox {
            account: a,
            mailbox: inbox,
            rights: r("lrsp"),
        }]
    );
    meta.set_acl(t, a, shared, "fred", r("lr"))
        .await
        .expect("l");
    let got = meta
        .shared_mailboxes(t, &ids(&["fred", "sales"]))
        .await
        .expect("shared");
    assert_eq!(got.len(), 2);
    assert!(got.contains(&SharedMailbox {
        account: a,
        mailbox: shared,
        rights: r("lr"),
    }));
    assert!(got.contains(&SharedMailbox {
        account: a,
        mailbox: inbox,
        rights: r("lrsip"),
    }));

    // Пустые права удаляют запись; удаление папки удаляет её ACL.
    meta.set_acl(t, a, inbox, "-fred", Rights::NONE)
        .await
        .expect("delete");
    assert_eq!(
        meta.acl_rights(t, a, inbox, &ids(&["fred"]))
            .await
            .expect("rights"),
        r("lrsp")
    );
    MailboxStore::delete(&meta, owner, shared, false)
        .await
        .expect("delete mailbox");
    assert_eq!(meta.acl(t, a, shared).await, Err(Error::NotFound));
    assert_eq!(
        meta.shared_mailboxes(t, &ids(&["fred"]))
            .await
            .expect("shared")
            .len(),
        1
    );

    // Ошибки: нет папки, недопустимый идентификатор.
    let missing = MailboxId::new();
    assert_eq!(
        meta.set_acl(t, a, missing, "fred", r("l")).await,
        Err(Error::NotFound)
    );
    for bad in ["", "a b", "-"] {
        assert!(matches!(
            meta.set_acl(t, a, inbox, bad, r("l")).await,
            Err(Error::Invalid(_))
        ));
    }

    // Другая организация не видит ACL.
    let other = common::account(&meta).await;
    assert_eq!(meta.acl(other.tenant, a, inbox).await, Err(Error::NotFound));
    assert!(
        meta.shared_mailboxes(other.tenant, &ids(&["fred"]))
            .await
            .expect("other")
            .is_empty()
    );
}

#[tokio::test]
async fn send_delegations() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let owner = common::account(&meta).await;
    let (t, a) = (owner.tenant, owner.account);
    assert_eq!(
        meta.send_right_of(t, a, &ids(&["fred"]))
            .await
            .expect("none"),
        None
    );
    meta.set_send_delegation(t, a, "fred", Some(SendRight::SendOnBehalf))
        .await
        .expect("fred");
    meta.set_send_delegation(t, a, "assistants", Some(SendRight::SendAs))
        .await
        .expect("group");
    assert_eq!(
        meta.send_right_of(t, a, &ids(&["fred"]))
            .await
            .expect("fred"),
        Some(SendRight::SendOnBehalf)
    );
    // Send As через группу сильнее Send on Behalf.
    assert_eq!(
        meta.send_right_of(t, a, &ids(&["fred", "assistants"]))
            .await
            .expect("group"),
        Some(SendRight::SendAs)
    );
    let list = meta.send_delegations(t, a).await.expect("list");
    let names: Vec<_> = list.iter().map(|d| d.delegate.as_str()).collect();
    assert_eq!(names, ["assistants", "fred"]);

    // Замена и отзыв.
    meta.set_send_delegation(t, a, "fred", Some(SendRight::SendAs))
        .await
        .expect("upgrade");
    assert_eq!(
        meta.send_right_of(t, a, &ids(&["fred"]))
            .await
            .expect("fred"),
        Some(SendRight::SendAs)
    );
    meta.set_send_delegation(t, a, "fred", None)
        .await
        .expect("revoke");
    assert_eq!(
        meta.send_right_of(t, a, &ids(&["fred"]))
            .await
            .expect("revoked"),
        None
    );
    assert_eq!(meta.send_delegations(t, a).await.expect("list").len(), 1);

    assert_eq!(
        meta.set_send_delegation(t, AccountId::new(), "fred", Some(SendRight::SendAs))
            .await,
        Err(Error::NotFound)
    );
    assert!(matches!(
        meta.set_send_delegation(t, a, "-fred", Some(SendRight::SendAs))
            .await,
        Err(Error::Invalid(_))
    ));
    let other = common::account(&meta).await;
    assert!(
        meta.send_delegations(other.tenant, a)
            .await
            .expect("other")
            .is_empty()
    );
}
