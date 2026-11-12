//! Набор проверок [`DavStore`] (CTR-45): in-memory `MemDav` и `PgMeta` (META-47) проходят одни и те же проверки.

use std::collections::BTreeSet;

use time::{Duration, OffsetDateTime};
use tp_contracts::bus::{EventBus, EventKind};
use tp_contracts::dav::{
    CardSearch, Collection, CollectionId, CollectionKind, Component, DavLimits, DavStore,
    DeleteOutcome, GAL_COLLECTION_NAME, GalCard, MoveOutcome, ObjectIndex, Precondition, Put,
    PutOutcome, Span, TimeRange, default_of,
};
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, Error, ModSeq, TenantId};

use super::next_signal;

/// Лимиты, с которыми хранилище проходит набор [`dav`](super::dav()) (у `PgMeta` — секция `meta` теста).
pub const LIMITS: DavLimits = DavLimits {
    max_object_size: 4096,
    max_objects: 4,
    max_collections: 3,
    max_busy_instances: 1000,
};

/// Учётные записи набора: `owner`, `peer` — без коллекций, одной организации; `foreign` — та же учётная запись `owner`
/// в другой организации (разделение организаций).
#[derive(Debug, Clone, Copy)]
pub struct DavFixture {
    /// Учётная запись, в которой работает набор.
    pub owner: AccountScope,
    /// Другая учётная запись той же организации.
    pub peer: AccountScope,
    /// `owner.account` в другой организации.
    pub foreign: AccountScope,
}

impl DavFixture {
    /// Новые случайные учётные записи (in-memory).
    pub fn new() -> Self {
        let tenant = TenantId::new();
        let owner = AccountScope {
            tenant,
            account: AccountId::new(),
        };
        Self {
            owner,
            peer: AccountScope {
                tenant,
                account: AccountId::new(),
            },
            foreign: AccountScope {
                tenant: TenantId::new(),
                account: owner.account,
            },
        }
    }
}

impl Default for DavFixture {
    fn default() -> Self {
        Self::new()
    }
}

fn at(h: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::days(20_000) + Duration::hours(h)
}

fn event(uid: &str, span: Option<(i64, Option<i64>)>) -> ObjectIndex {
    ObjectIndex::new(
        Component::Event,
        uid,
        span.map(|(s, e)| Span {
            start: at(s),
            end: e.map(at),
        }),
    )
}

fn body(uid: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

async fn put(
    store: &dyn DavStore,
    scope: AccountScope,
    coll: CollectionId,
    name: &str,
    index: ObjectIndex,
    pre: Precondition,
) -> PutOutcome {
    let text = body(&index.uid);
    store
        .put_object(scope, coll, name, text, index, pre)
        .await
        .expect("put_object")
}

fn stored(o: PutOutcome) -> Put {
    match o {
        PutOutcome::Stored(p) => p,
        other => panic!("ожидалась запись: {other:?}"),
    }
}

fn names(objs: &[tp_contracts::dav::DavObject]) -> Vec<&str> {
    objs.iter().map(|o| o.name.as_str()).collect()
}

/// Набор проверок [`DavStore`]: коллекции (порядок, имя по виду, свойства, лимит), условия ETag, конфликт UID,
/// лимиты объектов, поиск по интервалу, надгробия, `changes` по коллекции (страницы, повторное создание имени),
/// `collection_changes` и `account_changes`, удаление коллекции, разделение учётных записей и организаций.
/// Хранилище — с лимитами [`LIMITS`].
pub async fn dav(store: &dyn DavStore, fx: &DavFixture) {
    let s = fx.owner;
    assert_eq!(store.limits(), LIMITS, "набор требует лимиты LIMITS");
    for kind in [CollectionKind::Calendar, CollectionKind::AddressBook] {
        assert!(
            store
                .list_collections(s, kind)
                .await
                .expect("list")
                .is_empty()
        );
        assert_eq!(store.state(s, kind).await.expect("state"), ModSeq::ZERO);
    }

    // ---------- Коллекции ----------
    let work = Collection {
        order: 1,
        color: Some("#3366ff".into()),
        ..Collection::new(CollectionKind::Calendar, "work")
    };
    let home = Collection::new(CollectionKind::Calendar, "home");
    let book = Collection::new(CollectionKind::AddressBook, "work");
    let m_work = store
        .create_collection(s, work.clone())
        .await
        .expect("work");
    let m_home = store
        .create_collection(s, home.clone())
        .await
        .expect("home");
    assert!(m_home > m_work);
    assert_eq!(
        store
            .create_collection(s, Collection::new(CollectionKind::Calendar, "work"))
            .await,
        Err(Error::Conflict),
        "имя занято в виде"
    );
    assert_eq!(
        store
            .create_collection(
                s,
                Collection {
                    name: "other".into(),
                    ..work.clone()
                }
            )
            .await,
        Err(Error::Conflict),
        "идентификатор занят"
    );
    assert!(matches!(
        store
            .create_collection(s, Collection::new(CollectionKind::Calendar, "a/b"))
            .await,
        Err(Error::Invalid(_))
    ));
    // То же имя у другого вида допустимо.
    let m_book = store
        .create_collection(s, book.clone())
        .await
        .expect("book");
    assert_eq!(
        store
            .create_collection(s, Collection::new(CollectionKind::AddressBook, "more"))
            .await,
        Err(Error::QuotaExceeded),
        "коллекций не больше max_collections"
    );

    let cals = store
        .list_collections(s, CollectionKind::Calendar)
        .await
        .expect("list");
    let cal_names: Vec<&str> = cals.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(cal_names, ["home", "work"], "по order, затем по имени");
    assert_eq!(cals[1].state, m_work);
    assert_eq!(cals[1].color.as_deref(), Some("#3366ff"));
    let got = store
        .collection_by_name(s, CollectionKind::AddressBook, "work")
        .await
        .expect("by_name")
        .expect("book");
    assert_eq!((got.id, got.state), (book.id, m_book));
    assert_eq!(
        store
            .collection_by_name(s, CollectionKind::Calendar, "none")
            .await
            .expect("by_name"),
        None
    );
    assert_eq!(
        store
            .state(s, CollectionKind::Calendar)
            .await
            .expect("state"),
        m_home
    );

    let m_upd = store
        .update_collection(
            s,
            Collection {
                display_name: "Работа".into(),
                description: Some("Встречи".into()),
                timezone: Some("Europe/Moscow".into()),
                components: BTreeSet::from([Component::Event, Component::Todo, Component::Journal]),
                ..work.clone()
            },
        )
        .await
        .expect("update");
    assert!(m_upd > m_book);
    let work_now = store
        .get_collection(s, work.id)
        .await
        .expect("get")
        .expect("work");
    assert_eq!(work_now.display_name, "Работа");
    assert_eq!(work_now.timezone.as_deref(), Some("Europe/Moscow"));
    assert_eq!(work_now.state, m_upd);
    assert!(matches!(
        store
            .update_collection(
                s,
                Collection {
                    name: "renamed".into(),
                    ..work.clone()
                }
            )
            .await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        store
            .update_collection(s, Collection::new(CollectionKind::Calendar, "x"))
            .await,
        Err(Error::NotFound)
    );

    // ---------- Объекты: условия ETag ----------
    let a = stored(
        put(
            store,
            s,
            work.id,
            "a.ics",
            event("uid-a", Some((10, Some(11)))),
            Precondition::IfNoneMatch,
        )
        .await,
    );
    assert!(a.created && a.modseq > m_upd);
    assert_eq!(
        put(
            store,
            s,
            work.id,
            "a.ics",
            event("uid-a", None),
            Precondition::IfNoneMatch
        )
        .await,
        PutOutcome::PreconditionFailed
    );
    assert_eq!(
        put(
            store,
            s,
            work.id,
            "a.ics",
            event("uid-a", None),
            Precondition::IfMatch(format!("{}x", a.etag))
        )
        .await,
        PutOutcome::PreconditionFailed
    );
    assert_eq!(
        put(
            store,
            s,
            work.id,
            "new.ics",
            event("uid-new", None),
            Precondition::IfMatch(a.etag.clone())
        )
        .await,
        PutOutcome::PreconditionFailed,
        "If-Match без ресурса"
    );
    let a2 = stored(
        put(
            store,
            s,
            work.id,
            "a.ics",
            event("uid-a", Some((10, Some(12)))),
            Precondition::IfMatch(a.etag.clone()),
        )
        .await,
    );
    assert!(!a2.created && a2.id == a.id && a2.etag != a.etag && a2.modseq > a.modseq);
    let got = store
        .get_object(s, work.id, "a.ics")
        .await
        .expect("get")
        .expect("a");
    assert_eq!((got.id, got.collection), (a.id, work.id));
    assert_eq!(
        (got.etag.as_str(), got.body),
        (a2.etag.as_str(), body("uid-a"))
    );
    assert_eq!(got.index, event("uid-a", Some((10, Some(12)))));

    // Конфликт UID, компоненты, размер.
    assert_eq!(
        put(
            store,
            s,
            work.id,
            "b.ics",
            event("uid-a", None),
            Precondition::Any
        )
        .await,
        PutOutcome::UidConflict {
            name: "a.ics".into()
        }
    );
    assert!(
        store
            .get_object(s, work.id, "b.ics")
            .await
            .expect("get")
            .is_none(),
        "при конфликте ничего не записано"
    );
    let card = ObjectIndex {
        component: Component::Card,
        ..event("uid-card", None)
    };
    assert!(matches!(
        store
            .put_object(
                s,
                work.id,
                "c.vcf",
                "x".into(),
                card.clone(),
                Precondition::Any
            )
            .await,
        Err(Error::Invalid(_))
    ));
    let journal = ObjectIndex {
        component: Component::Journal,
        ..event("uid-j", None)
    };
    assert!(
        matches!(
            store
                .put_object(s, home.id, "j.ics", "x".into(), journal, Precondition::Any)
                .await,
            Err(Error::Invalid(_))
        ),
        "VJOURNAL не в компонентах home"
    );
    assert!(matches!(
        store
            .put_object(
                s,
                work.id,
                "../x",
                "x".into(),
                event("u", None),
                Precondition::Any
            )
            .await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        store
            .put_object(
                s,
                work.id,
                "big.ics",
                "x".repeat(LIMITS.max_object_size + 1),
                event("uid-big", None),
                Precondition::Any
            )
            .await,
        Err(Error::QuotaExceeded)
    );
    assert_eq!(
        store
            .put_object(
                s,
                CollectionId::new(),
                "a.ics",
                "x".into(),
                event("u", None),
                Precondition::Any
            )
            .await,
        Err(Error::NotFound)
    );
    // UID уникален в коллекции, не в учётной записи; vCard — в адресной книге.
    stored(
        put(
            store,
            s,
            home.id,
            "a.ics",
            event("uid-a", Some((30, None))),
            Precondition::Any,
        )
        .await,
    );
    stored(put(store, s, book.id, "c.vcf", card, Precondition::Any).await);

    // Лимит объектов в коллекции.
    let b = stored(
        put(
            store,
            s,
            work.id,
            "b.ics",
            event("uid-b", Some((20, Some(21)))),
            Precondition::Any,
        )
        .await,
    );
    let t = stored(
        put(
            store,
            s,
            work.id,
            "t.ics",
            event("uid-t", None),
            Precondition::Any,
        )
        .await,
    );
    let d = stored(
        put(
            store,
            s,
            work.id,
            "d.ics",
            event("uid-d", Some((0, None))),
            Precondition::Any,
        )
        .await,
    );
    assert_eq!(
        store
            .put_object(
                s,
                work.id,
                "e.ics",
                "x".into(),
                event("uid-e", None),
                Precondition::Any
            )
            .await,
        Err(Error::QuotaExceeded),
        "объектов не больше max_objects"
    );
    stored(
        put(
            store,
            s,
            work.id,
            "d.ics",
            event("uid-d", Some((0, None))),
            Precondition::Any,
        )
        .await,
    );

    // ---------- Чтение ----------
    let list = store.list_objects(s, work.id).await.expect("list");
    let listed: Vec<&str> = list.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(listed, ["a.ics", "b.ics", "d.ics", "t.ics"]);
    assert_eq!(list[0].etag, a2.etag);
    let by_uid = store
        .object_by_uid(s, work.id, "uid-b")
        .await
        .expect("by_uid")
        .expect("b");
    assert_eq!(by_uid.name, "b.ics");
    assert_eq!(
        store
            .object_by_uid(s, book.id, "uid-b")
            .await
            .expect("by_uid"),
        None
    );
    let mg = store
        .multiget(
            s,
            work.id,
            &["t.ics".into(), "none.ics".into(), "a.ics".into()],
        )
        .await
        .expect("multiget");
    assert_eq!(names(&mg), ["t.ics", "a.ics"]);
    let by_id = store
        .objects_by_id(s, &[b.id, tp_contracts::dav::DavObjectId::new(), a.id])
        .await
        .expect("by_id");
    assert_eq!(names(&by_id), ["b.ics", "a.ics"]);
    let range = |s0: i64, e0: i64| TimeRange {
        start: Some(at(s0)),
        end: Some(at(e0)),
    };
    let mut cand = names(
        &store
            .in_range(s, work.id, range(11, 15))
            .await
            .expect("range"),
    )
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    cand.sort();
    assert_eq!(
        cand,
        ["a.ics", "d.ics", "t.ics"],
        "пересечение, бесконечный, без дат"
    );
    let mut cand: Vec<String> = store
        .in_range(s, work.id, range(21, 30))
        .await
        .expect("range")
        .into_iter()
        .map(|o| o.name)
        .collect();
    cand.sort();
    assert_eq!(
        cand,
        ["b.ics", "d.ics", "t.ics"],
        "конец на начале интервала — кандидат"
    );

    // ---------- Удаление объекта ----------
    assert_eq!(
        store
            .delete_object(s, work.id, "none.ics", Precondition::Any)
            .await,
        Err(Error::NotFound)
    );
    assert_eq!(
        store
            .delete_object(s, work.id, "t.ics", Precondition::IfMatch("nope".into()))
            .await,
        Ok(DeleteOutcome::PreconditionFailed)
    );
    assert_eq!(
        store
            .delete_object(s, work.id, "t.ics", Precondition::IfNoneMatch)
            .await,
        Ok(DeleteOutcome::PreconditionFailed)
    );
    let DeleteOutcome::Deleted(m_del_t) = store
        .delete_object(s, work.id, "t.ics", Precondition::IfMatch(t.etag.clone()))
        .await
        .expect("delete")
    else {
        panic!("t.ics не удалён");
    };
    assert!(
        store
            .get_object(s, work.id, "t.ics")
            .await
            .expect("get")
            .is_none()
    );

    // ---------- Изменения коллекции ----------
    let full = store
        .changes(s, work.id, ModSeq::ZERO, 100)
        .await
        .expect("changes");
    let mut created = full.created.clone();
    created.sort();
    assert_eq!(created, ["a.ics", "b.ics", "d.ics"]);
    assert!(full.updated.is_empty() && full.destroyed.is_empty() && !full.has_more);
    let work_now = store
        .get_collection(s, work.id)
        .await
        .expect("get")
        .expect("work");
    assert_eq!(full.new_state, work_now.state);
    assert_eq!(work_now.state, m_del_t);

    // После `a2`: b, t (удалён), d — новые; t в destroyed.
    let ch = store
        .changes(s, work.id, a2.modseq, 100)
        .await
        .expect("changes");
    let mut created = ch.created.clone();
    created.sort();
    assert_eq!(created, ["b.ics", "d.ics"]);
    assert!(ch.updated.is_empty());
    assert_eq!(ch.destroyed, ["t.ics"]);
    // После `d`: d перезаписан — updated; t — destroyed.
    let ch = store
        .changes(s, work.id, d.modseq, 100)
        .await
        .expect("changes");
    assert!(ch.created.is_empty());
    assert_eq!(ch.updated, ["d.ics"]);
    assert_eq!(ch.destroyed, ["t.ics"]);

    // Повторное создание имени: не в destroyed.
    let t2 = stored(
        put(
            store,
            s,
            work.id,
            "t.ics",
            event("uid-t2", None),
            Precondition::IfNoneMatch,
        )
        .await,
    );
    assert_ne!(t2.id, t.id, "новый ресурс — новый идентификатор");
    let ch = store
        .changes(s, work.id, d.modseq, 100)
        .await
        .expect("changes");
    assert_eq!(ch.updated, ["d.ics"]);
    assert_eq!(ch.created, ["t.ics"], "создан после since");
    assert!(ch.destroyed.is_empty());
    let ch = store
        .changes(s, work.id, t2.modseq, 100)
        .await
        .expect("changes");
    assert!(ch.created.is_empty() && ch.updated.is_empty() && ch.destroyed.is_empty());
    assert_eq!(ch.new_state, t2.modseq);

    // Страницы по одному изменению: порядок по ModSeq, has_more, без потерь.
    let mut since = a.modseq;
    let mut seen = Vec::new();
    for _ in 0..10 {
        let p = store.changes(s, work.id, since, 1).await.expect("page");
        assert!(p.created.len() + p.updated.len() + p.destroyed.len() <= 1);
        seen.extend(p.created.into_iter().chain(p.updated).chain(p.destroyed));
        assert!(p.new_state >= since);
        since = p.new_state;
        if !p.has_more {
            break;
        }
    }
    assert_eq!(seen, ["a.ics", "b.ics", "d.ics", "t.ics"]);
    assert_eq!(since, t2.modseq);
    assert_eq!(
        store.changes(s, work.id, ModSeq(i64::MAX as u64), 10).await,
        Err(Error::Conflict),
        "since из будущего"
    );
    assert_eq!(
        store
            .changes(s, CollectionId::new(), ModSeq::ZERO, 10)
            .await,
        Err(Error::NotFound)
    );

    // ---------- Изменения учётной записи ----------
    let cal_state = store
        .state(s, CollectionKind::Calendar)
        .await
        .expect("state");
    assert_eq!(cal_state, t2.modseq);
    let acc = store
        .account_changes(s, CollectionKind::Calendar, a.modseq, 100)
        .await
        .expect("account_changes");
    let mut created = acc.created.clone();
    created.sort();
    let mut want = vec![b.id, d.id, t2.id];
    let home_a = store
        .get_object(s, home.id, "a.ics")
        .await
        .expect("get")
        .expect("home a");
    want.push(home_a.id);
    want.sort();
    assert_eq!(created, want);
    assert_eq!(acc.updated, [a.id]);
    assert_eq!(acc.destroyed, [t.id]);
    assert_eq!(acc.new_state, cal_state);
    let book_acc = store
        .account_changes(s, CollectionKind::AddressBook, ModSeq::ZERO, 100)
        .await
        .expect("book changes");
    assert_eq!(book_acc.created.len(), 1);
    assert_eq!(
        book_acc.new_state,
        store
            .state(s, CollectionKind::AddressBook)
            .await
            .expect("state")
    );

    let cc = store
        .collection_changes(s, CollectionKind::Calendar, ModSeq::ZERO, 100)
        .await
        .expect("collection_changes");
    let mut ids = cc.created.clone();
    ids.sort();
    let mut want = vec![work.id, home.id];
    want.sort();
    assert_eq!(ids, want);
    let cc = store
        .collection_changes(s, CollectionKind::Calendar, m_book, 100)
        .await
        .expect("collection_changes");
    assert!(cc.created.is_empty());
    assert_eq!(
        cc.updated,
        [work.id],
        "свойства изменены, объекты не учитываются"
    );
    let cc = store
        .collection_changes(s, CollectionKind::Calendar, m_upd, 100)
        .await
        .expect("collection_changes");
    assert!(cc.created.is_empty() && cc.updated.is_empty() && cc.destroyed.is_empty());
    assert_eq!(cc.new_state, cal_state);

    // ---------- Удаление коллекции ----------
    let before = store
        .state(s, CollectionKind::Calendar)
        .await
        .expect("state");
    let m_gone = store.delete_collection(s, work.id).await.expect("delete");
    assert!(m_gone > before);
    assert_eq!(
        store.delete_collection(s, work.id).await,
        Err(Error::NotFound)
    );
    assert_eq!(store.get_collection(s, work.id).await.expect("get"), None);
    assert_eq!(
        store.get_object(s, work.id, "a.ics").await,
        Err(Error::NotFound)
    );
    assert_eq!(
        store.changes(s, work.id, before, 10).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        store
            .state(s, CollectionKind::Calendar)
            .await
            .expect("state"),
        m_gone
    );
    let acc = store
        .account_changes(s, CollectionKind::Calendar, before, 100)
        .await
        .expect("account_changes");
    let mut gone = acc.destroyed.clone();
    gone.sort();
    let mut want = vec![a.id, b.id, d.id, t2.id];
    want.sort();
    assert_eq!(gone, want, "объекты удалённой коллекции — надгробия");
    assert!(acc.created.is_empty() && acc.updated.is_empty());
    assert_eq!(acc.new_state, m_gone);
    // Страницы account_changes по одному: каждое надгробие — свой ModSeq.
    let mut since = before;
    let mut n = 0;
    loop {
        let p = store
            .account_changes(s, CollectionKind::Calendar, since, 1)
            .await
            .expect("page");
        n += p.destroyed.len();
        assert!(p.created.len() + p.updated.len() + p.destroyed.len() <= 1);
        since = p.new_state;
        if !p.has_more {
            break;
        }
    }
    assert_eq!((n, since), (4, m_gone));
    let cc = store
        .collection_changes(s, CollectionKind::Calendar, before, 100)
        .await
        .expect("collection_changes");
    assert_eq!(cc.destroyed, [work.id]);
    assert!(cc.created.is_empty() && cc.updated.is_empty());
    assert_eq!(
        store
            .list_collections(s, CollectionKind::Calendar)
            .await
            .expect("list")
            .len(),
        1
    );
    // Место освободилось: коллекций снова меньше лимита.
    store
        .create_collection(s, Collection::new(CollectionKind::Calendar, "work"))
        .await
        .expect("снова work");

    // ---------- Разделение учётных записей и организаций ----------
    for other in [fx.peer, fx.foreign] {
        for kind in [CollectionKind::Calendar, CollectionKind::AddressBook] {
            assert!(
                store
                    .list_collections(other, kind)
                    .await
                    .expect("list")
                    .is_empty(),
                "{other:?}"
            );
            assert_eq!(store.state(other, kind).await.expect("state"), ModSeq::ZERO);
            let ch = store
                .account_changes(other, kind, ModSeq::ZERO, 10)
                .await
                .expect("changes");
            assert!(ch.created.is_empty());
        }
        assert_eq!(
            store.get_collection(other, home.id).await.expect("get"),
            None
        );
        assert_eq!(
            store
                .collection_by_name(other, CollectionKind::Calendar, "home")
                .await
                .expect("by_name"),
            None
        );
        assert_eq!(
            store.get_object(other, home.id, "a.ics").await,
            Err(Error::NotFound)
        );
        assert_eq!(
            store
                .put_object(
                    other,
                    home.id,
                    "x.ics",
                    "x".into(),
                    event("x", None),
                    Precondition::Any
                )
                .await,
            Err(Error::NotFound)
        );
        assert_eq!(
            store
                .delete_object(other, home.id, "a.ics", Precondition::Any)
                .await,
            Err(Error::NotFound)
        );
        assert_eq!(
            store
                .objects_by_id(other, &[home_a.id])
                .await
                .expect("by_id"),
            Vec::new()
        );
        assert_eq!(
            store.changes(other, home.id, ModSeq::ZERO, 10).await,
            Err(Error::NotFound)
        );
        assert_eq!(
            store.delete_collection(other, home.id).await,
            Err(Error::NotFound)
        );
    }
    // Чужие операции ничего не изменили.
    assert!(
        store
            .get_object(s, home.id, "a.ics")
            .await
            .expect("get")
            .is_some()
    );
}

/// Набор проверок сигналов [`DavStore`] (CTR-45): после создания коллекции, записи и удаления объекта, удаления
/// коллекции подписке учётной записи `scope` приходит `DavChanged` с видом, коллекцией и `ModSeq` изменения.
/// `scope` — учётная запись без коллекций.
pub async fn signals(store: &dyn DavStore, bus: &dyn EventBus, scope: AccountScope) {
    let mut events = bus.subscribe(scope).await.expect("subscribe");
    let mut expect = async |collection: CollectionId, modseq: ModSeq| {
        let e = next_signal(&mut events).await.expect("сигнал");
        assert_eq!((e.tenant, e.account), (scope.tenant, Some(scope.account)));
        assert_eq!(
            e.kind,
            EventKind::DavChanged {
                kind: CollectionKind::AddressBook,
                collection,
                modseq
            }
        );
    };
    let book = Collection::new(CollectionKind::AddressBook, "contacts");
    let m = store
        .create_collection(scope, book.clone())
        .await
        .expect("create");
    expect(book.id, m).await;
    let card = ObjectIndex::new(Component::Card, "uid-1", None);
    let p = stored(
        store
            .put_object(
                scope,
                book.id,
                "1.vcf",
                "BEGIN:VCARD\r\nEND:VCARD\r\n".into(),
                card,
                Precondition::Any,
            )
            .await
            .expect("put"),
    );
    expect(book.id, p.modseq).await;
    let DeleteOutcome::Deleted(m) = store
        .delete_object(scope, book.id, "1.vcf", Precondition::Any)
        .await
        .expect("delete")
    else {
        panic!("не удалён");
    };
    expect(book.id, m).await;
    let m = store
        .delete_collection(scope, book.id)
        .await
        .expect("delete");
    expect(book.id, m).await;
}

/// Набор проверок [`DavStore`] (CTR-54, CTR-56): коллекция по умолчанию (признак не больше чем у одной на вид),
/// подписка, видимость и оповещения по умолчанию; перенос объекта между коллекциями (идентификатор сохраняется,
/// изменения обеих коллекций, конфликт UID, занятое имя); поиск контактов по поисковым строкам индекса.
/// `s` — учётная запись без коллекций; хватает лимитов [`LIMITS`].
pub async fn organize(store: &dyn DavStore, s: AccountScope) {
    let cal = CollectionKind::Calendar;
    let list = async || store.list_collections(s, cal).await.expect("list");
    let get = async |id: CollectionId| {
        store
            .get_collection(s, id)
            .await
            .expect("get")
            .expect("коллекция")
    };

    // ---------- Коллекция по умолчанию и свойства (CTR-56) ----------
    let work = Collection::new(cal, "work");
    let home = Collection {
        order: 1,
        ..Collection::new(cal, "home")
    };
    for c in [&work, &home] {
        store.create_collection(s, c.clone()).await.expect("create");
    }
    // Не назначена — прежнее правило: первая по порядку.
    let all = list().await;
    assert!(
        all.iter()
            .all(|c| !c.is_default && c.subscribed && c.visible)
    );
    assert_eq!(default_of(&all).map(|c| c.id), Some(work.id));

    let alerts =
        r#"{"a1":{"@type":"Alert","trigger":{"@type":"OffsetTrigger","offset":"-PT15M"}}}"#;
    let home_set = Collection {
        is_default: true,
        subscribed: false,
        visible: false,
        alerts_with_time: Some(alerts.into()),
        alerts_without_time: Some("{}".into()),
        ..home.clone()
    };
    let m_home = store
        .update_collection(s, home_set.clone())
        .await
        .expect("update");
    // Оповещения, подписка и видимость сохраняются и читаются.
    assert_eq!(
        get(home.id).await,
        Collection {
            state: m_home,
            ..home_set.clone()
        }
    );
    assert_eq!(default_of(&list().await).map(|c| c.id), Some(home.id));

    // Смена коллекции по умолчанию: признак ровно у одной; у прежней — своё изменение свойств раньше новой.
    let before = store.state(s, cal).await.expect("state");
    let m_work = store
        .update_collection(
            s,
            Collection {
                is_default: true,
                ..work.clone()
            },
        )
        .await
        .expect("update");
    let all = list().await;
    assert_eq!(
        all.iter()
            .filter(|c| c.is_default)
            .map(|c| c.id)
            .collect::<Vec<_>>(),
        [work.id]
    );
    let was = get(home.id).await;
    assert!(before < was.state && was.state < m_work, "{was:?}");
    assert_eq!(
        was,
        Collection {
            is_default: false,
            state: was.state,
            ..home_set.clone()
        }
    );
    let ch = store
        .collection_changes(s, cal, before, 10)
        .await
        .expect("changes");
    assert_eq!(ch.updated, [home.id, work.id]);
    assert_eq!(ch.new_state, m_work);

    // Признак — по видам независимо; создание с признаком; оповещения — только у календаря.
    let bad = Collection {
        alerts_with_time: Some(alerts.into()),
        ..Collection::new(CollectionKind::AddressBook, "bad")
    };
    assert!(matches!(
        store.create_collection(s, bad).await,
        Err(Error::Invalid(_))
    ));
    let book = Collection {
        is_default: true,
        ..Collection::new(CollectionKind::AddressBook, "book")
    };
    store
        .create_collection(s, book.clone())
        .await
        .expect("create");
    assert!(get(book.id).await.is_default);
    assert!(get(work.id).await.is_default);
    // Снятие признака: коллекция по умолчанию не назначена.
    store
        .update_collection(s, work.clone())
        .await
        .expect("update");
    assert!(list().await.iter().all(|c| !c.is_default));

    // ---------- Перенос (CTR-54) ----------
    let e1 = stored(
        put(
            store,
            s,
            work.id,
            "e1.ics",
            event("u1", Some((1, Some(2)))),
            Precondition::Any,
        )
        .await,
    );
    for (coll, name, uid) in [
        (work.id, "e2.ics", "u2"),
        (home.id, "x.ics", "u1"),
        (home.id, "e2.ics", "u9"),
    ] {
        stored(put(store, s, coll, name, event(uid, None), Precondition::Any).await);
    }
    let mv = async |from: CollectionId, name: &'static str, to: CollectionId, pre: Precondition| {
        store.move_object(s, from, name, to, pre).await
    };
    assert_eq!(
        mv(work.id, "e1.ics", home.id, Precondition::Any).await,
        Ok(MoveOutcome::UidConflict {
            name: "x.ics".into()
        })
    );
    assert_eq!(
        mv(work.id, "e2.ics", home.id, Precondition::Any).await,
        Ok(MoveOutcome::PreconditionFailed)
    );
    assert_eq!(
        mv(
            work.id,
            "e1.ics",
            home.id,
            Precondition::IfMatch("other".into())
        )
        .await,
        Ok(MoveOutcome::PreconditionFailed)
    );
    assert_eq!(
        mv(work.id, "none.ics", home.id, Precondition::Any).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        mv(work.id, "e1.ics", CollectionId::new(), Precondition::Any).await,
        Err(Error::NotFound)
    );
    for to in [work.id, book.id] {
        assert!(matches!(
            mv(work.id, "e1.ics", to, Precondition::Any).await,
            Err(Error::Invalid(_))
        ));
    }
    store
        .delete_object(s, home.id, "x.ics", Precondition::Any)
        .await
        .expect("delete");
    let since = store.state(s, cal).await.expect("state");
    let source = store
        .get_object(s, work.id, "e1.ics")
        .await
        .expect("get")
        .expect("объект");
    let MoveOutcome::Moved(moved) = mv(
        work.id,
        "e1.ics",
        home.id,
        Precondition::IfMatch(e1.etag.clone()),
    )
    .await
    .expect("move") else {
        panic!("не перенесён");
    };
    // Идентификатор, тело и индекс сохраняются; ETag и ModSeq — новые.
    assert_eq!(moved.id, e1.id);
    assert_ne!(moved.etag, e1.etag);
    assert!(since < moved.removed && moved.removed < moved.modseq);
    let target = store
        .get_object(s, home.id, "e1.ics")
        .await
        .expect("get")
        .expect("объект");
    assert_eq!(
        target,
        tp_contracts::dav::DavObject {
            collection: home.id,
            etag: moved.etag.clone(),
            ..source
        }
    );
    assert_eq!(store.get_object(s, work.id, "e1.ics").await, Ok(None));
    assert_eq!(get(work.id).await.state, moved.removed);
    assert_eq!(get(home.id).await.state, moved.modseq);
    // Виден в изменениях обеих коллекций; в изменениях учётной записи — обновление, не удаление.
    let ch = store.changes(s, work.id, since, 10).await.expect("changes");
    assert_eq!(
        (ch.created.len(), ch.updated.len(), ch.destroyed),
        (0, 0, vec!["e1.ics".to_owned()])
    );
    let ch = store.changes(s, home.id, since, 10).await.expect("changes");
    assert_eq!(
        (ch.created, ch.updated.len(), ch.destroyed.len()),
        (vec!["e1.ics".to_owned()], 0, 0)
    );
    let ch = store
        .account_changes(s, cal, since, 10)
        .await
        .expect("changes");
    assert_eq!(
        (ch.created.len(), ch.updated, ch.destroyed.len()),
        (0, vec![e1.id], 0)
    );
    assert_eq!(ch.new_state, moved.modseq);
    // Лимит объектов целевой коллекции.
    let max = store.limits().max_objects;
    if max <= 8 {
        for i in 2..max {
            let (name, uid) = (format!("f{i}.ics"), format!("f{i}"));
            stored(
                put(
                    store,
                    s,
                    home.id,
                    &name,
                    event(&uid, None),
                    Precondition::Any,
                )
                .await,
            );
        }
        assert_eq!(
            mv(work.id, "e2.ics", home.id, Precondition::Any).await,
            Ok(MoveOutcome::PreconditionFailed)
        );
        stored(
            put(
                store,
                s,
                work.id,
                "e3.ics",
                event("u3", None),
                Precondition::Any,
            )
            .await,
        );
        assert_eq!(
            mv(work.id, "e3.ics", home.id, Precondition::Any).await,
            Err(Error::QuotaExceeded)
        );
    }

    // ---------- Поиск контактов (CTR-54) ----------
    let card = |uid: &str, search: &[&str]| ObjectIndex {
        search: CardSearch::new(search),
        ..ObjectIndex::new(Component::Card, uid, None)
    };
    let put_card = async |name: &str, index: ObjectIndex| {
        stored(
            store
                .put_object(
                    s,
                    book.id,
                    name,
                    "BEGIN:VCARD\r\nEND:VCARD\r\n".into(),
                    index,
                    Precondition::Any,
                )
                .await
                .expect("put"),
        )
    };
    put_card(
        "a.vcf",
        card("c1", &["Анна Петрова", "Петрова", "ann@example.com"]),
    )
    .await;
    put_card("b.vcf", card("c2", &["Борис 100%_x", "boris@example.com"])).await;
    put_card("c.vcf", card("c3", &[])).await;
    let find = async |needle: &str, limit: usize| {
        let found = store
            .find_cards(s, book.id, needle, limit)
            .await
            .expect("find");
        found.into_iter().map(|o| o.name).collect::<Vec<_>>()
    };
    // По части имени и по адресу, без учёта регистра; пустой запрос — все; по имени ресурса, не более limit.
    assert_eq!(find("ПЕТР", 10).await, ["a.vcf"]);
    assert_eq!(find("example.com", 10).await, ["a.vcf", "b.vcf"]);
    assert_eq!(find("example.com", 1).await, ["a.vcf"]);
    assert_eq!(find("", 10).await, ["a.vcf", "b.vcf", "c.vcf"]);
    assert_eq!(find("100%_", 10).await, ["b.vcf"]);
    assert_eq!(find("%", 10).await, ["b.vcf"]);
    assert!(find("иван", 10).await.is_empty());
    assert_eq!(
        store.find_cards(s, CollectionId::new(), "a", 10).await,
        Err(Error::NotFound)
    );
    // Строки читаются с объектом и заменяются при записи.
    let read = store
        .get_object(s, book.id, "a.vcf")
        .await
        .expect("get")
        .expect("объект");
    assert_eq!(
        read.index.search.strings,
        ["анна петрова", "петрова", "ann@example.com"]
    );
    put_card("a.vcf", card("c1", &["Анна Иванова"])).await;
    assert!(find("петр", 10).await.is_empty());
    assert_eq!(find("иван", 10).await, ["a.vcf"]);
    // Поисковые строки — только у контакта.
    let bad = ObjectIndex {
        search: CardSearch::new(["x"]),
        ..event("u7", None)
    };
    assert!(matches!(
        store
            .put_object(s, work.id, "bad.ics", body("u7"), bad, Precondition::Any)
            .await,
        Err(Error::Invalid(_))
    ));
}

// ---------- Книга GAL (CTR-60) ----------

fn gal_card(name: &str, uid: &str, full_name: &str) -> GalCard {
    GalCard {
        name: name.to_owned(),
        body: format!("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:{full_name}\r\nEND:VCARD\r\n"),
        index: ObjectIndex {
            search: CardSearch::new([full_name]),
            ..ObjectIndex::new(Component::Card, uid, None)
        },
    }
}

/// Набор проверок книги GAL [`DavStore`] (CTR-60): до первой [`DavStore::sync_gal`] книги нет, имя
/// [`GAL_COLLECTION_NAME`] зарезервировано; книга видна каждой учётной записи организации и не видна другой, не книга
/// по умолчанию; чтение — как у обычной книги; повтор набора без изменений не меняет ETag, `ModSeq` и `changes`;
/// изменённая карточка — новый ETag с прежним идентификатором, исчезнувшая — надгробие (`changes`, `gal_changes`);
/// запись пользователя — `Forbidden`; изменения и состояние учётной записи книгу не включают; отображаемое имя;
/// недопустимый набор ничего не меняет. `fx.owner`, `fx.peer` — без коллекций, у организации книги GAL нет;
/// хранилище — с лимитами [`LIMITS`].
pub async fn gal(store: &dyn DavStore, fx: &DavFixture) {
    let (s, peer, tenant) = (fx.owner, fx.peer, fx.owner.tenant);
    let book = CollectionKind::AddressBook;
    assert_eq!(store.limits(), LIMITS, "набор требует лимиты LIMITS");

    // До первой записи книги нет; имя зарезервировано.
    assert!(
        store
            .list_collections(s, book)
            .await
            .expect("list")
            .is_empty()
    );
    let none = store
        .gal_changes(tenant, ModSeq::ZERO, 10)
        .await
        .expect("gal_changes");
    assert!(none.created.is_empty() && !none.has_more);
    assert_eq!(none.new_state, ModSeq::ZERO);
    assert!(matches!(
        store.gal_changes(tenant, ModSeq(1), 10).await,
        Err(Error::Conflict)
    ));
    assert_eq!(
        store
            .create_collection(s, Collection::new(book, GAL_COLLECTION_NAME))
            .await,
        Err(Error::Conflict)
    );

    // ---------- Первая запись ----------
    let a = gal_card("a.vcf", "uid-a", "Анна Петрова");
    let b = gal_card("b.vcf", "uid-b", "Борис Иванов");
    let m1 = store
        .sync_gal(tenant, "Организация", vec![a.clone(), b.clone()])
        .await
        .expect("sync_gal");
    let gal = store
        .collection_by_name(s, book, GAL_COLLECTION_NAME)
        .await
        .expect("collection_by_name")
        .expect("книга GAL");
    assert!(gal.is_gal());
    assert_eq!((gal.display_name.as_str(), gal.state), ("Организация", m1));
    for scope in [s, peer] {
        let list = store.list_collections(scope, book).await.expect("list");
        assert_eq!(
            list,
            std::slice::from_ref(&gal),
            "книга видна каждой учётной записи"
        );
        assert_eq!(default_of(&list), None, "книга GAL — не книга по умолчанию");
        assert_eq!(
            store.get_collection(scope, gal.id).await.expect("get"),
            Some(gal.clone())
        );
    }
    let foreign = fx.foreign;
    assert!(
        store
            .list_collections(foreign, book)
            .await
            .expect("list")
            .is_empty()
    );
    assert_eq!(store.get_collection(foreign, gal.id).await, Ok(None));
    assert_eq!(
        store.list_objects(foreign, gal.id).await,
        Err(Error::NotFound)
    );

    // Чтение — как у обычной книги.
    let refs = store
        .list_objects(peer, gal.id)
        .await
        .expect("list_objects");
    assert_eq!(
        refs.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["a.vcf", "b.vcf"]
    );
    let (id_a, id_b) = (refs[0].id, refs[1].id);
    let got = store
        .get_object(peer, gal.id, "a.vcf")
        .await
        .expect("get_object")
        .expect("карточка a");
    assert_eq!((&got.body, &got.index), (&a.body, &a.index));
    assert_eq!(
        names(&store.find_cards(s, gal.id, "петр", 10).await.expect("find")),
        ["a.vcf"]
    );
    assert_eq!(
        names(&store.objects_by_id(peer, &[id_b]).await.expect("by id")),
        ["b.vcf"]
    );
    let full = store
        .changes(s, gal.id, ModSeq::ZERO, 10)
        .await
        .expect("changes");
    assert_eq!(full.created, ["a.vcf", "b.vcf"]);
    assert_eq!(full.new_state, m1);
    let full = store
        .gal_changes(tenant, ModSeq::ZERO, 10)
        .await
        .expect("gal_changes");
    assert_eq!((full.created, full.new_state), (vec![id_a, id_b], m1));

    // ---------- Повтор без изменений ----------
    assert_eq!(
        store
            .sync_gal(tenant, "Организация", vec![b.clone(), a.clone()])
            .await,
        Ok(m1)
    );
    assert_eq!(store.list_objects(s, gal.id).await.expect("list"), refs);
    let same = store.changes(s, gal.id, m1, 10).await.expect("changes");
    assert!(same.created.is_empty() && same.updated.is_empty() && same.destroyed.is_empty());
    assert_eq!(same.new_state, m1);
    let same = store
        .gal_changes(tenant, m1, 10)
        .await
        .expect("gal_changes");
    assert!(same.created.is_empty() && same.updated.is_empty() && same.destroyed.is_empty());

    // ---------- Изменённая, исчезнувшая, новая ----------
    let a2 = gal_card("a.vcf", "uid-a", "Анна Сидорова");
    let c = gal_card("c.vcf", "uid-c", "Вера Смирнова");
    let m2 = store
        .sync_gal(tenant, "Организация", vec![a2.clone(), c.clone()])
        .await
        .expect("sync_gal");
    assert!(m2 > m1);
    let refs2 = store.list_objects(s, gal.id).await.expect("list");
    assert_eq!(
        refs2.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
        ["a.vcf", "c.vcf"]
    );
    assert_eq!(refs2[0].id, id_a, "карточка с прежним именем — прежний id");
    assert_ne!(refs2[0].etag, refs[0].etag, "изменённая — новый ETag");
    let id_c = refs2[1].id;
    let ch = store.changes(s, gal.id, m1, 10).await.expect("changes");
    assert_eq!(
        (ch.created, ch.updated, ch.destroyed, ch.new_state),
        (
            vec!["c.vcf".to_owned()],
            vec!["a.vcf".to_owned()],
            vec!["b.vcf".to_owned()],
            m2
        )
    );
    let ch = store
        .gal_changes(tenant, m1, 10)
        .await
        .expect("gal_changes");
    assert_eq!(
        (ch.created, ch.updated, ch.destroyed, ch.new_state),
        (vec![id_c], vec![id_a], vec![id_b], m2)
    );
    let page = store.gal_changes(tenant, m1, 1).await.expect("gal_changes");
    assert!(page.has_more && page.new_state < m2);
    assert_eq!(
        store
            .get_collection(s, gal.id)
            .await
            .expect("get")
            .map(|g| g.state),
        Some(m2)
    );

    // ---------- Запись пользователя — Forbidden ----------
    let own = Collection::new(book, "contacts");
    store
        .create_collection(s, own.clone())
        .await
        .expect("create");
    let x = gal_card("x.vcf", "uid-x", "Свой Контакт");
    let put_x = stored(
        store
            .put_object(
                s,
                own.id,
                "x.vcf",
                x.body.clone(),
                x.index.clone(),
                Precondition::Any,
            )
            .await
            .expect("put"),
    );
    assert_eq!(
        store
            .put_object(
                s,
                gal.id,
                "x.vcf",
                x.body.clone(),
                x.index.clone(),
                Precondition::Any
            )
            .await,
        Err(Error::Forbidden)
    );
    assert_eq!(
        store
            .delete_object(s, gal.id, "a.vcf", Precondition::Any)
            .await,
        Err(Error::Forbidden)
    );
    let renamed = Collection {
        display_name: "Моя".into(),
        ..gal.clone()
    };
    assert_eq!(
        store.update_collection(s, renamed).await,
        Err(Error::Forbidden)
    );
    assert_eq!(
        store.delete_collection(s, gal.id).await,
        Err(Error::Forbidden)
    );
    for (from, name, to) in [(own.id, "x.vcf", gal.id), (gal.id, "a.vcf", own.id)] {
        assert_eq!(
            store
                .move_object(s, from, name, to, Precondition::Any)
                .await,
            Err(Error::Forbidden)
        );
    }
    assert_eq!(store.list_objects(s, gal.id).await.expect("list"), refs2);

    // ---------- Учётная запись: изменения и состояние без книги GAL ----------
    let list = store.list_collections(s, book).await.expect("list");
    assert_eq!(list.len(), 2);
    assert_eq!(default_of(&list).map(|c| c.id), Some(own.id));
    let acc = store
        .account_changes(s, book, ModSeq::ZERO, 10)
        .await
        .expect("account_changes");
    assert_eq!(acc.created, [put_x.id]);
    let colls = store
        .collection_changes(s, book, ModSeq::ZERO, 10)
        .await
        .expect("collection_changes");
    assert_eq!(colls.created, [own.id]);
    assert_eq!(store.state(s, book).await, Ok(put_x.modseq));
    assert_eq!(store.state(peer, book).await, Ok(ModSeq::ZERO));

    // ---------- Отображаемое имя ----------
    let m3 = store
        .sync_gal(tenant, "Компания", vec![a2.clone(), c.clone()])
        .await
        .expect("sync_gal");
    assert!(m3 > m2);
    let renamed = store
        .get_collection(peer, gal.id)
        .await
        .expect("get")
        .expect("книга GAL");
    assert_eq!(
        (renamed.display_name.as_str(), renamed.state),
        ("Компания", m3)
    );
    let ch = store.changes(s, gal.id, m2, 10).await.expect("changes");
    assert!(ch.created.is_empty() && ch.updated.is_empty() && ch.destroyed.is_empty());
    assert_eq!(ch.new_state, m3);

    // ---------- Недопустимый набор — ничего не изменено ----------
    let event = GalCard {
        index: ObjectIndex::new(Component::Event, "uid-e", None),
        ..gal_card("e.ics", "uid-e", "Событие")
    };
    let invalid = [
        vec![a2.clone(), a2.clone()],
        vec![
            a2.clone(),
            GalCard {
                name: "d.vcf".into(),
                ..a2.clone()
            },
        ],
        vec![event],
        vec![GalCard {
            name: "a/b".into(),
            ..a2.clone()
        }],
    ];
    for cards in invalid {
        assert!(
            matches!(
                store.sync_gal(tenant, "Компания", cards).await,
                Err(Error::Invalid(_))
            ),
            "недопустимый набор"
        );
    }
    assert!(matches!(
        store.sync_gal(tenant, "a\u{7}", vec![a2.clone()]).await,
        Err(Error::Invalid(_))
    ));
    let many: Vec<GalCard> = (0..=LIMITS.max_objects)
        .map(|i| gal_card(&format!("{i}.vcf"), &format!("uid-{i}"), "Имя"))
        .collect();
    let big = GalCard {
        body: "x".repeat(LIMITS.max_object_size + 1),
        ..a2.clone()
    };
    for cards in [many, vec![big]] {
        assert_eq!(
            store.sync_gal(tenant, "Компания", cards).await,
            Err(Error::QuotaExceeded)
        );
    }
    assert_eq!(store.list_objects(s, gal.id).await.expect("list"), refs2);
    assert_eq!(
        store.get_collection(s, gal.id).await.expect("get"),
        Some(renamed)
    );
}

/// Набор проверок сигнала книги GAL (CTR-60): [`DavStore::sync_gal`], изменивший книгу, публикует системный
/// `GalChanged` с `ModSeq` книги; повтор без изменений — без сигнала. «Не пришло» проверяется тем, что первым пришёл
/// сигнал следующей записи. У организации `tenant` книги GAL нет.
pub async fn gal_signals(store: &dyn DavStore, bus: &dyn EventBus, tenant: TenantId) {
    let mut sys = bus.subscribe_system().await.expect("subscribe_system");
    let scope = AccountScope {
        tenant,
        account: AccountId::new(),
    };
    let a = gal_card("a.vcf", "uid-a", "Анна Петрова");
    let m1 = store
        .sync_gal(tenant, "Организация", vec![a.clone()])
        .await
        .expect("sync_gal");
    let gal = store
        .collection_by_name(scope, CollectionKind::AddressBook, GAL_COLLECTION_NAME)
        .await
        .expect("collection_by_name")
        .expect("книга GAL");
    let mut expect = async |modseq: ModSeq| {
        let e = next_signal(&mut sys).await.expect("сигнал");
        assert_eq!((e.tenant, e.account), (tenant, None));
        assert_eq!(
            e.kind,
            EventKind::GalChanged {
                collection: gal.id,
                modseq
            }
        );
    };
    expect(m1).await;
    assert_eq!(store.sync_gal(tenant, "Организация", vec![a]).await, Ok(m1));
    let m2 = store
        .sync_gal(tenant, "Организация", Vec::new())
        .await
        .expect("sync_gal");
    expect(m2).await;
}
