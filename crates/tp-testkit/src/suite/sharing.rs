//! Наборы проверок общего доступа (CTR-48, CONTRACTS.md#Sharing): права на коллекции, представители, правила
//! бронирования ресурсов, политика организации, уведомления о доступе (CTR-86). In-memory (`MemDav`, `MemMetadata`,
//! `MemShareNotifications`) и `PgMeta` (META-52) проходят одни и те же проверки.

use std::collections::BTreeSet;
use std::time::Duration;

use time::{OffsetDateTime, Time};
use tp_contracts::dav::{Collection, CollectionId, CollectionKind, DavStore};
use tp_contracts::meta::{AccountKind, AccountScope, AccountStore, AccountUpsert};
use tp_contracts::sharing::{
    ANYONE, Bookers, BookingPolicy, CollectionAcl, CollectionAclEntry, CollectionRights, DayOfWeek,
    Delegate, DelegateLevel, DelegateStore, Delegation, MAX_NOTICES, ResourceStore, ShareChangedBy,
    ShareNotification, ShareNotificationFilter, ShareNotificationStore, SharedCollection,
    SharedObjectType, SharingPolicy, SharingPolicyStore, SharingScope, WorkingHours,
};
use tp_contracts::{AccountId, Error, GroupId, ShareNotificationId, TenantId};

/// Учётные записи наборов: `owner`, `peer` — пользователи одной организации, `resource` — учётная запись вида
/// [`AccountKind::Resource`] той же организации, `foreign` — учётная запись другой организации.
#[derive(Debug, Clone, Copy)]
pub struct SharingFixture {
    /// Владелец коллекций.
    pub owner: AccountScope,
    /// Другой пользователь организации.
    pub peer: AccountScope,
    /// Ресурс организации (переговорная).
    pub resource: AccountScope,
    /// Учётная запись другой организации.
    pub foreign: AccountScope,
}

fn ids(list: &[&str]) -> Vec<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

fn entry(identifier: &str, rights: CollectionRights) -> CollectionAclEntry {
    CollectionAclEntry {
        identifier: identifier.into(),
        rights,
    }
}

async fn create(
    dav: &dyn DavStore,
    scope: AccountScope,
    kind: CollectionKind,
    name: &str,
) -> CollectionId {
    let c = Collection::new(kind, name);
    let id = c.id;
    dav.create_collection(scope, c).await.expect("коллекция");
    id
}

/// Набор [`CollectionAcl`]: нет коллекции, недопустимые субъекты, запись как выдана, замена и удаление пустыми
/// правами, действующие права (объединение субъектов с младшими), `shared_with` (порядок, без своих коллекций,
/// без коллекций без прав), удаление коллекции, разделение организаций. `dav` — коллекции того же хранилища; у
/// учётных записей `owner` и `peer` коллекций нет.
pub async fn collection_acl(acl: &dyn CollectionAcl, dav: &dyn DavStore, fx: &SharingFixture) {
    let (owner, peer) = (fx.owner, fx.peer);
    let cal = create(dav, owner, CollectionKind::Calendar, "work").await;
    let book = create(dav, owner, CollectionKind::AddressBook, "team").await;
    let private = create(dav, owner, CollectionKind::Calendar, "private").await;
    let peer_cal = create(dav, peer, CollectionKind::Calendar, "peer").await;

    // ---------- Нет коллекции ----------
    let missing = CollectionId::new();
    assert_eq!(
        acl.collection_acl(owner, missing).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        acl.set_collection_acl(owner, missing, "bob", CollectionRights::READ)
            .await,
        Err(Error::NotFound)
    );
    assert_eq!(
        acl.set_collection_acl(peer, cal, "bob", CollectionRights::READ)
            .await,
        Err(Error::NotFound),
        "коллекция другой учётной записи"
    );
    assert_eq!(
        acl.collection_rights(owner, missing, &ids(&["bob"])).await,
        Ok(CollectionRights::NONE)
    );
    assert_eq!(acl.collection_acl(owner, cal).await, Ok(Vec::new()));

    // ---------- Недопустимые субъекты ----------
    for bad in ["", "-bob", "-", "a b", "a\tb"] {
        assert!(
            matches!(
                acl.set_collection_acl(owner, cal, bad, CollectionRights::READ)
                    .await,
                Err(Error::Invalid(_))
            ),
            "субъект {bad:?}"
        );
    }
    assert!(matches!(
        acl.collection_rights(owner, cal, &ids(&["-bob"])).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        acl.shared_with(peer, &ids(&["a b"])).await,
        Err(Error::Invalid(_))
    ));

    // ---------- Запись, замена, удаление ----------
    let fb_sched = CollectionRights {
        free_busy: true,
        schedule: true,
        ..CollectionRights::NONE
    };
    let write_only = CollectionRights {
        write: true,
        ..CollectionRights::NONE
    };
    acl.set_collection_acl(owner, cal, "bob", CollectionRights::READ)
        .await
        .expect("bob");
    acl.set_collection_acl(owner, cal, "team", write_only)
        .await
        .expect("team");
    acl.set_collection_acl(owner, cal, ANYONE, CollectionRights::FREE_BUSY)
        .await
        .expect("anyone");
    acl.set_collection_acl(owner, book, "bob", fb_sched)
        .await
        .expect("book");
    assert_eq!(
        acl.collection_acl(owner, cal).await,
        Ok(vec![
            entry(ANYONE, CollectionRights::FREE_BUSY),
            entry("bob", CollectionRights::READ),
            entry("team", write_only),
        ]),
        "по идентификатору, права как выданы"
    );
    acl.set_collection_acl(owner, cal, "bob", CollectionRights::SCHEDULE)
        .await
        .expect("замена");
    acl.set_collection_acl(owner, cal, "carol", CollectionRights::ALL)
        .await
        .expect("carol");
    acl.set_collection_acl(owner, cal, "carol", CollectionRights::NONE)
        .await
        .expect("отзыв");
    acl.set_collection_acl(owner, cal, "dave", CollectionRights::NONE)
        .await
        .expect("отзыв без записи");
    assert_eq!(
        acl.collection_acl(owner, cal).await,
        Ok(vec![
            entry(ANYONE, CollectionRights::FREE_BUSY),
            entry("bob", CollectionRights::SCHEDULE),
            entry("team", write_only),
        ])
    );

    // ---------- Действующие права ----------
    assert_eq!(
        acl.collection_rights(owner, cal, &ids(&["bob", "team"]))
            .await,
        Ok(CollectionRights {
            schedule: true,
            ..CollectionRights::WRITE
        }),
        "объединение с младшими"
    );
    assert_eq!(
        acl.collection_rights(owner, cal, &ids(&["bob"])).await,
        Ok(CollectionRights::SCHEDULE),
        "anyone — только среди идентификаторов"
    );
    assert_eq!(
        acl.collection_rights(owner, cal, &ids(&["carol"])).await,
        Ok(CollectionRights::NONE)
    );
    assert_eq!(
        acl.collection_rights(owner, cal, &[]).await,
        Ok(CollectionRights::NONE)
    );
    acl.set_collection_acl(
        owner,
        private,
        "zed",
        CollectionRights {
            admin: true,
            ..CollectionRights::NONE
        },
    )
    .await
    .expect("admin");
    assert_eq!(
        acl.collection_rights(owner, private, &ids(&["zed"])).await,
        Ok(CollectionRights::ALL),
        "admin — все права"
    );

    // ---------- Чужие коллекции субъекта ----------
    acl.set_collection_acl(peer, peer_cal, "bob", CollectionRights::WRITE)
        .await
        .expect("peer");
    let mut expected = vec![
        SharedCollection {
            owner: owner.account,
            collection: cal,
            rights: CollectionRights {
                schedule: true,
                ..CollectionRights::FREE_BUSY
            },
        },
        SharedCollection {
            owner: owner.account,
            collection: book,
            rights: fb_sched,
        },
    ];
    expected.sort_by_key(|s| (s.owner, s.collection));
    assert_eq!(
        acl.shared_with(peer, &ids(&["bob", ANYONE])).await,
        Ok(expected),
        "без своих коллекций и коллекций без прав, по (владелец, коллекция)"
    );
    let mut both = vec![
        SharedCollection {
            owner: owner.account,
            collection: cal,
            rights: CollectionRights {
                schedule: true,
                ..CollectionRights::FREE_BUSY
            },
        },
        SharedCollection {
            owner: owner.account,
            collection: book,
            rights: fb_sched,
        },
        SharedCollection {
            owner: peer.account,
            collection: peer_cal,
            rights: CollectionRights::WRITE,
        },
    ];
    both.sort_by_key(|s| (s.owner, s.collection));
    assert_eq!(
        acl.shared_with(fx.resource, &ids(&["bob", ANYONE])).await,
        Ok(both)
    );
    assert_eq!(
        acl.shared_with(fx.resource, &ids(&["team"])).await,
        Ok(vec![SharedCollection {
            owner: owner.account,
            collection: cal,
            rights: CollectionRights::WRITE,
        }]),
        "действующие права — с младшими"
    );
    assert_eq!(
        acl.shared_with(peer, &ids(&["nobody"])).await,
        Ok(Vec::new())
    );
    assert_eq!(acl.shared_with(peer, &[]).await, Ok(Vec::new()));

    // ---------- Разделение организаций ----------
    assert_eq!(
        acl.shared_with(fx.foreign, &ids(&["bob", ANYONE])).await,
        Ok(Vec::new())
    );
    let alien = AccountScope {
        tenant: fx.foreign.tenant,
        account: owner.account,
    };
    assert_eq!(acl.collection_acl(alien, cal).await, Err(Error::NotFound));
    assert_eq!(
        acl.collection_rights(alien, cal, &ids(&["bob"])).await,
        Ok(CollectionRights::NONE)
    );

    // ---------- Удаление коллекции ----------
    dav.delete_collection(owner, book).await.expect("удаление");
    assert_eq!(acl.collection_acl(owner, book).await, Err(Error::NotFound));
    assert_eq!(
        acl.collection_rights(owner, book, &ids(&["bob"])).await,
        Ok(CollectionRights::NONE)
    );
    assert_eq!(
        acl.shared_with(peer, &ids(&["bob"])).await,
        Ok(vec![SharedCollection {
            owner: owner.account,
            collection: cal,
            rights: CollectionRights::SCHEDULE,
        }])
    );
    let again = Collection::new(CollectionKind::AddressBook, "team");
    let again_id = again.id;
    dav.create_collection(owner, again)
        .await
        .expect("то же имя");
    assert_eq!(
        acl.collection_acl(owner, again_id).await,
        Ok(Vec::new()),
        "новая коллекция с прежним именем — без записей"
    );
}

/// Набор [`DelegateStore`]: нет учётных записей, сам себе, назначение и замена, порядок, обратный список
/// `delegations`, отзыв, разделение организаций.
pub async fn delegate_store(store: &dyn DelegateStore, fx: &SharingFixture) {
    let (owner, peer, room) = (fx.owner, fx.peer, fx.resource);
    let missing = AccountScope {
        tenant: owner.tenant,
        account: AccountId::new(),
    };
    let read = |delegate: AccountId| Delegate {
        delegate,
        level: DelegateLevel::Read,
        schedule_on_behalf: false,
    };
    assert_eq!(store.delegates(owner).await, Ok(Vec::new()));
    assert_eq!(store.delegates(missing).await, Err(Error::NotFound));
    assert_eq!(store.delegations(peer).await, Ok(Vec::new()));
    assert_eq!(store.delegations(missing).await, Ok(Vec::new()));
    assert!(matches!(
        store.set_delegate(owner, read(owner.account)).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        store.set_delegate(owner, read(missing.account)).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        store.set_delegate(missing, read(peer.account)).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        store.set_delegate(owner, read(fx.foreign.account)).await,
        Err(Error::NotFound),
        "представитель другой организации"
    );
    assert_eq!(
        store.remove_delegate(missing, peer.account).await,
        Err(Error::NotFound)
    );

    // ---------- Назначение, замена, порядок ----------
    store
        .set_delegate(owner, read(peer.account))
        .await
        .expect("peer");
    let room_delegate = Delegate {
        delegate: room.account,
        level: DelegateLevel::Write,
        schedule_on_behalf: true,
    };
    store
        .set_delegate(owner, room_delegate)
        .await
        .expect("room");
    let mut expected = vec![read(peer.account), room_delegate];
    expected.sort_by_key(|d| d.delegate);
    assert_eq!(store.delegates(owner).await, Ok(expected));
    let writer = Delegate {
        delegate: peer.account,
        level: DelegateLevel::Write,
        schedule_on_behalf: true,
    };
    store.set_delegate(owner, writer).await.expect("замена");
    let mut expected = vec![writer, room_delegate];
    expected.sort_by_key(|d| d.delegate);
    assert_eq!(store.delegates(owner).await, Ok(expected));

    // ---------- Обратный список ----------
    store
        .set_delegate(room, read(peer.account))
        .await
        .expect("ответственный ресурса");
    let mut delegations = vec![
        Delegation {
            owner: owner.account,
            level: DelegateLevel::Write,
            schedule_on_behalf: true,
        },
        Delegation {
            owner: room.account,
            level: DelegateLevel::Read,
            schedule_on_behalf: false,
        },
    ];
    delegations.sort_by_key(|d| d.owner);
    assert_eq!(store.delegations(peer).await, Ok(delegations));
    assert_eq!(store.delegations(owner).await, Ok(Vec::new()));
    assert_eq!(store.delegates(room).await, Ok(vec![read(peer.account)]));

    // ---------- Разделение организаций ----------
    let alien = AccountScope {
        tenant: fx.foreign.tenant,
        account: peer.account,
    };
    assert_eq!(store.delegations(alien).await, Ok(Vec::new()));
    assert_eq!(
        store
            .delegates(AccountScope {
                tenant: fx.foreign.tenant,
                account: owner.account,
            })
            .await,
        Err(Error::NotFound)
    );
    assert_eq!(store.delegates(fx.foreign).await, Ok(Vec::new()));

    // ---------- Отзыв ----------
    store
        .remove_delegate(owner, peer.account)
        .await
        .expect("отзыв");
    store
        .remove_delegate(owner, peer.account)
        .await
        .expect("повторный отзыв");
    assert_eq!(store.delegates(owner).await, Ok(vec![room_delegate]));
    assert_eq!(
        store.delegations(peer).await,
        Ok(vec![Delegation {
            owner: room.account,
            level: DelegateLevel::Read,
            schedule_on_behalf: false,
        }])
    );
}

/// Запись учётной записи `scope` с видом `kind` (остальные поля — прежние).
async fn set_kind(accounts: &dyn AccountStore, scope: AccountScope, kind: AccountKind) {
    let a = accounts
        .get(scope.tenant, scope.account)
        .await
        .expect("get")
        .expect("учётная запись");
    accounts
        .upsert(
            scope.tenant,
            AccountUpsert {
                domain: a.domain,
                login: a.login,
                display_name: a.display_name,
                external_id: a.external_id,
                enabled: a.enabled,
                aliases: a.aliases,
                kind,
                hidden_from_gal: a.hidden_from_gal,
                resource_type: None,
            },
        )
        .await
        .expect("upsert");
}

/// Набор [`ResourceStore`]: список ресурсов, нет учётной записи, не ресурс, недопустимые правила, запись и чтение
/// всех полей, снятие, смена вида на пользователя снимает правила, разделение организаций. `accounts` — учётные
/// записи того же хранилища; в организации `fx` ресурс один — `fx.resource`, правил у него нет.
pub async fn resource_store(
    store: &dyn ResourceStore,
    accounts: &dyn AccountStore,
    fx: &SharingFixture,
) {
    let (owner, room) = (fx.owner, fx.resource);
    let missing = AccountScope {
        tenant: owner.tenant,
        account: AccountId::new(),
    };
    assert_eq!(store.resources(owner.tenant).await, Ok(vec![room.account]));
    assert_eq!(store.booking_policy(room).await, Ok(None));
    assert_eq!(store.booking_policy(owner).await, Ok(None));
    assert_eq!(store.booking_policy(missing).await, Err(Error::NotFound));
    assert_eq!(
        store
            .booking_policy(AccountScope {
                tenant: fx.foreign.tenant,
                account: room.account,
            })
            .await,
        Err(Error::NotFound),
        "другая организация"
    );

    let policy = BookingPolicy {
        auto_accept: false,
        allow_conflicts: true,
        max_duration: Some(Duration::from_secs(2 * 3600)),
        horizon: None,
        bookers: Bookers::Only {
            accounts: vec![owner.account],
            groups: vec![GroupId::new()],
        },
        working_hours: Some(WorkingHours {
            timezone: "Europe/Moscow".into(),
            days: BTreeSet::from([DayOfWeek::Monday, DayOfWeek::Friday]),
            from: Time::from_hms(9, 0, 0).expect("время"),
            to: Time::from_hms(18, 30, 0).expect("время"),
        }),
    };
    assert_eq!(
        store
            .set_booking_policy(missing, Some(policy.clone()))
            .await,
        Err(Error::NotFound)
    );
    assert!(matches!(
        store.set_booking_policy(owner, Some(policy.clone())).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        store
            .set_booking_policy(
                room,
                Some(BookingPolicy {
                    max_duration: Some(Duration::ZERO),
                    ..BookingPolicy::default()
                })
            )
            .await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(store.booking_policy(room).await, Ok(None));

    // ---------- Запись, замена, снятие ----------
    store
        .set_booking_policy(room, Some(policy.clone()))
        .await
        .expect("правила");
    assert_eq!(store.booking_policy(room).await, Ok(Some(policy.clone())));
    store
        .set_booking_policy(room, Some(BookingPolicy::default()))
        .await
        .expect("замена");
    assert_eq!(
        store.booking_policy(room).await,
        Ok(Some(BookingPolicy::default()))
    );
    store.set_booking_policy(room, None).await.expect("снятие");
    assert_eq!(store.booking_policy(room).await, Ok(None));

    // ---------- Смена вида ----------
    store
        .set_booking_policy(room, Some(policy.clone()))
        .await
        .expect("правила");
    set_kind(accounts, room, AccountKind::Resource).await;
    assert_eq!(
        store.booking_policy(room).await,
        Ok(Some(policy.clone())),
        "запись без смены вида правила не меняет"
    );
    set_kind(accounts, room, AccountKind::User).await;
    assert_eq!(store.booking_policy(room).await, Ok(None));
    assert_eq!(store.resources(owner.tenant).await, Ok(Vec::new()));
    set_kind(accounts, room, AccountKind::Resource).await;
    assert_eq!(
        store.booking_policy(room).await,
        Ok(None),
        "смена вида на пользователя сняла правила"
    );
    assert_eq!(store.resources(owner.tenant).await, Ok(vec![room.account]));
    assert_eq!(store.resources(fx.foreign.tenant).await, Ok(Vec::new()));
}

/// Набор [`SharingPolicyStore`]: политика по умолчанию, запись и замена, разделение организаций.
pub async fn sharing_policy_store(
    store: &dyn SharingPolicyStore,
    tenant: TenantId,
    foreign: TenantId,
) {
    assert_eq!(
        store.sharing_policy(tenant).await,
        Ok(SharingPolicy::default())
    );
    let groups = SharingPolicy {
        scope: SharingScope::Groups {
            groups: vec![GroupId::new(), GroupId::new()],
        },
        max_outside: CollectionRights::FREE_BUSY,
    };
    store
        .set_sharing_policy(tenant, groups.clone())
        .await
        .expect("политика");
    assert_eq!(store.sharing_policy(tenant).await, Ok(groups));
    assert_eq!(
        store.sharing_policy(foreign).await,
        Ok(SharingPolicy::default())
    );
    let nobody = SharingPolicy {
        scope: SharingScope::Nobody,
        max_outside: CollectionRights::NONE,
    };
    store
        .set_sharing_policy(tenant, nobody.clone())
        .await
        .expect("замена");
    assert_eq!(store.sharing_policy(tenant).await, Ok(nobody));
}

/// Набор проверок `ShareNotificationStore` (CTR-86): уведомление видно только получателю, по (`created`, `id`);
/// повтор идентификатора — `Conflict`, права без изменения — `Invalid`; изменения по состоянию (записанное и удалённое
/// после `since` не входит, `limit`, `has_more`, состояние из будущего — `Conflict`); удалённое не возвращается; срок
/// хранения удаляет старые у всех получателей и попадает в изменения; сверх `MAX_NOTICES` запись вытесняет старшее
/// (CTR-90), вытесненное — в `destroyed`. `recipient`, `other` — новые учётные записи
/// без уведомлений (разных организаций или одной).
pub async fn share_notifications(
    store: &dyn ShareNotificationStore,
    recipient: AccountScope,
    other: AccountScope,
) {
    let t0 = OffsetDateTime::UNIX_EPOCH + time::Duration::days(400);
    let sec = time::Duration::seconds;
    let (o1, o2) = (AccountId::new(), AccountId::new());
    let notice = |at: i64, object_type, owner, new_rights| ShareNotification {
        id: ShareNotificationId::new(),
        created: t0 + sec(at),
        changed_by: ShareChangedBy {
            name: "Алиса".into(),
            email: Some("alice@example.org".into()),
            account: Some(owner),
        },
        object_type,
        owner,
        collection: CollectionId::new(),
        old_rights: CollectionRights::NONE,
        new_rights,
        name: "Общая".into(),
    };
    let sorted = |mut ids: Vec<ShareNotificationId>| {
        ids.sort_unstable();
        ids
    };
    let n1 = notice(0, SharedObjectType::Calendar, o1, CollectionRights::READ);
    let n2 = notice(
        10,
        SharedObjectType::AddressBook,
        o2,
        CollectionRights::WRITE,
    );
    let n3 = notice(20, SharedObjectType::TaskList, o1, CollectionRights::ALL);

    assert_eq!(store.notification_state(recipient).await, Ok(0));
    assert_eq!(store.record(recipient, n1.clone()).await, Ok(1));
    assert_eq!(store.record(recipient, n2.clone()).await, Ok(2));
    assert_eq!(
        store.record(recipient, n1.clone()).await,
        Err(Error::Conflict)
    );
    let unchanged = notice(5, SharedObjectType::Calendar, o1, CollectionRights::NONE);
    assert!(matches!(
        store.record(recipient, unchanged).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(store.record(recipient, n3.clone()).await, Ok(3));

    assert_eq!(
        store.notifications(recipient).await,
        Ok(vec![n1.clone(), n2.clone(), n3.clone()])
    );
    assert_eq!(
        store.notification(recipient, n2.id).await,
        Ok(Some(n2.clone()))
    );
    assert_eq!(
        store.notifications(other).await,
        Ok(Vec::new()),
        "чужой ящик"
    );
    assert_eq!(store.notification(other, n1.id).await, Ok(None));
    assert_eq!(store.notification_state(other).await, Ok(0));
    let by_owner = ShareNotificationFilter {
        owner: Some(o1),
        ..ShareNotificationFilter::default()
    };
    let mine: Vec<_> = store
        .notifications(recipient)
        .await
        .expect("notifications")
        .into_iter()
        .filter(|n| by_owner.matches(n))
        .collect();
    assert_eq!(mine, [n1.clone(), n3.clone()]);

    let c = store
        .notification_changes(recipient, 0, 10)
        .await
        .expect("changes");
    assert_eq!(c.created, sorted(vec![n1.id, n2.id, n3.id]));
    assert!(c.destroyed.is_empty());
    assert_eq!((c.new_state, c.has_more), (3, false));
    let c = store
        .notification_changes(recipient, 0, 2)
        .await
        .expect("changes limit");
    assert_eq!(c.created, sorted(vec![n1.id, n2.id]));
    assert_eq!((c.new_state, c.has_more), (2, true));
    let c = store
        .notification_changes(recipient, 2, 2)
        .await
        .expect("changes rest");
    assert_eq!(c.created, [n3.id]);
    assert_eq!((c.new_state, c.has_more), (3, false));

    // Удаление (`/set destroy`).
    assert_eq!(store.destroy_notification(recipient, n2.id).await, Ok(4));
    assert_eq!(
        store.destroy_notification(recipient, n2.id).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        store.destroy_notification(other, n1.id).await,
        Err(Error::NotFound),
        "чужое не удаляется"
    );
    assert_eq!(store.notification(recipient, n2.id).await, Ok(None));
    assert_eq!(
        store.notifications(recipient).await,
        Ok(vec![n1.clone(), n3.clone()])
    );
    let c = store
        .notification_changes(recipient, 3, 10)
        .await
        .expect("changes destroy");
    assert_eq!((c.created, c.destroyed), (Vec::new(), vec![n2.id]));
    let c = store
        .notification_changes(recipient, 0, 10)
        .await
        .expect("changes from start");
    assert_eq!(
        c.created,
        sorted(vec![n1.id, n3.id]),
        "записанное и удалённое"
    );
    assert!(c.destroyed.is_empty());
    assert_eq!(c.new_state, 4);
    assert_eq!(
        store.notification_changes(recipient, 5, 10).await,
        Err(Error::Conflict)
    );
    assert!(matches!(
        store.notification_changes(recipient, 0, 0).await,
        Err(Error::Invalid(_))
    ));

    // Срок хранения: старше `t0 + 15` — у всех получателей.
    let n4 = notice(5, SharedObjectType::Calendar, o2, CollectionRights::READ);
    assert_eq!(store.record(other, n4.clone()).await, Ok(1));
    let removed = store
        .expire_notifications(t0 + sec(15), 1000)
        .await
        .expect("expire");
    assert!(removed >= 2, "удалено {removed}");
    assert_eq!(store.notifications(recipient).await, Ok(vec![n3.clone()]));
    assert_eq!(store.notifications(other).await, Ok(Vec::new()));
    assert_eq!(store.notification_state(recipient).await, Ok(5));
    assert_eq!(store.notification_state(other).await, Ok(2));
    let c = store
        .notification_changes(recipient, 4, 10)
        .await
        .expect("changes expire");
    assert_eq!((c.created, c.destroyed), (Vec::new(), vec![n1.id]));
    assert_eq!(
        store.expire_notifications(t0 + sec(15), 1000).await,
        Ok(0),
        "повтор — нечего удалять"
    );

    // Предел (CTR-90): `MAX_NOTICES` записей, затем ещё одна вытесняет старшую. Время — позже срока хранения выше:
    // параллельный прогон набора на той же БД их не удалит.
    let full: Vec<_> = (0..MAX_NOTICES)
        .map(|i| {
            notice(
                100 + i64::try_from(i).expect("i64"),
                SharedObjectType::Calendar,
                o1,
                CollectionRights::READ,
            )
        })
        .collect();
    for n in &full {
        store.record(other, n.clone()).await.expect("record");
    }
    let base = 2 + u64::try_from(MAX_NOTICES).expect("u64");
    assert_eq!(store.notification_state(other).await, Ok(base));
    let last = notice(
        100_000,
        SharedObjectType::Calendar,
        o1,
        CollectionRights::READ,
    );
    assert_eq!(
        store.record(other, last.clone()).await,
        Ok(base + 2),
        "запись и вытеснение"
    );
    let list = store.notifications(other).await.expect("notifications");
    assert_eq!(list.len(), MAX_NOTICES);
    assert_eq!(list.first(), full.get(1), "старшее вытеснено");
    assert_eq!(list.last(), Some(&last));
    assert_eq!(store.notification(other, full[0].id).await, Ok(None));
    let c = store
        .notification_changes(other, base, 10)
        .await
        .expect("changes evict");
    assert_eq!((c.created, c.destroyed), (vec![last.id], vec![full[0].id]));
    assert_eq!((c.new_state, c.has_more), (base + 2, false));
}
