//! Общие наборы тестов контрактов: in-memory реализации testkit и настоящие реализации полос
//! (`tp-meta` — `Membership`, `AclStore`, `tp-platform` — `SecretStore`) проходят одни и те же проверки.
//!
//! Время в наборах идёт через [`Wait`]: для testkit — [`ManualClock`] (сдвиг без ожидания), для PostgreSQL
//! и OpenBao — [`RealTime`] (сон), тогда сроки выбирают с запасом на задержки.
//!
//! ```
//! # tokio_test_block_on(async {
//! use std::time::Duration;
//! use tp_testkit::{ManualClock, MemMembership, suite};
//!
//! let clock = ManualClock::default();
//! suite::membership(&MemMembership::new(clock.shared()), Duration::from_secs(30), &clock).await;
//! # });
//! # fn tokio_test_block_on(f: impl std::future::Future<Output = ()>) {
//! #     tokio::runtime::Builder::new_current_thread().enable_all().build().expect("rt").block_on(f)
//! # }
//! ```

use std::time::Duration;

use async_trait::async_trait;
use tp_contracts::acl::{ANYONE, AclEntry, AclStore, SendDelegation, SharedMailbox};
use tp_contracts::auth::{Rights, SendRight};
use tp_contracts::membership::{Membership, NodeId};
use tp_contracts::meta::{AccountScope, MailboxStore, NewMailbox, SpecialUse};
use tp_contracts::secrets::{SecretLease, SecretRef, SecretStore};
use tp_contracts::{AccountId, Error, MailboxId, TenantId};

use crate::ManualClock;

/// Ход времени для наборов тестов.
#[async_trait]
pub trait Wait: Send + Sync {
    /// Дождаться, пока пройдёт `d` по часам проверяемого хранилища.
    async fn wait(&self, d: Duration);
}

#[async_trait]
impl Wait for ManualClock {
    async fn wait(&self, d: Duration) {
        self.advance(d);
    }
}

/// Настоящее время: `tokio::time::sleep`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealTime;

#[async_trait]
impl Wait for RealTime {
    async fn wait(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

// ---------- Membership ----------

/// Набор проверок `Membership`. `ttl` — срок записи узла; с [`RealTime`] — с запасом на задержки
/// хранилища (секунды). Набор не требует пустого состава: узлы набора — с уникальным префиксом,
/// чужие записи в `live()` не мешают.
pub async fn membership(m: &dyn Membership, ttl: Duration, wait: &dyn Wait) {
    let prefix = format!("suite-{}", TenantId::new());
    let node = |n: &str| NodeId::new(format!("{prefix}-{n}")).expect("node id");
    let (a, b, c) = (node("a"), node("b"), node("c"));
    let ours = || async {
        let live = m.live().await.expect("live");
        let mut sorted = live.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(live, sorted, "live(): по возрастанию без повторов");
        live.into_iter()
            .filter(|n| n.as_str().starts_with(&prefix))
            .collect::<Vec<_>>()
    };

    assert!(ours().await.is_empty(), "до heartbeat узлов набора нет");

    // Создание записи и повторное продление без повторов в составе.
    m.heartbeat(&c, ttl).await.expect("heartbeat c");
    m.heartbeat(&a, ttl).await.expect("heartbeat a");
    m.heartbeat(&b, ttl).await.expect("heartbeat b");
    m.heartbeat(&a, ttl).await.expect("heartbeat a again");
    assert_eq!(ours().await, vec![a.clone(), b.clone(), c.clone()]);

    // Выход — сразу, без ожидания срока; повторный выход и выход отсутствующего — не ошибка.
    m.leave(&c).await.expect("leave c");
    m.leave(&c).await.expect("leave c again");
    m.leave(&node("never")).await.expect("leave absent");
    assert_eq!(ours().await, vec![a.clone(), b.clone()]);

    // Продление переносит срок: a продлевается каждые ttl/2, b — нет.
    let half = ttl / 2;
    wait.wait(half).await;
    m.heartbeat(&a, ttl).await.expect("renew a");
    wait.wait(half + ttl / 4).await;
    assert_eq!(ours().await, vec![a.clone()], "b истёк, a продлён");

    // Без продления истекает и a; продление после истечения возвращает узел.
    wait.wait(ttl).await;
    assert!(ours().await.is_empty(), "все записи набора истекли");
    m.heartbeat(&b, ttl)
        .await
        .expect("heartbeat b after expiry");
    assert_eq!(ours().await, vec![b.clone()]);
    m.leave(&b).await.expect("leave b");
    assert!(ours().await.is_empty());
}

// ---------- SecretStore ----------

/// Данные хранилища для [`secret_store`]; хранилище заполняет тест до запуска набора.
#[derive(Debug, Clone)]
pub struct SecretFixture {
    /// Статический секрет и его значение.
    pub present: (SecretRef, String),
    /// Поле, которого нет у существующего пути.
    pub missing_field: SecretRef,
    /// Несуществующий путь.
    pub missing_path: SecretRef,
    /// Ссылка, запрещённая политикой, если хранилище умеет политики.
    pub forbidden: Option<SecretRef>,
    /// Динамический секрет с продлеваемой арендой (срок — короткий для [`RealTime`]), если есть.
    pub dynamic: Option<SecretRef>,
}

/// Набор проверок `SecretStore` на подготовленных данных `fixture`.
pub async fn secret_store(store: &dyn SecretStore, fixture: &SecretFixture, wait: &dyn Wait) {
    let (present, value) = &fixture.present;
    let s = store.get(present).await.expect("present secret");
    assert_eq!(s.expose(), value);
    assert!(s.lease().is_none(), "статический секрет — без аренды");
    let shown = format!("{s:?} {s}");
    assert!(!shown.contains(value.as_str()), "значение скрыто: {shown}");

    for missing in [&fixture.missing_field, &fixture.missing_path] {
        assert_eq!(
            store.get(missing).await.map(|_| ()),
            Err(Error::NotFound),
            "{missing}"
        );
    }
    if let Some(forbidden) = &fixture.forbidden {
        assert_eq!(
            store.get(forbidden).await.map(|_| ()),
            Err(Error::Forbidden),
            "{forbidden}"
        );
    }

    let unknown = SecretLease {
        id: format!("suite/unknown/{}", TenantId::new()),
        ttl: Duration::from_secs(60),
        renewable: true,
    };
    assert_eq!(store.renew(&unknown).await, Err(Error::NotFound));

    let Some(dynamic) = &fixture.dynamic else {
        return;
    };
    let s = store.get(dynamic).await.expect("dynamic secret");
    let lease = s.lease().cloned().expect("динамический секрет — с арендой");
    assert!(lease.renewable && !lease.ttl.is_zero(), "{lease:?}");
    assert_eq!(s.lease_ttl(), Some(lease.ttl));

    // Продление до истечения переносит срок от текущего момента.
    wait.wait(lease.ttl / 2).await;
    let renewed = store.renew(&lease).await.expect("renew");
    assert_eq!(renewed.id, lease.id);
    assert!(renewed.renewable && !renewed.ttl.is_zero(), "{renewed:?}");
    wait.wait(renewed.ttl * 3 / 4).await;
    let renewed = store
        .renew(&renewed)
        .await
        .expect("renew after first renew");

    // Истёкшая аренда — `NotFound`, секрет читается заново с новой арендой.
    wait.wait(renewed.ttl + renewed.ttl / 4).await;
    assert_eq!(store.renew(&renewed).await, Err(Error::NotFound));
    let again = store.get(dynamic).await.expect("dynamic again");
    assert!(again.lease().is_some_and(|l| l.id != lease.id));
}

// ---------- AclStore ----------

/// Учётные записи для [`acl_store`]; создаёт тест до запуска набора (у каждой — системные папки).
#[derive(Debug, Clone, Copy)]
pub struct AclFixture {
    /// Владелец папок.
    pub owner: AccountScope,
    /// Другая учётная запись той же организации.
    pub peer: AccountScope,
    /// Учётная запись другой организации.
    pub foreign: AccountScope,
}

/// Набор проверок `AclStore`; папки создаёт и удаляет через `mailboxes`. Идентификаторы — с уникальным
/// суффиксом: записи других тестов той же организации не мешают.
pub async fn acl_store(acl: &dyn AclStore, mailboxes: &dyn MailboxStore, fx: &AclFixture) {
    let sfx = TenantId::new().to_string();
    let id = |n: &str| format!("{n}-{sfx}");
    let ids = |list: &[&str]| list.iter().map(|n| id(n)).collect::<Vec<_>>();
    let r = |s: &str| s.parse::<Rights>().expect("rights");
    let (fred, sales, assistants) = (id("fred"), id("sales"), id("assistants"));
    let not_fred = format!("-{fred}");
    let owner = fx.owner;
    let folder = |scope: AccountScope, name: &str| {
        let name = format!("{name}-{sfx}");
        async move {
            mailboxes
                .create(
                    scope,
                    NewMailbox {
                        parent: None,
                        name,
                        role: None,
                    },
                )
                .await
                .expect("create mailbox")
                .id
        }
    };
    let inbox = mailboxes
        .list(owner)
        .await
        .expect("list")
        .into_iter()
        .find(|m| m.role == Some(SpecialUse::Inbox))
        .expect("inbox")
        .id;
    let shared = folder(owner, "Shared").await;
    let peer_box = folder(fx.peer, "Peer").await;

    // Чтение и запись ACL; замена прав целиком; порядок — по идентификатору.
    assert!(acl.acl(owner, inbox).await.expect("acl").is_empty());
    for (who, rights) in [
        (fred.as_str(), "lrs"),
        (sales.as_str(), "lrswi"),
        (ANYONE, "l"),
        (not_fred.as_str(), "w"),
        (fred.as_str(), "lrsp"),
    ] {
        acl.set_acl(owner, inbox, who, r(rights))
            .await
            .expect("set_acl");
    }
    let mut expected = vec![
        AclEntry {
            identifier: not_fred.clone(),
            rights: r("w"),
        },
        AclEntry {
            identifier: ANYONE.to_owned(),
            rights: r("l"),
        },
        AclEntry {
            identifier: fred.clone(),
            rights: r("lrsp"),
        },
        AclEntry {
            identifier: sales.clone(),
            rights: r("lrswi"),
        },
    ];
    expected.sort_by(|a, b| a.identifier.cmp(&b.identifier));
    assert_eq!(acl.acl(owner, inbox).await.expect("acl"), expected);

    // Действующие права: объединение своих, групп и anyone минус отрицательные.
    let rights = |mailbox: MailboxId, who: Vec<String>| async move {
        acl.acl_rights(owner, mailbox, &who).await.expect("rights")
    };
    assert_eq!(rights(inbox, ids(&["fred", "sales"])).await, r("lrsip"));
    assert_eq!(rights(inbox, ids(&["fred"])).await, r("lrsp"));
    assert_eq!(rights(inbox, ids(&["bob"])).await, r("l"), "anyone");
    assert_eq!(rights(inbox, Vec::new()).await, r("l"), "только anyone");
    assert_eq!(rights(shared, ids(&["fred"])).await, Rights::NONE);
    assert_eq!(rights(MailboxId::new(), ids(&["fred"])).await, Rights::NONE);
    assert!(matches!(
        acl.acl_rights(owner, inbox, std::slice::from_ref(&not_fred))
            .await,
        Err(Error::Invalid(_))
    ));

    // Общие папки: только с правом l, по возрастанию (учётная запись, папка).
    let shared_of = |who: Vec<String>| async move {
        acl.shared_mailboxes(owner.tenant, &who)
            .await
            .expect("shared")
    };
    acl.set_acl(owner, shared, &fred, r("r"))
        .await
        .expect("без l");
    assert_eq!(
        shared_of(ids(&["fred"])).await,
        vec![SharedMailbox {
            account: owner.account,
            mailbox: inbox,
            rights: r("lrsp"),
        }]
    );
    acl.set_acl(owner, shared, &fred, r("lr"))
        .await
        .expect("с l");
    acl.set_acl(fx.peer, peer_box, &sales, r("lre"))
        .await
        .expect("peer");
    let mut expected = vec![
        SharedMailbox {
            account: owner.account,
            mailbox: inbox,
            rights: r("lrsip"),
        },
        SharedMailbox {
            account: owner.account,
            mailbox: shared,
            rights: r("lr"),
        },
        SharedMailbox {
            account: fx.peer.account,
            mailbox: peer_box,
            rights: r("lre"),
        },
    ];
    expected.sort_by_key(|s| (s.account, s.mailbox));
    assert_eq!(shared_of(ids(&["fred", "sales"])).await, expected);
    assert!(matches!(
        acl.shared_mailboxes(owner.tenant, std::slice::from_ref(&not_fred))
            .await,
        Err(Error::Invalid(_))
    ));

    // Пустые права удаляют запись (повторно — не ошибка); удаление папки удаляет её ACL.
    for _ in 0..2 {
        acl.set_acl(owner, inbox, &not_fred, Rights::NONE)
            .await
            .expect("delete entry");
    }
    assert_eq!(rights(inbox, ids(&["fred"])).await, r("lrsp"));
    mailboxes
        .delete(owner, shared, false)
        .await
        .expect("delete mailbox");
    assert_eq!(acl.acl(owner, shared).await, Err(Error::NotFound));
    assert_eq!(rights(shared, ids(&["fred"])).await, Rights::NONE);
    let left: Vec<MailboxId> = shared_of(ids(&["fred"]))
        .await
        .iter()
        .map(|s| s.mailbox)
        .collect();
    assert_eq!(left, vec![inbox]);

    // Ошибки: нет папки (и папка другой учётной записи); недопустимый идентификатор проверяется первым.
    for mailbox in [MailboxId::new(), peer_box] {
        assert_eq!(acl.acl(owner, mailbox).await, Err(Error::NotFound));
        assert_eq!(
            acl.set_acl(owner, mailbox, &fred, r("l")).await,
            Err(Error::NotFound)
        );
    }
    let long = "x".repeat(256);
    for bad in ["", "-", "a b", "a\tb", long.as_str()] {
        for mailbox in [inbox, MailboxId::new()] {
            assert!(
                matches!(
                    acl.set_acl(owner, mailbox, bad, r("l")).await,
                    Err(Error::Invalid(_))
                ),
                "{bad:?}"
            );
        }
    }

    // Другая организация не видит ACL.
    let alien = AccountScope {
        tenant: fx.foreign.tenant,
        account: owner.account,
    };
    assert_eq!(acl.acl(alien, inbox).await, Err(Error::NotFound));
    assert_eq!(
        acl.set_acl(alien, inbox, &fred, r("l")).await,
        Err(Error::NotFound)
    );
    assert!(
        acl.shared_mailboxes(fx.foreign.tenant, &ids(&["fred", "sales"]))
            .await
            .expect("foreign")
            .is_empty()
    );

    // ---- Делегирование отправки ----
    let right_of = |who: Vec<String>| async move {
        acl.send_right_of(owner, &who).await.expect("send_right_of")
    };
    assert!(acl.send_delegations(owner).await.expect("list").is_empty());
    assert_eq!(right_of(ids(&["fred"])).await, None);
    acl.set_send_delegation(owner, &fred, Some(SendRight::SendOnBehalf))
        .await
        .expect("fred");
    acl.set_send_delegation(owner, &assistants, Some(SendRight::SendAs))
        .await
        .expect("group");
    assert_eq!(
        right_of(ids(&["fred"])).await,
        Some(SendRight::SendOnBehalf)
    );
    // Send As через группу сильнее Send on Behalf; anyone не добавляется.
    assert_eq!(
        right_of(ids(&["fred", "assistants"])).await,
        Some(SendRight::SendAs)
    );
    assert_eq!(right_of(Vec::new()).await, None);
    let mut expected = vec![
        SendDelegation {
            delegate: assistants.clone(),
            right: SendRight::SendAs,
        },
        SendDelegation {
            delegate: fred.clone(),
            right: SendRight::SendOnBehalf,
        },
    ];
    expected.sort_by(|a, b| a.delegate.cmp(&b.delegate));
    assert_eq!(acl.send_delegations(owner).await.expect("list"), expected);
    assert!(
        acl.send_delegations(fx.peer)
            .await
            .expect("peer")
            .is_empty()
    );

    // Замена и отзыв (повторный отзыв — не ошибка).
    acl.set_send_delegation(owner, &fred, Some(SendRight::SendAs))
        .await
        .expect("upgrade");
    assert_eq!(right_of(ids(&["fred"])).await, Some(SendRight::SendAs));
    for _ in 0..2 {
        acl.set_send_delegation(owner, &fred, None)
            .await
            .expect("revoke");
    }
    assert_eq!(right_of(ids(&["fred"])).await, None);
    assert_eq!(acl.send_delegations(owner).await.expect("list").len(), 1);

    // Ошибки: нет учётной записи, недопустимый идентификатор; другая организация не видит делегирований.
    let missing = AccountScope {
        tenant: owner.tenant,
        account: AccountId::new(),
    };
    assert_eq!(
        acl.set_send_delegation(missing, &fred, Some(SendRight::SendAs))
            .await,
        Err(Error::NotFound)
    );
    for bad in ["", "-fred", "a b"] {
        assert!(matches!(
            acl.set_send_delegation(owner, bad, Some(SendRight::SendAs))
                .await,
            Err(Error::Invalid(_))
        ));
    }
    assert!(matches!(
        acl.send_right_of(owner, std::slice::from_ref(&not_fred))
            .await,
        Err(Error::Invalid(_))
    ));
    assert!(
        acl.send_delegations(alien)
            .await
            .expect("foreign")
            .is_empty()
    );
    assert_eq!(
        acl.set_send_delegation(alien, &fred, Some(SendRight::SendAs))
            .await,
        Err(Error::NotFound)
    );
    assert_eq!(acl.send_right_of(alien, &[assistants]).await, Ok(None));
}
