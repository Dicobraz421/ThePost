//! Набор проверок [`FreeBusy`] (CTR-47) поверх индекса занятости [`DavStore`]: in-memory `MemDav` и `PgMeta`
//! (META-51) проходят одни и те же проверки. Повторяющиеся события (META-54, решение 59) — набор [`recurring`].

use time::{Duration, OffsetDateTime};
use tp_contracts::dav::{
    Collection, CollectionId, CollectionKind, Component, DavStore, ObjectIndex, Precondition, Span,
};
use tp_contracts::meta::AccountScope;
use tp_contracts::schedule::{BusyKind, BusyPeriod, FreeBusy, Period};
use tp_contracts::{AccountId, Error};

use super::dav::DavFixture;

fn at(h: i64) -> OffsetDateTime {
    OffsetDateTime::UNIX_EPOCH + Duration::days(20_000) + Duration::hours(h)
}

fn period(s: i64, e: i64) -> Period {
    Period {
        start: at(s),
        end: at(e),
    }
}

fn busy(s: i64, e: i64, kind: BusyKind) -> BusyPeriod {
    BusyPeriod {
        start: at(s),
        end: at(e),
        kind,
    }
}

async fn calendar(store: &dyn DavStore, scope: AccountScope, coll: Collection) -> CollectionId {
    let id = coll.id;
    store
        .create_collection(scope, coll)
        .await
        .expect("create_collection");
    id
}

async fn event(
    store: &dyn DavStore,
    scope: AccountScope,
    coll: CollectionId,
    name: &str,
    busy: Vec<BusyPeriod>,
) {
    let start = busy.iter().map(|p| p.start).min().unwrap_or(at(0));
    let end = busy.iter().map(|p| p.end).max();
    let index = ObjectIndex {
        busy,
        ..ObjectIndex::new(Component::Event, name, Some(Span { start, end }))
    };
    store
        .put_object(
            scope,
            coll,
            name,
            "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".into(),
            index,
            Precondition::Any,
        )
        .await
        .expect("put_object");
}

/// Набор проверок [`FreeBusy::query`]: периоды всех непрозрачных календарей учётной записи, обрезка по интервалу,
/// склейка периодов одного вида ([`tp_contracts::schedule::merge_busy`]), разные виды не склеиваются, порядок и повторы
/// `accounts`, учётная запись без данных и чужая организация — пусто, удаление объекта и прозрачность календаря
/// убирают занятость, недопустимый интервал — `Invalid`. `dav` и `fb` — одно хранилище; учётные записи `fx` — без
/// данных; лимиты — `suite::dav::LIMITS`.
pub async fn freebusy(dav: &dyn DavStore, fb: &dyn FreeBusy, fx: &DavFixture) {
    let (owner, peer) = (fx.owner, fx.peer);
    let tenant = owner.tenant;
    let work = calendar(
        dav,
        owner,
        Collection::new(CollectionKind::Calendar, "work"),
    )
    .await;
    let home = calendar(
        dav,
        owner,
        Collection::new(CollectionKind::Calendar, "home"),
    )
    .await;
    let holidays = Collection {
        transparent: true,
        ..Collection::new(CollectionKind::Calendar, "holidays")
    };
    let holidays_id = calendar(dav, owner, holidays.clone()).await;
    // Встреча 1–3, повторяющаяся 5–6 и 29–30, предварительная 2–4; смежная 3–4 в другом календаре.
    event(dav, owner, work, "a", vec![busy(1, 3, BusyKind::Busy)]).await;
    event(
        dav,
        owner,
        work,
        "weekly",
        vec![busy(5, 6, BusyKind::Busy), busy(29, 30, BusyKind::Busy)],
    )
    .await;
    event(
        dav,
        owner,
        work,
        "maybe",
        vec![busy(2, 4, BusyKind::Tentative)],
    )
    .await;
    event(dav, owner, home, "b", vec![busy(3, 4, BusyKind::Busy)]).await;
    event(dav, owner, home, "free", Vec::new()).await;
    event(
        dav,
        owner,
        holidays_id,
        "h",
        vec![busy(10, 12, BusyKind::Unavailable)],
    )
    .await;
    let peer_cal = calendar(dav, peer, Collection::new(CollectionKind::Calendar, "work")).await;
    event(
        dav,
        peer,
        peer_cal,
        "p",
        vec![busy(2, 3, BusyKind::Unavailable)],
    )
    .await;

    let nobody = AccountId::new();
    let all = fb
        .query(
            tenant,
            &[owner.account, peer.account, owner.account, nobody],
            period(0, 48),
        )
        .await
        .expect("query");
    assert_eq!(
        all,
        vec![
            (
                owner.account,
                vec![
                    busy(1, 4, BusyKind::Busy),
                    busy(2, 4, BusyKind::Tentative),
                    busy(5, 6, BusyKind::Busy),
                    busy(29, 30, BusyKind::Busy),
                ]
            ),
            (peer.account, vec![busy(2, 3, BusyKind::Unavailable)]),
            (nobody, Vec::new()),
        ],
        "порядок accounts без повторов, склейка, прозрачный календарь не учтён"
    );

    // Обрезка по интервалу; конец интервала не включён.
    let clipped = fb
        .query(tenant, &[owner.account], period(3, 29))
        .await
        .expect("query clipped");
    assert_eq!(
        clipped,
        vec![(
            owner.account,
            vec![
                busy(3, 4, BusyKind::Busy),
                busy(3, 4, BusyKind::Tentative),
                busy(5, 6, BusyKind::Busy),
            ]
        )]
    );
    assert_eq!(
        fb.query(tenant, &[owner.account], period(6, 29)).await,
        Ok(vec![(owner.account, Vec::new())]),
        "период, кончающийся в начале интервала, и начинающийся в его конце — не пересекаются"
    );

    // Чужая организация и недопустимый интервал.
    assert_eq!(
        fb.query(fx.foreign.tenant, &[owner.account], period(0, 48))
            .await,
        Ok(vec![(owner.account, Vec::new())])
    );
    for bad in [period(5, 5), period(6, 5)] {
        assert!(matches!(
            fb.query(tenant, &[owner.account], bad).await,
            Err(Error::Invalid(_))
        ));
    }
    assert_eq!(fb.query(tenant, &[], period(0, 1)).await, Ok(Vec::new()));

    // Удаление объекта, прозрачность и удаление календаря.
    dav.delete_object(owner, work, "maybe", Precondition::Any)
        .await
        .expect("delete");
    let mut work_coll = dav
        .get_collection(owner, work)
        .await
        .expect("get")
        .expect("work");
    work_coll.transparent = true;
    dav.update_collection(owner, work_coll)
        .await
        .expect("transparent");
    dav.update_collection(
        owner,
        Collection {
            transparent: false,
            ..holidays
        },
    )
    .await
    .expect("opaque");
    dav.delete_collection(owner, home)
        .await
        .expect("delete home");
    assert_eq!(
        fb.query(tenant, &[owner.account], period(0, 48)).await,
        Ok(vec![(
            owner.account,
            vec![busy(10, 12, BusyKind::Unavailable)]
        )])
    );
}

fn ts(unix: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(unix).expect("метка времени")
}

/// Тело события с повторами: `lines` — свойства `VEVENT` после `UID`.
fn recurring_body(uid: &str, lines: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//thepost//suite//EN\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
         DTSTAMP:20260101T000000Z\r\n{lines}END:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

async fn put_recurring(
    store: &dyn DavStore,
    scope: AccountScope,
    coll: CollectionId,
    name: &str,
    body: String,
    start: OffsetDateTime,
) {
    let index = ObjectIndex {
        recurring: true,
        ..ObjectIndex::new(Component::Event, name, Some(Span { start, end: None }))
    };
    store
        .put_object(scope, coll, name, body, index, Precondition::Any)
        .await
        .expect("put_object");
}

/// Набор проверок занятости повторяющихся событий (META-54, решение 59): повторы разворачиваются при запросе, без
/// горизонта — еженедельная встреча без конца видна через 3 года, исключённая дата (`EXDATE`) не занята; время без
/// пояса — в поясе календаря; вхождения сверх `DavLimits::max_busy_instances` не видны; повторяющийся объект с
/// индексом занятости — `Invalid`; перезапись неповторяющимся возвращает занятость по индексу. `dav` и `fb` — одно
/// хранилище; учётные записи `fx` — без данных; лимиты — `suite::dav::LIMITS`.
pub async fn recurring(dav: &dyn DavStore, fb: &dyn FreeBusy, fx: &DavFixture) {
    let owner = fx.owner;
    let tenant = owner.tenant;
    let work = calendar(
        dav,
        owner,
        Collection::new(CollectionKind::Calendar, "work"),
    )
    .await;
    let tokyo = calendar(
        dav,
        owner,
        Collection {
            timezone: Some("Asia/Tokyo".into()),
            ..Collection::new(CollectionKind::Calendar, "tokyo")
        },
    )
    .await;
    // Понедельник 5 января 2026, 10:00–11:00 UTC, каждую неделю без конца; 12 февраля 2029 исключено.
    let first = ts(1_767_607_200);
    put_recurring(
        dav,
        owner,
        work,
        "weekly",
        recurring_body(
            "weekly",
            "DTSTART:20260105T100000Z\r\nDTEND:20260105T110000Z\r\nRRULE:FREQ=WEEKLY\r\n\
             EXDATE:20290212T100000Z\r\n",
        ),
        first,
    )
    .await;
    let hour = |unix: i64| BusyPeriod {
        start: ts(unix),
        end: ts(unix + 3600),
        kind: BusyKind::Busy,
    };
    // 1–20 февраля 2029: понедельники 5 и 19; 12-е исключено.
    let feb_2029 = Period {
        start: ts(1_864_598_400),
        end: ts(1_866_240_000),
    };
    assert_eq!(
        fb.query(tenant, &[owner.account], feb_2029).await,
        Ok(vec![(
            owner.account,
            vec![hour(1_864_980_000), hour(1_866_189_600)]
        )]),
        "встреча без конца видна через 3 года, исключённая дата не занята"
    );
    // Вхождения сверх лимита (1000 недель от начала) не разворачиваются: 2050 год пуст.
    assert_eq!(
        fb.query(
            tenant,
            &[owner.account],
            Period {
                start: ts(2_524_608_000),
                end: ts(2_524_608_000 + 30 * 86_400),
            }
        )
        .await,
        Ok(vec![(owner.account, vec![])])
    );
    // Время без пояса — в поясе календаря: 10:00 в Токио — 01:00 UTC.
    put_recurring(
        dav,
        owner,
        tokyo,
        "floating",
        recurring_body(
            "floating",
            "DTSTART:20260105T100000\r\nDTEND:20260105T110000\r\nRRULE:FREQ=DAILY;COUNT=2\r\n",
        ),
        ts(1_767_574_800),
    )
    .await;
    assert_eq!(
        fb.query(
            tenant,
            &[owner.account],
            Period {
                start: ts(1_767_574_800 - 3600),
                end: ts(1_767_574_800 + 7200),
            }
        )
        .await,
        Ok(vec![(owner.account, vec![hour(1_767_574_800)])])
    );
    // Повторяющийся объект с индексом занятости не принимается.
    let both = ObjectIndex {
        recurring: true,
        busy: vec![hour(1_767_607_200)],
        ..ObjectIndex::new(
            Component::Event,
            "both",
            Some(Span {
                start: first,
                end: None,
            }),
        )
    };
    assert!(matches!(
        dav.put_object(
            owner,
            work,
            "both",
            recurring_body("both", "DTSTART:20260105T100000Z\r\nRRULE:FREQ=DAILY\r\n"),
            both,
            Precondition::Any,
        )
        .await,
        Err(Error::Invalid(_))
    ));
    // Перезапись неповторяющимся: занятость — по индексу, повторов нет; и обратно.
    let single = ObjectIndex {
        busy: vec![hour(1_864_980_000)],
        ..ObjectIndex::new(
            Component::Event,
            "weekly",
            Some(Span {
                start: ts(1_864_980_000),
                end: Some(ts(1_864_980_000 + 3600)),
            }),
        )
    };
    dav.put_object(
        owner,
        work,
        "weekly",
        recurring_body(
            "weekly",
            "DTSTART:20290205T100000Z\r\nDTEND:20290205T110000Z\r\n",
        ),
        single,
        Precondition::Any,
    )
    .await
    .expect("put single");
    assert_eq!(
        fb.query(tenant, &[owner.account], feb_2029).await,
        Ok(vec![(owner.account, vec![hour(1_864_980_000)])])
    );
    put_recurring(
        dav,
        owner,
        work,
        "weekly",
        recurring_body(
            "weekly",
            "DTSTART:20260105T100000Z\r\nDTEND:20260105T110000Z\r\nRRULE:FREQ=WEEKLY\r\n",
        ),
        first,
    )
    .await;
    assert_eq!(
        fb.query(tenant, &[owner.account], feb_2029).await,
        Ok(vec![(
            owner.account,
            vec![
                hour(1_864_980_000),
                hour(1_864_980_000 + 7 * 86_400),
                hour(1_866_189_600)
            ]
        )])
    );
}
