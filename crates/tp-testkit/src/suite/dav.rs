//! Набор проверок [`DavStore`] (CTR-45): in-memory `MemDav` и `PgMeta` (META-47) проходят одни и те же проверки.

use std::collections::BTreeSet;

use time::{Duration, OffsetDateTime};
use tp_contracts::bus::{EventBus, EventKind};
use tp_contracts::dav::{
    Collection, CollectionId, CollectionKind, Component, DavLimits, DavStore, DeleteOutcome,
    ObjectIndex, Precondition, Put, PutOutcome, Span, TimeRange,
};
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, Error, ModSeq, TenantId};

use super::next_signal;

/// Лимиты, с которыми хранилище проходит набор [`dav`](super::dav()) (у `PgMeta` — секция `meta` теста).
pub const LIMITS: DavLimits = DavLimits {
    max_object_size: 4096,
    max_objects: 4,
    max_collections: 3,
    busy_horizon: std::time::Duration::from_secs(30 * 24 * 3600),
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
