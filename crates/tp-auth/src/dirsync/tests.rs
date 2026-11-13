//! Синхронизация каталога на in-memory каталоге и хранилищах `tp-testkit` (приёмка AUTH-10).

use std::collections::BTreeMap;

use tp_contracts::addressbook::{ContactStore, OrgStore};
use tp_contracts::auth::{DirAccount, DirContact, DirOrg, DirUnit};
use tp_contracts::meta::AccountState;
use tp_contracts::transport::{AcceptedDomain, DomainKind};
use tp_testkit::{
    FakeDirectory, MemAddressBook, MemMetadata, MemSyncGate, MemTransport, SystemClock,
};

use super::*;

const T: TenantId = TenantId::DEFAULT;

struct Env {
    sync: DirectorySync,
    dir: FakeDirectory,
    meta: MemMetadata,
    book: MemAddressBook,
    gate: MemSyncGate,
}

async fn env() -> Env {
    let dir = FakeDirectory::default();
    let meta = MemMetadata::new(Arc::new(SystemClock));
    let book = MemAddressBook::new();
    let gate = MemSyncGate::new();
    let transport = MemTransport::new();
    let domain = AcceptedDomain {
        id: DomainId::new(),
        tenant: T,
        name: "example.org".into(),
        kind: DomainKind::Authoritative,
    };
    transport.put_domain(0, &domain).await.expect("domain");
    let stores = DirStores {
        accounts: Arc::new(meta.clone()),
        contacts: Arc::new(book.clone()),
        org: Arc::new(book.clone()),
        gate: Arc::new(gate.clone()),
        transport: Arc::new(transport),
    };
    let sync = DirectorySync::new(
        Arc::new(dir.clone()),
        T,
        7,
        RemovalPolicy::default(),
        stores,
    )
    .expect("sync");
    Env {
        sync,
        dir,
        meta,
        book,
        gate,
    }
}

fn person(n: usize) -> DirAccount {
    DirAccount {
        external_id: format!("guid-{n:02}"),
        login: format!("user{n:02}@example.org"),
        display_name: format!("User {n}"),
        emails: vec![format!("user{n:02}@example.org")],
        enabled: true,
        attributes: BTreeMap::new(),
        org: DirOrg::default(),
        hidden_from_gal: false,
    }
}

/// 25 учётных записей: порог 5 % — не больше одной к отключению за запуск.
async fn populated() -> (Env, DirCursor) {
    let e = env().await;
    for n in 0..25 {
        e.dir.upsert_account(T, person(n));
    }
    let r = e.sync.accounts(None).await.expect("full");
    assert_eq!((r.upserted, r.disabled, r.skipped), (25, 0, 0));
    let cursor = r.cursor.expect("cursor");
    (e, cursor)
}

async fn state(e: &Env, n: usize) -> AccountState {
    let a = e
        .meta
        .by_external_id(T, &format!("guid-{n:02}"))
        .await
        .expect("by_external_id")
        .expect("account");
    e.meta.account_state(T, a.id).await.expect("state")
}

#[tokio::test]
async fn contact_in_gal() {
    let e = env().await;
    let contact = DirContact {
        external_id: "c-1".into(),
        display_name: "Vendor Support".into(),
        email: "support@vendor.example".into(),
        attributes: BTreeMap::from([("company".into(), "Vendor".into())]),
    };
    e.dir.upsert_contact(T, contact.clone());
    let full = e.sync.contacts(None).await.expect("full");
    assert_eq!((full.upserted, full.removed, full.skipped), (1, 0, 0));
    let stored = e
        .book
        .contact_by_address(T, "SUPPORT@vendor.example")
        .await
        .expect("by address")
        .expect("contact");
    assert_eq!(stored.external_id.as_deref(), Some("c-1"));
    assert_eq!(stored.display_name, "Vendor Support");
    assert_eq!(stored.attributes, contact.attributes);
    assert!(!stored.hidden_from_gal, "контакт каталога — в GAL");

    // Изменение сохраняет идентификатор; удаление из каталога удаляет контакт.
    e.dir.upsert_contact(
        T,
        DirContact {
            display_name: "Vendor".into(),
            ..contact
        },
    );
    let inc = e.sync.contacts(Some(&full.cursor)).await.expect("inc");
    assert_eq!(inc.upserted, 1);
    let again = e.book.contacts(T).await.expect("list");
    assert_eq!(again.len(), 1);
    assert_eq!(
        (again[0].id, again[0].display_name.as_str()),
        (stored.id, "Vendor")
    );
    e.dir.remove_contact(T, "c-1");
    let gone = e.sync.contacts(Some(&inc.cursor)).await.expect("gone");
    assert_eq!(gone.removed, 1);
    assert!(e.book.contacts(T).await.expect("list").is_empty());
}

#[tokio::test]
async fn org_attributes_and_units() {
    let e = env().await;
    e.dir.set_units(
        T,
        vec![
            DirUnit {
                external_id: "u-dev".into(),
                name: "Разработка".into(),
                parent: Some("u-root".into()),
            },
            DirUnit {
                external_id: "u-root".into(),
                name: "Компания".into(),
                parent: None,
            },
        ],
    );
    let mut boss = person(1);
    boss.org.seniority = Some(10);
    let mut dev = person(2);
    dev.attributes = BTreeMap::from([("office".into(), "301".into())]);
    // Руководитель — запись той же выгрузки, но позже.
    dev.org = DirOrg {
        department: Some("u-dev".into()),
        title: Some("Engineer".into()),
        manager: Some(boss.external_id.clone()),
        seniority: Some(3),
    };
    e.dir.upsert_account(T, dev.clone());
    e.dir.upsert_account(T, boss.clone());
    e.sync.accounts(None).await.expect("full");

    let units = e.book.org_units(T).await.expect("units");
    let root = units.iter().find(|u| u.name == "Компания").expect("root");
    let unit = units.iter().find(|u| u.name == "Разработка").expect("dev");
    assert_eq!(unit.parent, Some(root.id));
    let id = |x: &str| {
        let meta = e.meta.clone();
        let x = x.to_owned();
        async move {
            meta.by_external_id(T, &x)
                .await
                .expect("get")
                .expect("acc")
                .id
        }
    };
    let (dev_id, boss_id) = (id(&dev.external_id).await, id(&boss.external_id).await);
    let org = e.book.org(T, dev_id).await.expect("org");
    assert_eq!(org.department, Some(unit.id));
    assert_eq!(org.title.as_deref(), Some("Engineer"));
    assert_eq!(org.manager, Some(boss_id));
    assert_eq!(org.seniority, Some(3));
    assert_eq!(org.attributes, dev.attributes);
    assert_eq!(
        e.book.org(T, boss_id).await.expect("org").seniority,
        Some(10)
    );

    // Подразделение исчезло из каталога — удаляется, у учётной записи сбрасывается.
    e.dir.set_units(
        T,
        vec![DirUnit {
            external_id: "u-root".into(),
            name: "Компания".into(),
            parent: None,
        }],
    );
    dev.org.department = None;
    e.dir.upsert_account(T, dev);
    e.sync.accounts(None).await.expect("again");
    let units = e.book.org_units(T).await.expect("units");
    assert_eq!(units.len(), 1);
    assert_eq!(e.book.org(T, dev_id).await.expect("org").department, None);
}

#[tokio::test]
async fn disable_instead_of_delete() {
    let (e, cursor) = populated().await;
    e.dir.remove_account(T, "guid-03");
    let before = OffsetDateTime::now_utc();
    let r = e.sync.accounts(Some(&cursor)).await.expect("inc");
    assert_eq!((r.disabled, r.held.is_none()), (1, true));
    let AccountState::Disabled { until } = state(&e, 3).await else {
        panic!("ящик должен быть отключён");
    };
    let retention = RemovalPolicy::default().retention;
    assert!(until >= before + retention && until <= OffsetDateTime::now_utc() + retention);
    assert_eq!(state(&e, 4).await, AccountState::Active);
    assert_eq!(e.meta.disabled_accounts(T).await.expect("list").len(), 1);

    // Повтор не отключает повторно; полная выгрузка тоже видит отсутствие, но ящик уже отключён.
    let idle = e.sync.accounts(r.cursor.as_ref()).await.expect("idle");
    assert_eq!(idle.disabled, 0);
    let full = e.sync.accounts(None).await.expect("full");
    assert_eq!((full.disabled, full.upserted), (0, 24));

    // Учётная запись вернулась в каталог — ящик восстановлен.
    e.dir.upsert_account(T, person(3));
    let back = e.sync.accounts(full.cursor.as_ref()).await.expect("back");
    assert_eq!(back.restored, 1);
    assert_eq!(state(&e, 3).await, AccountState::Active);
}

#[tokio::test]
async fn mass_removal_stops_until_approved() {
    let (e, cursor) = populated().await;
    for n in [5, 6, 7] {
        e.dir.remove_account(T, &format!("guid-{n:02}"));
    }
    e.dir.upsert_account(
        T,
        DirAccount {
            display_name: "Renamed".into(),
            ..person(8)
        },
    );
    let r = e.sync.accounts(Some(&cursor)).await.expect("held");
    let held = r.held.expect("остановка");
    assert_eq!((held.removed, held.total, held.approved), (3, 25, false));
    assert_eq!((r.upserted, r.disabled), (0, 0), "ничего не применено");
    assert_eq!(r.cursor.as_ref(), Some(&cursor), "курсор прежний");
    for n in [5, 6, 7] {
        assert_eq!(state(&e, n).await, AccountState::Active);
    }
    assert_eq!(
        e.gate.pending_sync(T).await.expect("pending"),
        Some(held.clone())
    );

    // Без подтверждения повтор снова останавливается.
    let again = e.sync.accounts(Some(&cursor)).await.expect("held again");
    assert!(again.held.is_some());

    // Администратор подтвердил (Admin API) — запуск отключает и снимает остановку.
    e.gate.approve_sync(T).await.expect("approve");
    let done = e.sync.accounts(Some(&cursor)).await.expect("approved");
    assert_eq!((done.disabled, done.upserted, done.held), (3, 1, None));
    for n in [5, 6, 7] {
        assert!(matches!(state(&e, n).await, AccountState::Disabled { .. }));
    }
    assert_eq!(e.gate.pending_sync(T).await.expect("pending"), None);
}

#[tokio::test]
async fn empty_listing_disables_nobody() {
    let (e, _) = populated().await;
    for n in 0..25 {
        e.dir.remove_account(T, &format!("guid-{n:02}"));
    }
    let r = e.sync.accounts(None).await.expect("empty");
    assert_eq!((r.disabled, r.held), (0, None));
    assert_eq!(state(&e, 0).await, AccountState::Active);
}

#[tokio::test]
async fn foreign_domain_skipped() {
    let e = env().await;
    let mut a = person(1);
    a.emails = vec!["user@other.example".into()];
    e.dir.upsert_account(T, a);
    let r = e.sync.accounts(None).await.expect("run");
    assert_eq!((r.upserted, r.skipped), (0, 1));
}

/// Признак «скрыта из книги «Организация»» каталога (CTR-83) переносится в учётную запись и снимается вместе с ним.
#[tokio::test]
async fn hidden_from_gal_synced() {
    let e = env().await;
    let mut a = person(1);
    a.hidden_from_gal = true;
    e.dir.upsert_account(T, a.clone());
    let r = e.sync.accounts(None).await.expect("run");
    let stored = |e: &Env| {
        let meta = e.meta.clone();
        async move {
            meta.by_external_id(T, "guid-01")
                .await
                .expect("by_external_id")
                .expect("account")
                .hidden_from_gal
        }
    };
    assert!(stored(&e).await);
    a.hidden_from_gal = false;
    e.dir.upsert_account(T, a);
    e.sync.accounts(r.cursor.as_ref()).await.expect("again");
    assert!(!stored(&e).await);
}

#[test]
fn zero_batch_rejected() {
    let meta = MemMetadata::new(Arc::new(SystemClock));
    let book = MemAddressBook::new();
    let stores = DirStores {
        accounts: Arc::new(meta),
        contacts: Arc::new(book.clone()),
        org: Arc::new(book),
        gate: Arc::new(MemSyncGate::new()),
        transport: Arc::new(MemTransport::new()),
    };
    let r = DirectorySync::new(
        Arc::new(FakeDirectory::default()),
        T,
        0,
        RemovalPolicy::default(),
        stores,
    );
    assert!(matches!(r, Err(Error::Invalid(_))));
}
