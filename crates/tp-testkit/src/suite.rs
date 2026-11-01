//! Общие наборы тестов контрактов: in-memory реализации testkit и настоящие реализации полос
//! (`tp-meta` — `Membership`, `AclStore`, `AppPasswordStore`, `DeliveryLog`, `GroupStore`, сжатие `BlobIndex`,
//! `tp-platform` — `SecretStore`) проходят одни и те же проверки.
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
use time::OffsetDateTime;
use tp_contracts::acl::{ANYONE, AclEntry, AclStore, SendDelegation, SharedMailbox};
use tp_contracts::app_password::{AppPassword, AppPasswordStore};
use tp_contracts::auth::{Rights, SendRight};
use tp_contracts::blob::{Acquire, BlobIndex, Commit, Compression};
use tp_contracts::delivery::{
    DeliveryDecision, DeliveryEvent, DeliveryEventKind, DeliveryLog, DeliveryPage, DeliveryQuery,
};
use tp_contracts::groups::{
    DistributionGroup, GroupMember, GroupStore, HeldMessage, SenderRestriction,
};
use tp_contracts::membership::{Membership, NodeId};
use tp_contracts::meta::{AccountScope, MailboxStore, NewMailbox, SpecialUse};
use tp_contracts::secrets::{SecretLease, SecretRef, SecretStore};
use tp_contracts::{
    AccountId, BlobHash, BlobKey, Error, Gen, GroupId, HeldId, MailboxId, QueueId, TenantId,
};

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

// ---------- AppPasswordStore ----------

/// Учётные записи для [`app_password_store`]; создаёт тест до запуска набора.
#[derive(Debug, Clone, Copy)]
pub struct AppPasswordFixture {
    /// Владелец паролей (без паролей до набора).
    pub owner: AccountScope,
    /// Другая учётная запись той же организации (без паролей до набора).
    pub peer: AccountScope,
    /// Другая организация.
    pub foreign_tenant: TenantId,
}

/// Набор проверок `AppPasswordStore`. Моменты — целые секунды (точность хранилища — не грубее микросекунды).
pub async fn app_password_store(store: &dyn AppPasswordStore, fx: &AppPasswordFixture) {
    let at = |s: i64| OffsetDateTime::from_unix_timestamp(1_800_000_000 + s).expect("time");
    let pw = |id: &str, created: i64| AppPassword {
        id: id.to_owned(),
        label: format!("Телефон {id}"),
        hash: format!("$argon2id$v=19$m=19456,t=2,p=1$c2FsdA${id}"),
        created_at: at(created),
        revoked_at: None,
    };
    let owner = fx.owner;
    let list = |scope: AccountScope| async move { store.list(scope).await.expect("list") };
    assert!(list(owner).await.is_empty());

    // Порядок — по created_at, затем по id; отозванный при добавлении сохраняется как есть.
    let (b, z, a) = (pw("b", 10), pw("z", 0), pw("a", 10));
    let old = AppPassword {
        revoked_at: Some(at(5)),
        ..pw("old", -10)
    };
    for p in [&b, &z, &a, &old] {
        store.insert(owner, p.clone()).await.expect("insert");
    }
    assert_eq!(
        list(owner).await,
        vec![old.clone(), z.clone(), a.clone(), b.clone()]
    );

    // Идентификатор занят — Conflict; в другой учётной записи тот же идентификатор свободен.
    assert_eq!(store.insert(owner, pw("a", 20)).await, Err(Error::Conflict));
    assert!(list(fx.peer).await.is_empty());
    store.insert(fx.peer, pw("a", 20)).await.expect("peer");
    assert_eq!(list(fx.peer).await, vec![pw("a", 20)]);

    // Недопустимые поля — Invalid (раньше NotFound); нет учётной записи — NotFound.
    let missing = AccountScope {
        tenant: owner.tenant,
        account: AccountId::new(),
    };
    let bad = [
        AppPassword {
            id: String::new(),
            ..pw("x", 0)
        },
        AppPassword {
            id: "a b".into(),
            ..pw("x", 0)
        },
        AppPassword {
            hash: String::new(),
            ..pw("x", 0)
        },
        AppPassword {
            label: "a\nb".into(),
            ..pw("x", 0)
        },
    ];
    for p in bad {
        for scope in [owner, missing] {
            assert!(
                matches!(store.insert(scope, p.clone()).await, Err(Error::Invalid(_))),
                "{p:?}"
            );
        }
    }
    assert_eq!(
        store.insert(missing, pw("x", 0)).await,
        Err(Error::NotFound)
    );
    assert!(list(missing).await.is_empty());

    // Отзыв: момент первого отзыва сохраняется; нет пароля — NotFound.
    store.revoke(owner, "a", at(30)).await.expect("revoke");
    store
        .revoke(owner, "a", at(40))
        .await
        .expect("revoke again");
    store
        .revoke(owner, "old", at(50))
        .await
        .expect("revoke revoked");
    let revoked = AppPassword {
        revoked_at: Some(at(30)),
        ..a.clone()
    };
    assert_eq!(list(owner).await, vec![old, z, revoked, b]);
    assert_eq!(
        list(fx.peer).await,
        vec![pw("a", 20)],
        "отзыв не задел peer"
    );
    assert_eq!(
        store.revoke(owner, "nope", at(30)).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        store.revoke(fx.peer, "b", at(30)).await,
        Err(Error::NotFound)
    );

    // Другая организация не видит пароли.
    let alien = AccountScope {
        tenant: fx.foreign_tenant,
        account: owner.account,
    };
    assert!(list(alien).await.is_empty());
    assert_eq!(store.revoke(alien, "b", at(30)).await, Err(Error::NotFound));
    assert_eq!(store.insert(alien, pw("c", 0)).await, Err(Error::NotFound));
    assert_eq!(list(owner).await.len(), 4);
}

// ---------- BlobIndex: сжатие ----------

/// Набор проверок признака сжатия `BlobIndex` (`supports_compression`). Хэши набора — случайные:
/// чужие записи учёта не мешают.
pub async fn blob_compression(index: &dyn BlobIndex, tenant: TenantId) {
    let fresh = || {
        let mut h = [0u8; 32];
        h[..16].copy_from_slice(AccountId::new().as_uuid().as_bytes());
        h[16..].copy_from_slice(AccountId::new().as_uuid().as_bytes());
        BlobHash(h)
    };
    let absent =
        async |hash: &BlobHash| match index.acquire(tenant, hash, 1).await.expect("acquire") {
            Acquire::Absent { generation } => generation,
            other => panic!("ожидался Absent: {other:?}"),
        };
    let done = |c: Commit| match c {
        Commit::Done { generation } => generation,
        other => panic!("ожидался Done: {other:?}"),
    };
    let key = |hash: BlobHash, generation| BlobKey {
        tenant,
        hash,
        generation,
    };
    assert!(index.supports_compression());

    // Сжатый объект: признак в записи, повторная ссылка его не меняет.
    let zstd = fresh();
    let g = absent(&zstd).await;
    let g = done(
        index
            .commit_new_with(tenant, &zstd, g, 100, 1, Compression::Zstd)
            .await
            .expect("commit"),
    );
    assert_eq!(
        index.compression(&key(zstd, g)).await,
        Ok(Some(Compression::Zstd))
    );
    assert!(matches!(
        index.acquire(tenant, &zstd, 1).await,
        Ok(Acquire::Existing { generation }) if generation == g
    ));
    assert_eq!(
        index.compression(&key(zstd, g)).await,
        Ok(Some(Compression::Zstd))
    );

    // Параллельная загрузка без сжатия получает поколение существующей записи и её признак.
    let other = done(
        index
            .commit_new_with(tenant, &zstd, g, 100, 1, Compression::None)
            .await
            .expect("commit"),
    );
    assert_eq!(other, g);
    assert_eq!(
        index.compression(&key(zstd, g)).await,
        Ok(Some(Compression::Zstd))
    );

    // commit_new — без сжатия; нет записи или чужое поколение — None.
    let plain = fresh();
    let pg = absent(&plain).await;
    let pg = done(
        index
            .commit_new(tenant, &plain, pg, 10, 1)
            .await
            .expect("commit"),
    );
    assert_eq!(
        index.compression(&key(plain, pg)).await,
        Ok(Some(Compression::None))
    );
    assert_eq!(index.compression(&key(fresh(), g)).await, Ok(None));
    assert_eq!(index.compression(&key(zstd, g.next())).await, Ok(None));
}

// ---------- DeliveryLog ----------

/// Организации для [`delivery_log`].
#[derive(Debug, Clone, Copy)]
pub struct DeliveryFixture {
    /// Организация набора.
    pub tenant: TenantId,
    /// Другая организация.
    pub foreign_tenant: TenantId,
}

/// Набор проверок `DeliveryLog`: запись, поиск по отправителю, получателю, Message-ID и интервалу, страницы,
/// изоляция организаций. Адреса набора — с уникальной меткой: чужие записи не мешают. Моменты — текущие,
/// целые секунды; `purge` набор не проверяет (удаление по секциям — тест реализации).
pub async fn delivery_log(log: &dyn DeliveryLog, fx: &DeliveryFixture) {
    let tag = AccountId::new().to_string();
    let addr = |who: &str| format!("{who}-{tag}@example.org");
    let now = OffsetDateTime::now_utc()
        .replace_nanosecond(0)
        .expect("time");
    let at = |s: i64| now - time::Duration::seconds(100 - s);
    let node = NodeId::new("suite-node").expect("node");
    let event = |s: i64, kind, from: &str, to: &[&str], msg: &str| DeliveryEvent {
        tenant: fx.tenant,
        at: at(s),
        node: node.clone(),
        kind,
        sender: addr(from),
        recipients: to.iter().map(|r| addr(r)).collect(),
        message_id: Some(format!("{msg}.{tag}@example.org")),
        queue_id: None,
        decision: DeliveryDecision::Accept,
        reason: None,
    };
    use DeliveryEventKind as K;
    let received = event(0, K::Receive, "alice", &["bob", "carol"], "m1");
    let delivered = event(1, K::Deliver, "alice", &["bob", "carol"], "m1");
    let rejected = DeliveryEvent {
        decision: DeliveryDecision::Reject,
        reason: Some("550 5.7.1 spam".into()),
        message_id: None,
        ..event(2, K::Reject, "mallory", &["bob"], "m2")
    };
    let relayed = DeliveryEvent {
        queue_id: Some(QueueId::new()),
        ..event(3, K::Relay, "bob", &["dave"], "m3")
    };
    let dsn = DeliveryEvent {
        sender: String::new(),
        ..event(4, K::Dsn, "x", &["bob"], "m3")
    };
    // Записи в другом порядке: поиск упорядочивает по моменту.
    log.record(relayed.clone()).await.expect("record");
    log.record_batch(vec![received.clone(), delivered.clone(), rejected.clone()])
        .await
        .expect("batch");
    log.record(dsn.clone()).await.expect("record");
    log.record(DeliveryEvent {
        tenant: fx.foreign_tenant,
        ..received.clone()
    })
    .await
    .expect("foreign");

    let search = async |q: DeliveryQuery| log.search(fx.tenant, &q).await.expect("search");
    let mut by = DeliveryQuery::all(100);
    by.recipient = Some(addr("BOB"));
    assert_eq!(
        search(by.clone()).await,
        DeliveryPage {
            events: vec![
                received.clone(),
                delivered.clone(),
                rejected.clone(),
                dsn.clone()
            ],
            next: None,
        },
        "получатель без учёта регистра, по моменту"
    );

    let mut q = DeliveryQuery::all(100);
    q.sender = Some(addr("alice").to_uppercase());
    assert_eq!(
        search(q).await.events,
        vec![received.clone(), delivered.clone()]
    );
    let mut q = DeliveryQuery::all(100);
    q.message_id = received.message_id.clone();
    assert_eq!(
        search(q).await.events,
        vec![received.clone(), delivered.clone()]
    );
    let mut q = DeliveryQuery::all(100);
    q.message_id = relayed.message_id.clone();
    q.sender = Some(addr("bob"));
    assert_eq!(search(q).await.events, vec![relayed.clone()]);

    // Интервал [since, until).
    let mut q = by.clone();
    q.since = Some(at(1));
    q.until = Some(at(4));
    assert_eq!(
        search(q).await.events,
        vec![delivered.clone(), rejected.clone()]
    );

    // Страницы по limit.
    let mut q = by.clone();
    q.limit = 3;
    let first = search(q.clone()).await;
    assert_eq!(
        first.events,
        vec![received.clone(), delivered.clone(), rejected.clone()]
    );
    q.after = Some(first.next.expect("next page"));
    assert_eq!(
        search(q).await,
        DeliveryPage {
            events: vec![dsn.clone()],
            next: None,
        }
    );

    // Некорректный запрос — Invalid; другая организация видит только своё.
    let mut q = by.clone();
    q.limit = 0;
    assert!(matches!(
        log.search(fx.tenant, &q).await,
        Err(Error::Invalid(_))
    ));
    let mut q = DeliveryQuery::all(100);
    q.message_id = received.message_id.clone();
    assert_eq!(
        log.search(fx.foreign_tenant, &q)
            .await
            .expect("foreign")
            .events
            .len(),
        1
    );
}

// ---------- GroupStore ----------

/// Учётные записи для [`group_store`]; создаёт тест до запуска набора.
#[derive(Debug, Clone, Copy)]
pub struct GroupFixture {
    /// Организация набора.
    pub tenant: TenantId,
    /// Учётные записи организации.
    pub accounts: [AccountId; 4],
    /// Другая организация.
    pub foreign_tenant: TenantId,
}

/// Набор проверок `GroupStore`: группы, вложенность и циклы, ограничения отправителей, очередь модерации,
/// изоляция организаций. Адреса и `external_id` набора — с уникальной меткой.
pub async fn group_store(store: &dyn GroupStore, fx: &GroupFixture) {
    let tag = AccountId::new().to_string();
    let t = fx.tenant;
    let [a, b, c, d] = fx.accounts;
    let group = |name: &str, members: Vec<GroupMember>| DistributionGroup {
        id: GroupId::new(),
        tenant: t,
        name: format!("{name} {tag}"),
        address: Some(format!("{name}-{tag}@example.org")),
        external_id: Some(format!("{name}-{tag}")),
        members,
        senders: SenderRestriction::Anyone,
        moderators: Vec::new(),
        hidden_from_gal: false,
    };
    let sorted = |mut v: Vec<AccountId>| {
        v.sort();
        v
    };

    // Вложенность с циклом: all → (a, team), team → (b, inner, all), inner → (c, team); d — вне групп.
    let mut all = group("all", vec![GroupMember::Account(a)]);
    let mut team = group("team", vec![GroupMember::Account(b)]);
    let inner = DistributionGroup {
        address: None,
        ..group(
            "inner",
            vec![GroupMember::Account(c), GroupMember::Group(team.id)],
        )
    };
    all.members.push(GroupMember::Group(team.id));
    team.members.extend([
        GroupMember::Group(inner.id),
        GroupMember::Group(all.id),
        GroupMember::Group(GroupId::new()), // неизвестная группа
        GroupMember::Account(b),            // повтор
    ]);
    for g in [&all, &team, &inner] {
        store.put(g.clone()).await.expect("put");
    }
    assert_eq!(store.expand(t, all.id).await, Ok(sorted(vec![a, b, c])));
    assert_eq!(store.expand(t, inner.id).await, Ok(sorted(vec![a, b, c])));
    assert_eq!(store.expand(t, GroupId::new()).await, Err(Error::NotFound));

    // Поиск: по id, адресу без учёта регистра, external_id; список организации.
    assert_eq!(store.get(t, all.id).await, Ok(Some(all.clone())));
    let upper = all.address.clone().expect("address").to_uppercase();
    assert_eq!(store.by_address(t, &upper).await, Ok(Some(all.clone())));
    assert_eq!(
        store.by_external_id(t, &format!("team-{tag}")).await,
        Ok(Some(team.clone()))
    );
    let mine: Vec<_> = store
        .list(t)
        .await
        .expect("list")
        .into_iter()
        .filter(|g| g.name.ends_with(&tag))
        .collect();
    let mut expected = vec![all.clone(), team.clone(), inner.clone()];
    expected.sort_by_key(|g| g.id);
    assert_eq!(mine, expected);

    // Замена целиком; занятый адрес или external_id — Conflict; некорректное — Invalid.
    let hidden = DistributionGroup {
        hidden_from_gal: true,
        senders: SenderRestriction::Internal,
        ..team.clone()
    };
    store.put(hidden.clone()).await.expect("replace");
    assert_eq!(store.get(t, team.id).await, Ok(Some(hidden.clone())));
    let clash = DistributionGroup {
        address: Some(upper.clone()),
        ..group("clash", Vec::new())
    };
    assert_eq!(store.put(clash).await, Err(Error::Conflict));
    let clash = DistributionGroup {
        external_id: all.external_id.clone(),
        ..group("clash2", Vec::new())
    };
    assert_eq!(store.put(clash).await, Err(Error::Conflict));
    let bad = DistributionGroup {
        name: String::new(),
        ..group("bad", Vec::new())
    };
    assert!(matches!(store.put(bad).await, Err(Error::Invalid(_))));

    // Ограничения отправителей.
    assert_eq!(store.sender_allowed(t, &all, None).await, Ok(true));
    assert_eq!(store.sender_allowed(t, &hidden, None).await, Ok(false));
    assert_eq!(store.sender_allowed(t, &hidden, Some(d)).await, Ok(true));
    let closed = DistributionGroup {
        senders: SenderRestriction::Only {
            accounts: vec![d],
            groups: vec![inner.id],
        },
        moderators: vec![a],
        ..group("closed", Vec::new())
    };
    store.put(closed.clone()).await.expect("put");
    assert_eq!(store.sender_allowed(t, &closed, Some(d)).await, Ok(true));
    assert_eq!(store.sender_allowed(t, &closed, Some(c)).await, Ok(true));
    assert_eq!(store.sender_allowed(t, &closed, None).await, Ok(false));

    // Очередь модерации: по моменту, по группе; take_held — один раз.
    assert!(closed.requires_moderation(Some(d)));
    let base = OffsetDateTime::from_unix_timestamp(1_800_000_000).expect("time");
    let held = |s: i64, group: GroupId| HeldMessage {
        id: HeldId::new(),
        group,
        sender: format!("d-{tag}@example.org"),
        sender_account: Some(d),
        message_id: Some(format!("{s}.{tag}@example.org")),
        blob: BlobKey {
            tenant: t,
            hash: BlobHash([7; 32]),
            generation: Gen::INITIAL,
        },
        size: 1000,
        held_at: base + time::Duration::seconds(s),
    };
    let (h2, h1, h3) = (held(2, closed.id), held(1, closed.id), held(3, all.id));
    for h in [&h2, &h1, &h3] {
        store.hold(t, h.clone()).await.expect("hold");
    }
    assert_eq!(store.hold(t, h1.clone()).await, Err(Error::Conflict));
    assert_eq!(
        store.hold(t, held(4, GroupId::new())).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        store.held(t, Some(closed.id), 10).await,
        Ok(vec![h1.clone(), h2.clone()])
    );
    assert_eq!(
        store.held(t, Some(closed.id), 1).await,
        Ok(vec![h1.clone()])
    );
    assert_eq!(store.take_held(t, h1.id).await, Ok(h1.clone()));
    assert_eq!(store.take_held(t, h1.id).await, Err(Error::NotFound));

    // Другая организация не видит группы и очередь.
    let f = fx.foreign_tenant;
    assert_eq!(store.get(f, all.id).await, Ok(None));
    assert_eq!(store.by_address(f, &upper).await, Ok(None));
    assert_eq!(store.expand(f, all.id).await, Err(Error::NotFound));
    assert_eq!(store.held(f, None, 10).await, Ok(Vec::new()));
    assert_eq!(store.take_held(f, h2.id).await, Err(Error::NotFound));
    assert_eq!(store.remove(f, closed.id).await, Ok(Vec::new()));
    assert_eq!(store.get(t, closed.id).await, Ok(Some(closed.clone())));

    // Удаление возвращает очередь группы; вложенная ссылка становится неизвестной; повтор — не ошибка.
    assert_eq!(store.remove(t, closed.id).await, Ok(vec![h2]));
    assert_eq!(store.remove(t, closed.id).await, Ok(Vec::new()));
    assert_eq!(store.get(t, closed.id).await, Ok(None));
    store.remove(t, inner.id).await.expect("remove");
    assert_eq!(store.expand(t, all.id).await, Ok(sorted(vec![a, b])));
    assert_eq!(store.held(t, Some(all.id), 10).await, Ok(vec![h3]));
}
