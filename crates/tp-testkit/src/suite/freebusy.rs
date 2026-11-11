//! Набор проверок [`FreeBusy`] (CTR-47) поверх индекса занятости [`DavStore`]: in-memory `MemDav` и `PgMeta`
//! (META-51) проходят одни и те же проверки.

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
    let work = calendar(dav, owner, Collection::new(CollectionKind::Calendar, "work")).await;
    let home = calendar(dav, owner, Collection::new(CollectionKind::Calendar, "home")).await;
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
    event(dav, owner, work, "maybe", vec![busy(2, 4, BusyKind::Tentative)]).await;
    event(dav, owner, home, "b", vec![busy(3, 4, BusyKind::Busy)]).await;
    event(dav, owner, home, "free", Vec::new()).await;
    event(dav, owner, holidays_id, "h", vec![busy(10, 12, BusyKind::Unavailable)]).await;
    let peer_cal = calendar(dav, peer, Collection::new(CollectionKind::Calendar, "work")).await;
    event(dav, peer, peer_cal, "p", vec![busy(2, 3, BusyKind::Unavailable)]).await;

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
    dav.delete_collection(owner, home).await.expect("delete home");
    assert_eq!(
        fb.query(tenant, &[owner.account], period(0, 48)).await,
        Ok(vec![(owner.account, vec![busy(10, 12, BusyKind::Unavailable)])])
    );
}
