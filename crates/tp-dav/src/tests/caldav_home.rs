//! DAV-05: дом календарей (календарь и список задач по умолчанию), свойства календаря для Thunderbird, iOS и
//! DAVx5 (снимки ответов), часовой пояс календаря, заготовки ресурсов планирования.

use axum::http::StatusCode;
use tp_contracts::dav::{CollectionKind, DavStore};

use super::{CALS, Env};

/// Thunderbird: поиск календарей в доме (`Depth: 1`).
const THUNDERBIRD_HOME: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:A="http://apple.com/ns/ical/">
  <D:prop>
    <D:resourcetype/>
    <D:displayname/>
    <D:current-user-privilege-set/>
    <A:calendar-color/>
    <C:supported-calendar-component-set/>
  </D:prop>
</D:propfind>"#;

/// iOS (dataaccessd): дом календарей (`Depth: 1`), сокращённый набор свойств запроса.
const IOS_HOME: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<A:propfind xmlns:A="DAV:">
  <A:prop>
    <A:displayname/>
    <A:resourcetype/>
    <A:owner/>
    <A:sync-token/>
    <A:supported-report-set/>
    <B:getctag xmlns:B="http://calendarserver.org/ns/"/>
    <C:calendar-description xmlns:C="urn:ietf:params:xml:ns:caldav"/>
    <C:calendar-timezone xmlns:C="urn:ietf:params:xml:ns:caldav"/>
    <C:supported-calendar-component-set xmlns:C="urn:ietf:params:xml:ns:caldav"/>
    <C:schedule-calendar-transp xmlns:C="urn:ietf:params:xml:ns:caldav"/>
    <D:calendar-color xmlns:D="http://apple.com/ns/ical/"/>
    <D:calendar-order xmlns:D="http://apple.com/ns/ical/"/>
  </A:prop>
</A:propfind>"#;

/// DAVx5: свойства календаря (`Depth: 0`).
const DAVX5_CALENDAR: &str = r#"<?xml version='1.0' encoding='UTF-8' ?>
<propfind xmlns="DAV:" xmlns:CAL="urn:ietf:params:xml:ns:caldav" xmlns:ical="http://apple.com/ns/ical/">
  <prop>
    <resourcetype/>
    <displayname/>
    <current-user-privilege-set/>
    <CAL:calendar-description/>
    <ical:calendar-color/>
    <CAL:calendar-timezone-id/>
    <CAL:supported-calendar-component-set/>
    <CAL:supported-calendar-data/>
    <CAL:max-resource-size/>
    <ical:calendar-order/>
  </prop>
</propfind>"#;

#[tokio::test]
async fn thunderbird_home_snapshot() {
    let env = Env::new();
    let r = env.propfind(CALS, "1", THUNDERBIRD_HOME).await;
    insta::assert_snapshot!("thunderbird_home", r.show());
}

#[tokio::test]
async fn ios_home_snapshot() {
    let env = Env::new();
    let r = env.propfind(CALS, "1", IOS_HOME).await;
    insta::assert_snapshot!("ios_home", r.show());
}

#[tokio::test]
async fn davx5_calendar_snapshot() {
    let env = Env::new();
    env.propfind(CALS, "0", "").await;
    let patch = r#"<d:propertyupdate xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
      <d:set><d:prop><c:calendar-timezone-id>Europe/Moscow</c:calendar-timezone-id></d:prop></d:set>
    </d:propertyupdate>"#;
    let r = env
        .send("PROPPATCH", &format!("{CALS}calendar/"), &[], patch)
        .await;
    assert!(r.body.contains("200 OK"), "{}", r.body);
    let r = env
        .propfind(&format!("{CALS}calendar/"), "0", DAVX5_CALENDAR)
        .await;
    insta::assert_snapshot!("davx5_calendar", r.show());
    let r = env
        .propfind(&format!("{CALS}tasks/"), "0", DAVX5_CALENDAR)
        .await;
    insta::assert_snapshot!("davx5_tasks", r.show());
}

#[tokio::test]
async fn defaults_created_once() {
    let env = Env::new();
    let list = || async {
        env.store
            .list_collections(env.scope, CollectionKind::Calendar)
            .await
            .expect("список")
    };
    assert!(list().await.is_empty());
    for depth in ["0", "1", "1"] {
        let r = env.propfind(CALS, depth, "").await;
        assert_eq!(r.status, StatusCode::MULTI_STATUS);
    }
    let names: Vec<_> = list()
        .await
        .into_iter()
        .map(|c| (c.name, c.display_name))
        .collect();
    assert_eq!(
        names,
        [
            ("calendar".to_owned(), "Календарь".to_owned()),
            ("tasks".to_owned(), "Задачи".to_owned())
        ]
    );
    // Пока есть хоть один календарь, удалённый не создаётся заново.
    let r = env.send("DELETE", &format!("{CALS}tasks/"), &[], "").await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    env.propfind(CALS, "1", "").await;
    assert_eq!(list().await.len(), 1);
}

#[tokio::test]
async fn existing_calendars_keep_home_as_is() {
    let env = Env::new();
    env.calendar("work").await;
    env.propfind(CALS, "1", "").await;
    assert!(
        env.collection(CollectionKind::Calendar, "calendar")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn calendar_timezone() {
    let env = Env::new();
    env.calendar("work").await;
    let path = format!("{CALS}work/");
    let set = r#"<d:propertyupdate xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
      <d:set><d:prop><c:calendar-timezone>BEGIN:VCALENDAR
VERSION:2.0
PRODID:-//Apple Inc.//iOS 17.0//EN
BEGIN:VTIMEZONE
TZID:Europe/Moscow
BEGIN:STANDARD
DTSTART:20010101T000000
TZOFFSETFROM:+0300
TZOFFSETTO:+0300
END:STANDARD
END:VTIMEZONE
END:VCALENDAR
</c:calendar-timezone></d:prop></d:set>
    </d:propertyupdate>"#;
    let r = env.send("PROPPATCH", &path, &[], set).await;
    assert!(r.body.contains("200 OK"), "{}", r.body);
    let c = env
        .collection(CollectionKind::Calendar, "work")
        .await
        .expect("work");
    assert_eq!(c.timezone.as_deref(), Some("Europe/Moscow"));

    let get = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
      <d:prop><c:calendar-timezone/><c:calendar-timezone-id/></d:prop></d:propfind>"#;
    let r = env.propfind(&path, "0", get).await;
    for part in [
        "<c:calendar-timezone-id>Europe/Moscow</c:calendar-timezone-id>",
        "BEGIN:VTIMEZONE",
        "TZID:Europe/Moscow",
        "TZOFFSETTO:+0300",
        "END:VCALENDAR",
    ] {
        assert!(r.body.contains(part), "{part}: {}", r.body);
    }

    let bad = r#"<d:propertyupdate xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
      <d:set><d:prop><c:calendar-timezone-id>Mars/Olympus</c:calendar-timezone-id></d:prop></d:set>
    </d:propertyupdate>"#;
    let r = env.send("PROPPATCH", &path, &[], bad).await;
    assert!(r.body.contains("409 Conflict"), "{}", r.body);

    let remove = r#"<d:propertyupdate xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
      <d:remove><d:prop><c:calendar-timezone/></d:prop></d:remove></d:propertyupdate>"#;
    let r = env.send("PROPPATCH", &path, &[], remove).await;
    assert!(r.body.contains("200 OK"), "{}", r.body);
    let r = env.propfind(&path, "0", get).await;
    assert!(r.body.contains("404 Not Found"), "{}", r.body);
}

#[tokio::test]
async fn schedule_stubs() {
    let env = Env::new();
    let get = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
      <d:prop><c:schedule-inbox-URL/><c:schedule-outbox-URL/></d:prop></d:propfind>"#;
    let r = env
        .propfind("/dav/principals/ivan@example.org/", "0", get)
        .await;
    for part in [
        "<c:schedule-inbox-URL><d:href>/dav/calendars/ivan@example.org/inbox/</d:href></c:schedule-inbox-URL>",
        "<c:schedule-outbox-URL><d:href>/dav/calendars/ivan@example.org/outbox/</d:href></c:schedule-outbox-URL>",
    ] {
        assert!(r.body.contains(part), "{part}: {}", r.body);
    }
    let rt = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/></d:prop></d:propfind>"#;
    let r = env.propfind(&format!("{CALS}inbox/"), "0", rt).await;
    assert!(
        r.body
            .contains("<d:resourcetype><d:collection/><c:schedule-inbox/></d:resourcetype>"),
        "{}",
        r.body
    );
    let r = env.propfind(&format!("{CALS}outbox/"), "1", rt).await;
    assert!(r.body.contains("<c:schedule-outbox/>"), "{}", r.body);
    for name in ["inbox", "outbox"] {
        let r = env
            .send("MKCALENDAR", &format!("{CALS}{name}/"), &[], "")
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{name}");
    }
    // Ресурсы планирования не входят в список календарей дома.
    let r = env.propfind(CALS, "1", rt).await;
    assert!(!r.body.contains("inbox"), "{}", r.body);
}
