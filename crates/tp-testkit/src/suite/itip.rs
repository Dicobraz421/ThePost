//! Набор проверок планирования в [`DavStore`] (CTR-47): входящие планирования (`inbox_*`), `Schedule-Tag` и индекс
//! занятости объекта, прозрачность календаря. In-memory `MemDav` и `PgMeta` (META-51) проходят одни и те же проверки.

use time::{Duration, OffsetDateTime};
use tp_contracts::bus::{EventBus, EventKind};
use tp_contracts::dav::{
    Collection, CollectionKind, Component, DavStore, ObjectIndex, Precondition, PutOutcome, Span,
};
use tp_contracts::meta::AccountScope;
use tp_contracts::schedule::{BusyKind, BusyPeriod, InboxMessage, ItipMethod};
use tp_contracts::{Error, ModSeq};

use super::dav::{DavFixture, LIMITS};
use super::next_signal;

fn at(h: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::days(20_000) + Duration::hours(h)
}

fn message(method: ItipMethod, uid: &str) -> InboxMessage {
    InboxMessage {
        method,
        uid: uid.into(),
        from: "boss@partner.example".into(),
        body: format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:{method}\r\nBEGIN:VEVENT\r\nUID:{uid}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
        ),
    }
}

/// Набор проверок планирования [`DavStore`]: входящие (создание, перезапись с прежним `id`, повтор UID, порядок по
/// `ModSeq`, состояние, проверки имени и сообщения, лимиты, удаление, разделение учётных записей и организаций,
/// независимость от состояния календарей), `Schedule-Tag` и занятость в [`ObjectIndex`], `Collection::transparent`.
/// Хранилище — с лимитами [`LIMITS`]; учётные записи `fx` — без данных.
pub async fn itip(store: &dyn DavStore, fx: &DavFixture) {
    let s = fx.owner;
    assert_eq!(store.limits(), LIMITS, "набор требует лимиты LIMITS");
    let empty = store.inbox_list(s).await.expect("inbox_list");
    assert!(empty.items.is_empty());
    assert_eq!(empty.state, ModSeq::ZERO);

    // Календарь до входящих: состояние вида не зависит от входящих.
    let work = Collection::new(CollectionKind::Calendar, "work");
    store
        .create_collection(s, work.clone())
        .await
        .expect("create");
    let cal_state = store
        .state(s, CollectionKind::Calendar)
        .await
        .expect("state");

    // Создание и повтор UID.
    let a = store
        .inbox_put(s, "a.ics", message(ItipMethod::Request, "u1"))
        .await
        .expect("inbox_put a");
    assert!(a.created);
    let b = store
        .inbox_put(s, "b.ics", message(ItipMethod::Cancel, "u1"))
        .await
        .expect("UID во входящих не уникален");
    assert!(b.created && b.modseq > a.modseq && b.id != a.id);
    let inbox = store.inbox_list(s).await.expect("list");
    assert_eq!(inbox.state, b.modseq);
    let got: Vec<(&str, ItipMethod)> = inbox
        .items
        .iter()
        .map(|i| (i.name.as_str(), i.message.method))
        .collect();
    assert_eq!(
        got,
        vec![("a.ics", ItipMethod::Request), ("b.ics", ItipMethod::Cancel)]
    );
    assert_eq!(inbox.items[0].id, a.id);
    assert_eq!(inbox.items[0].etag, a.etag);
    assert_eq!(inbox.items[0].modseq, a.modseq);
    assert_eq!(inbox.items[0].message, message(ItipMethod::Request, "u1"));

    // Перезапись: прежний id, новые ETag и ModSeq, порядок по ModSeq.
    let a2 = store
        .inbox_put(s, "a.ics", message(ItipMethod::Request, "u2"))
        .await
        .expect("перезапись");
    assert!(!a2.created);
    assert_eq!(a2.id, a.id);
    assert!(a2.modseq > b.modseq && a2.etag != a.etag);
    let inbox = store.inbox_list(s).await.expect("list");
    let names: Vec<&str> = inbox.items.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["b.ics", "a.ics"]);
    assert_eq!(inbox.items[1].message.uid, "u2");

    // Проверки и лимиты.
    for bad in ["", "a/b", ".."] {
        assert!(
            matches!(
                store
                    .inbox_put(s, bad, message(ItipMethod::Reply, "u3"))
                    .await,
                Err(Error::Invalid(_))
            ),
            "{bad:?}"
        );
    }
    for bad in [
        InboxMessage {
            uid: String::new(),
            ..message(ItipMethod::Reply, "u3")
        },
        InboxMessage {
            from: "a b".into(),
            ..message(ItipMethod::Reply, "u3")
        },
    ] {
        assert!(matches!(
            store.inbox_put(s, "c.ics", bad).await,
            Err(Error::Invalid(_))
        ));
    }
    let big = InboxMessage {
        body: "x".repeat(LIMITS.max_object_size + 1),
        ..message(ItipMethod::Reply, "u3")
    };
    assert_eq!(
        store.inbox_put(s, "c.ics", big).await,
        Err(Error::QuotaExceeded)
    );
    for (i, name) in ["c.ics", "d.ics"].into_iter().enumerate() {
        store
            .inbox_put(s, name, message(ItipMethod::Reply, &format!("r{i}")))
            .await
            .expect("до лимита");
    }
    assert_eq!(
        store
            .inbox_put(s, "e.ics", message(ItipMethod::Reply, "r9"))
            .await,
        Err(Error::QuotaExceeded),
        "сообщений не больше max_objects"
    );
    store
        .inbox_put(s, "d.ics", message(ItipMethod::Counter, "r1"))
        .await
        .expect("перезапись при лимите");

    // Удаление.
    let m = store.inbox_delete(s, "c.ics").await.expect("inbox_delete");
    let inbox = store.inbox_list(s).await.expect("list");
    assert_eq!(inbox.state, m);
    assert!(inbox.items.iter().all(|i| i.name != "c.ics"));
    assert_eq!(inbox.items.len(), 3);
    assert_eq!(store.inbox_delete(s, "c.ics").await, Err(Error::NotFound));

    // Разделение учётных записей и организаций.
    for other in [fx.peer, fx.foreign] {
        let inbox = store.inbox_list(other).await.expect("list other");
        assert!(inbox.items.is_empty() && inbox.state == ModSeq::ZERO);
        assert_eq!(store.inbox_delete(other, "a.ics").await, Err(Error::NotFound));
    }

    // Входящие не меняют состояние календарей и не ломают `since`.
    assert_eq!(
        store.state(s, CollectionKind::Calendar).await,
        Ok(cal_state)
    );
    let ch = store
        .account_changes(s, CollectionKind::Calendar, cal_state, 10)
        .await
        .expect("account_changes после входящих");
    assert!(ch.created.is_empty() && ch.updated.is_empty() && ch.destroyed.is_empty());
    let ch = store
        .changes(s, work.id, cal_state, 10)
        .await
        .expect("changes после входящих");
    assert!(ch.created.is_empty() && ch.updated.is_empty() && ch.destroyed.is_empty());

    schedule_fields(store, s, work).await;
}

/// `Schedule-Tag`, занятость объекта и прозрачность календаря хранятся как записаны.
async fn schedule_fields(store: &dyn DavStore, s: AccountScope, work: Collection) {
    let busy = vec![
        BusyPeriod {
            start: at(1),
            end: at(2),
            kind: BusyKind::Busy,
        },
        BusyPeriod {
            start: at(25),
            end: at(26),
            kind: BusyKind::Tentative,
        },
    ];
    let index = ObjectIndex {
        busy: busy.clone(),
        schedule_tag: Some("st-1".into()),
        ..ObjectIndex::new(
            Component::Event,
            "meet",
            Some(Span {
                start: at(1),
                end: Some(at(26)),
            }),
        )
    };
    let body = "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".to_owned();
    let PutOutcome::Stored(_) = store
        .put_object(
            s,
            work.id,
            "meet.ics",
            body.clone(),
            index.clone(),
            Precondition::IfNoneMatch,
        )
        .await
        .expect("put")
    else {
        panic!("ожидалась запись");
    };
    let got = store
        .get_object(s, work.id, "meet.ics")
        .await
        .expect("get")
        .expect("объект");
    assert_eq!(got.index, index, "занятость и Schedule-Tag сохранены");
    let by_uid = store
        .object_by_uid(s, work.id, "meet")
        .await
        .expect("by uid")
        .expect("объект");
    assert_eq!(by_uid.index.schedule_tag.as_deref(), Some("st-1"));
    // Изменение участником только PARTSTAT: Schedule-Tag прежний, ETag новый.
    let PutOutcome::Stored(p) = store
        .put_object(
            s,
            work.id,
            "meet.ics",
            body.clone(),
            index.clone(),
            Precondition::IfMatch(got.etag.clone()),
        )
        .await
        .expect("put partstat")
    else {
        panic!("ожидалась запись");
    };
    let again = store
        .get_object(s, work.id, "meet.ics")
        .await
        .expect("get")
        .expect("объект");
    assert_ne!(again.etag, got.etag);
    assert_eq!(again.etag, p.etag);
    assert_eq!(again.index.schedule_tag.as_deref(), Some("st-1"));
    for bad in [
        ObjectIndex {
            schedule_tag: Some("a\"b".into()),
            ..index.clone()
        },
        ObjectIndex {
            busy: vec![BusyPeriod {
                start: at(2),
                end: at(1),
                kind: BusyKind::Busy,
            }],
            ..index.clone()
        },
    ] {
        assert!(matches!(
            store
                .put_object(s, work.id, "meet.ics", body.clone(), bad, Precondition::Any)
                .await,
            Err(Error::Invalid(_))
        ));
    }

    // Прозрачность календаря.
    let holidays = Collection {
        transparent: true,
        ..Collection::new(CollectionKind::Calendar, "holidays")
    };
    store
        .create_collection(s, holidays.clone())
        .await
        .expect("create transparent");
    let got = store
        .get_collection(s, holidays.id)
        .await
        .expect("get")
        .expect("коллекция");
    assert!(got.transparent);
    store
        .update_collection(
            s,
            Collection {
                transparent: false,
                ..got
            },
        )
        .await
        .expect("update");
    assert!(
        !store
            .get_collection(s, holidays.id)
            .await
            .expect("get")
            .expect("коллекция")
            .transparent
    );
}

/// Набор проверок сигналов входящих планирования (CTR-47): `inbox_put` и `inbox_delete` публикуют
/// `ScheduleInboxChanged` с `ModSeq` изменения подписке учётной записи `scope`. `scope` — без данных.
pub async fn signals(store: &dyn DavStore, bus: &dyn EventBus, scope: AccountScope) {
    let mut events = bus.subscribe(scope).await.expect("subscribe");
    let a = store
        .inbox_put(scope, "a.ics", message(ItipMethod::Request, "u1"))
        .await
        .expect("put");
    let b = store
        .inbox_put(scope, "a.ics", message(ItipMethod::Request, "u1"))
        .await
        .expect("put again");
    let d = store.inbox_delete(scope, "a.ics").await.expect("delete");
    for modseq in [a.modseq, b.modseq, d] {
        let e = next_signal(&mut events).await.expect("сигнал");
        assert_eq!(
            (e.tenant, e.account, e.kind),
            (
                scope.tenant,
                Some(scope.account),
                EventKind::ScheduleInboxChanged { modseq }
            )
        );
    }
}
