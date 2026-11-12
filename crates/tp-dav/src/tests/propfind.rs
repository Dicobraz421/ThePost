//! DAV-02: PROPFIND (снимки запросов Thunderbird, iOS, DAVx5), Depth, allprop, propname, лимит ответа,
//! PROPPATCH (атомарность), ошибки XML.

use std::collections::BTreeSet;

use axum::http::StatusCode;
use tp_contracts::dav::{Collection, CollectionKind, Component, DavStore};

use super::{CALS, Env};
use crate::DavConfig;

/// Thunderbird: проверка календаря перед подпиской.
const THUNDERBIRD: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<D:propfind xmlns:D="DAV:" xmlns:CS="http://calendarserver.org/ns/" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>
    <D:resourcetype/>
    <D:owner/>
    <D:current-user-principal/>
    <D:current-user-privilege-set/>
    <D:supported-report-set/>
    <C:supported-calendar-component-set/>
    <CS:getctag/>
  </D:prop>
</D:propfind>"#;

/// iOS (dataaccessd): свойства принципала, префикс переопределяется на каждом элементе.
const IOS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<A:propfind xmlns:A="DAV:">
  <A:prop>
    <A:calendar-home-set xmlns:A="urn:ietf:params:xml:ns:caldav"/>
    <A:calendar-user-address-set xmlns:A="urn:ietf:params:xml:ns:caldav"/>
    <A:current-user-principal/>
    <A:displayname/>
    <A:dropbox-home-URL xmlns:A="http://calendarserver.org/ns/"/>
    <A:email-address-set xmlns:A="http://calendarserver.org/ns/"/>
    <A:principal-URL/>
    <A:resource-id/>
    <A:supported-report-set/>
  </A:prop>
</A:propfind>"#;

/// DAVx5: список календарей дома (`Depth: 1`).
const DAVX5: &str = r#"<?xml version='1.0' encoding='UTF-8' ?>
<propfind xmlns="DAV:" xmlns:CAL="urn:ietf:params:xml:ns:caldav" xmlns:CS="http://calendarserver.org/ns/" xmlns:ical="http://apple.com/ns/ical/">
  <prop>
    <resourcetype/>
    <displayname/>
    <CAL:calendar-description/>
    <ical:calendar-color/>
    <CAL:supported-calendar-component-set/>
    <sync-token/>
    <CS:getctag/>
    <CAL:calendar-timezone/>
  </prop>
</propfind>"#;

#[tokio::test]
async fn thunderbird_snapshot() {
    let env = Env::new();
    env.calendar("work").await;
    env.put_event("work", "ev1.ics", "ev-1", "Планёрка").await;
    let r = env
        .propfind(&format!("{CALS}work/"), "0", THUNDERBIRD)
        .await;
    insta::assert_snapshot!(r.show(), @r#"
    207 Multi-Status
    <?xml version="1.0" encoding="utf-8"?>
    <d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cs="http://calendarserver.org/ns/" xmlns:ical="http://apple.com/ns/ical/">
    <d:response>
    <d:href>/dav/calendars/ivan@example.org/work/</d:href>
    <d:propstat>
    <d:prop>
    <d:resourcetype>
    <d:collection/>
    <c:calendar/>
    </d:resourcetype>
    <d:owner>
    <d:href>/dav/principals/ivan@example.org/</d:href>
    </d:owner>
    <d:current-user-principal>
    <d:href>/dav/principals/ivan@example.org/</d:href>
    </d:current-user-principal>
    <d:current-user-privilege-set>
    <d:privilege>
    <d:all/>
    </d:privilege>
    <d:privilege>
    <d:read/>
    </d:privilege>
    <d:privilege>
    <d:write/>
    </d:privilege>
    <d:privilege>
    <d:write-properties/>
    </d:privilege>
    <d:privilege>
    <d:write-content/>
    </d:privilege>
    <d:privilege>
    <d:bind/>
    </d:privilege>
    <d:privilege>
    <d:unbind/>
    </d:privilege>
    <d:privilege>
    <d:read-current-user-privilege-set/>
    </d:privilege>
    </d:current-user-privilege-set>
    <d:supported-report-set>
    <d:supported-report>
    <d:report>
    <d:sync-collection/>
    </d:report>
    </d:supported-report>
    <d:supported-report>
    <d:report>
    <c:calendar-query/>
    </d:report>
    </d:supported-report>
    <d:supported-report>
    <d:report>
    <c:calendar-multiget/>
    </d:report>
    </d:supported-report>
    <d:supported-report>
    <d:report>
    <c:free-busy-query/>
    </d:report>
    </d:supported-report>
    </d:supported-report-set>
    <c:supported-calendar-component-set>
    <c:comp name="VEVENT"/>
    <c:comp name="VTODO"/>
    </c:supported-calendar-component-set>
    <cs:getctag>2</cs:getctag>
    </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
    </d:response>
    </d:multistatus>
    "#);
}

#[tokio::test]
async fn ios_snapshot() {
    let env = Env::new();
    let r = env
        .propfind("/dav/principals/ivan@example.org/", "0", IOS)
        .await;
    insta::assert_snapshot!(r.show(), @r#"
    207 Multi-Status
    <?xml version="1.0" encoding="utf-8"?>
    <d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cs="http://calendarserver.org/ns/" xmlns:ical="http://apple.com/ns/ical/">
    <d:response>
    <d:href>/dav/principals/ivan@example.org/</d:href>
    <d:propstat>
    <d:prop>
    <c:calendar-home-set>
    <d:href>/dav/calendars/ivan@example.org/</d:href>
    </c:calendar-home-set>
    <c:calendar-user-address-set>
    <d:href>mailto:ivan@example.org</d:href>
    <d:href>/dav/principals/ivan@example.org/</d:href>
    </c:calendar-user-address-set>
    <d:current-user-principal>
    <d:href>/dav/principals/ivan@example.org/</d:href>
    </d:current-user-principal>
    <d:displayname>ivan@example.org</d:displayname>
    <d:principal-URL>
    <d:href>/dav/principals/ivan@example.org/</d:href>
    </d:principal-URL>
    <d:supported-report-set/>
    </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
    <d:propstat>
    <d:prop>
    <cs:dropbox-home-URL/>
    <cs:email-address-set/>
    <d:resource-id/>
    </d:prop>
    <d:status>HTTP/1.1 404 Not Found</d:status>
    </d:propstat>
    </d:response>
    </d:multistatus>
    "#);
}

#[tokio::test]
async fn davx5_snapshot() {
    let env = Env::new();
    env.calendar("work").await;
    env.create(Collection {
        display_name: "Задачи".into(),
        order: 1,
        components: BTreeSet::from([Component::Todo]),
        ..Collection::new(CollectionKind::Calendar, "tasks")
    })
    .await;
    let r = env.propfind(CALS, "1", DAVX5).await;
    insta::assert_snapshot!(r.show(), @r#"
    207 Multi-Status
    <?xml version="1.0" encoding="utf-8"?>
    <d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cs="http://calendarserver.org/ns/" xmlns:ical="http://apple.com/ns/ical/">
    <d:response>
    <d:href>/dav/calendars/ivan@example.org/</d:href>
    <d:propstat>
    <d:prop>
    <d:resourcetype>
    <d:collection/>
    </d:resourcetype>
    </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
    <d:propstat>
    <d:prop>
    <d:displayname/>
    <c:calendar-description/>
    <ical:calendar-color/>
    <c:supported-calendar-component-set/>
    <d:sync-token/>
    <cs:getctag/>
    <c:calendar-timezone/>
    </d:prop>
    <d:status>HTTP/1.1 404 Not Found</d:status>
    </d:propstat>
    </d:response>
    <d:response>
    <d:href>/dav/calendars/ivan@example.org/work/</d:href>
    <d:propstat>
    <d:prop>
    <d:resourcetype>
    <d:collection/>
    <c:calendar/>
    </d:resourcetype>
    <d:displayname>Работа</d:displayname>
    <c:calendar-description>Рабочие встречи</c:calendar-description>
    <ical:calendar-color>#FF2968FF</ical:calendar-color>
    <c:supported-calendar-component-set>
    <c:comp name="VEVENT"/>
    <c:comp name="VTODO"/>
    </c:supported-calendar-component-set>
    <d:sync-token>urn:thepost:sync:1</d:sync-token>
    <cs:getctag>1</cs:getctag>
    </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
    <d:propstat>
    <d:prop>
    <c:calendar-timezone/>
    </d:prop>
    <d:status>HTTP/1.1 404 Not Found</d:status>
    </d:propstat>
    </d:response>
    <d:response>
    <d:href>/dav/calendars/ivan@example.org/tasks/</d:href>
    <d:propstat>
    <d:prop>
    <d:resourcetype>
    <d:collection/>
    <c:calendar/>
    </d:resourcetype>
    <d:displayname>Задачи</d:displayname>
    <c:supported-calendar-component-set>
    <c:comp name="VTODO"/>
    </c:supported-calendar-component-set>
    <d:sync-token>urn:thepost:sync:2</d:sync-token>
    <cs:getctag>2</cs:getctag>
    </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
    <d:propstat>
    <d:prop>
    <c:calendar-description/>
    <ical:calendar-color/>
    <c:calendar-timezone/>
    </d:prop>
    <d:status>HTTP/1.1 404 Not Found</d:status>
    </d:propstat>
    </d:response>
    </d:multistatus>
    "#);
}

#[tokio::test]
async fn depth_infinity_forbidden() {
    let env = Env::new();
    for depth in [Some("infinity"), Some("Infinity"), None] {
        let headers: Vec<(&str, &str)> = depth.map(|d| ("depth", d)).into_iter().collect();
        let r = env.send("PROPFIND", CALS, &headers, "").await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{depth:?}");
        assert!(r.body.contains("<d:error"), "{}", r.body);
        assert!(r.body.contains("<d:propfind-finite-depth/></d:error>"));
    }
    let r = env.propfind(CALS, "2", "").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn allprop_propname_and_children() {
    let env = Env::new();
    env.calendar("work").await;
    let e1 = env.put_event("work", "a.ics", "a", "Первое").await;
    let e2 = env.put_event("work", "b.ics", "b", "Второе").await;
    assert_eq!((e1.as_str(), e2.as_str()), ("\"2\"", "\"3\""));

    let r = env.propfind(&format!("{CALS}work/"), "1", "").await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    for part in [
        "<d:href>/dav/calendars/ivan@example.org/work/a.ics</d:href><d:propstat><d:prop><d:resourcetype/><d:getetag>\"2\"</d:getetag><d:getcontenttype>text/calendar; charset=utf-8</d:getcontenttype></d:prop>",
        "<d:href>/dav/calendars/ivan@example.org/work/b.ics</d:href>",
        "<d:displayname>Работа</d:displayname>",
    ] {
        assert!(r.body.contains(part), "{part}\n{}", r.body);
    }
    assert!(
        !r.body.contains("current-user-privilege-set"),
        "вычисляемые — не в allprop"
    );

    let include = r#"<d:propfind xmlns:d="DAV:"><d:allprop/><d:include><d:sync-token/><d:x-unknown/></d:include></d:propfind>"#;
    let r = env.propfind(&format!("{CALS}work/"), "0", include).await;
    assert!(
        r.body
            .contains("<d:sync-token>urn:thepost:sync:3</d:sync-token>"),
        "{}",
        r.body
    );
    assert!(
        r.body
            .contains("<d:x-unknown/></d:prop><d:status>HTTP/1.1 404 Not Found")
    );

    let names = r#"<d:propfind xmlns:d="DAV:"><d:propname/></d:propfind>"#;
    let r = env.propfind(&format!("{CALS}work/a.ics"), "0", names).await;
    for part in [
        "<d:getetag/>",
        "<c:calendar-data/>",
        "<d:getcontentlength/>",
        "<d:resourcetype/>",
    ] {
        assert!(r.body.contains(part), "{part}\n{}", r.body);
    }

    let data = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/><c:calendar-data/></d:prop></d:propfind>"#;
    let r = env.propfind(&format!("{CALS}work/"), "1", data).await;
    assert!(r.body.contains("SUMMARY:Первое"), "{}", r.body);
    assert!(r.body.contains("SUMMARY:Второе"));

    for path in [
        format!("{CALS}none/"),
        format!("{CALS}work/none.ics"),
        format!("{CALS}none/a.ics"),
    ] {
        let r = env.propfind(&path, "0", "").await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{path}");
    }
}

#[tokio::test]
async fn response_limit() {
    let env = Env::with_config(DavConfig {
        max_response_items: 1,
        ..DavConfig::default()
    });
    env.calendar("work").await;
    env.calendar("home").await;
    env.put_event("work", "a.ics", "a", "A").await;
    env.put_event("work", "b.ics", "b", "B").await;
    let truncated = "<d:href>/dav/calendars/ivan@example.org/work/</d:href><d:status>HTTP/1.1 507 Insufficient Storage</d:status><d:error><d:number-of-matches-within-limits/></d:error>";
    let r = env.propfind(&format!("{CALS}work/"), "1", "").await;
    assert!(r.body.contains("work/a.ics"));
    assert!(!r.body.contains("work/b.ics"));
    assert!(r.body.contains(truncated), "{}", r.body);
    let r = env.propfind(CALS, "1", "").await;
    assert!(r.body.contains("/dav/calendars/ivan@example.org/home/"));
    assert!(
        !r.body
            .contains("<d:href>/dav/calendars/ivan@example.org/work/</d:href><d:propstat>")
    );
    assert!(
        r.body
            .contains("<d:href>/dav/calendars/ivan@example.org/</d:href><d:status>HTTP/1.1 507")
    );
}

#[tokio::test]
async fn bad_bodies() {
    let env = Env::new();
    for body in [
        "<d:propfind xmlns:d=\"DAV:\"><d:prop>",
        "<!DOCTYPE x [<!ENTITY e SYSTEM \"file:///etc/passwd\">]><d:propfind xmlns:d=\"DAV:\"><d:prop>&e;</d:prop></d:propfind>",
        "<d:other xmlns:d=\"DAV:\"/>",
        "<d:propfind xmlns:d=\"DAV:\"/>",
    ] {
        let r = env.propfind(CALS, "0", body).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{body}");
    }
    let small = Env::with_config(DavConfig {
        max_request_size: 64,
        ..DavConfig::default()
    });
    let r = small.propfind(CALS, "0", &" ".repeat(65)).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
}

const PATCH: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propertyupdate xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:i="http://apple.com/ns/ical/">
  <d:set><d:prop><d:displayname>Новое имя</d:displayname><i:calendar-color>#00FF00FF</i:calendar-color></d:prop></d:set>
  <d:remove><d:prop><c:calendar-description/></d:prop></d:remove>
  <d:set><d:prop><i:calendar-order>3</i:calendar-order></d:prop></d:set>
</d:propertyupdate>"#;

#[tokio::test]
async fn proppatch_applies() {
    let env = Env::new();
    env.calendar("work").await;
    let r = env
        .send("PROPPATCH", &format!("{CALS}work/"), &[], PATCH)
        .await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert!(
        r.body.contains(concat!(
            "<d:propstat><d:prop><d:displayname/><ical:calendar-color/><c:calendar-description/>",
            "<ical:calendar-order/></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat>"
        )),
        "{}",
        r.body
    );
    let c = env
        .collection(CollectionKind::Calendar, "work")
        .await
        .expect("work");
    assert_eq!(c.display_name, "Новое имя");
    assert_eq!(c.color.as_deref(), Some("#00FF00FF"));
    assert_eq!(c.description, None);
    assert_eq!(c.order, 3);
    assert_eq!(c.state.0, 2, "изменение свойств — новое состояние");
}

#[tokio::test]
async fn proppatch_is_atomic() {
    let env = Env::new();
    let before = env.calendar("work").await;
    let body = r#"<d:propertyupdate xmlns:d="DAV:" xmlns:i="http://apple.com/ns/ical/" xmlns:x="urn:x">
      <d:set><d:prop><d:displayname>Другое</d:displayname><x:dead>v</x:dead><i:calendar-order>abc</i:calendar-order><d:getetag>"1"</d:getetag></d:prop></d:set>
    </d:propertyupdate>"#;
    let r = env
        .send("PROPPATCH", &format!("{CALS}work/"), &[], body)
        .await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    for part in [
        "<d:propstat><d:prop><d:displayname/></d:prop><d:status>HTTP/1.1 424 Failed Dependency</d:status></d:propstat>",
        "<d:propstat><d:prop><x:dead xmlns:x=\"urn:x\"/><d:getetag/></d:prop><d:status>HTTP/1.1 403 Forbidden</d:status></d:propstat>",
        "<d:propstat><d:prop><ical:calendar-order/></d:prop><d:status>HTTP/1.1 409 Conflict</d:status></d:propstat>",
    ] {
        assert!(r.body.contains(part), "{part}\n{}", r.body);
    }
    let after = env
        .collection(CollectionKind::Calendar, "work")
        .await
        .expect("work");
    assert_eq!(after, before, "ничего не записано");

    let r = env
        .send("PROPPATCH", "/dav/principals/ivan@example.org/", &[], PATCH)
        .await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert!(
        r.body
            .contains("<d:status>HTTP/1.1 403 Forbidden</d:status>")
    );
    assert!(!r.body.contains("200 OK"));
    let r = env
        .send("PROPPATCH", &format!("{CALS}work/"), &[], "<x/>")
        .await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

/// DAV-25: оповещения по умолчанию календаря — заданные по JMAP (поля коллекции, JSON `Id[Alert]`) видны в CalDAV
/// компонентами VALARM, заданные `PROPPATCH` — в коллекции (JMAP); удаление и неразбираемое значение.
#[tokio::test]
async fn default_alarms() {
    let env = Env::new();
    let c = env.calendar("work").await;
    let jmap = r#"{"a1":{"@type":"Alert","trigger":{"@type":"OffsetTrigger","offset":"-PT15M","relativeTo":"start"},"action":"display"}}"#;
    env.store
        .update_collection(
            env.scope,
            Collection {
                alerts_with_time: Some(jmap.to_owned()),
                ..c
            },
        )
        .await
        .expect("коллекция");
    let path = format!("{CALS}work/");
    let body = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
        <d:prop><c:default-alarm-vevent-datetime/><c:default-alarm-vevent-date/></d:prop></d:propfind>"#;
    let r = env.propfind(&path, "0", body).await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    let body_text = r.body.replace("&#13;", "");
    let datetime = body_text
        .split("<c:default-alarm-vevent-datetime>")
        .nth(1)
        .and_then(|s| s.split("</c:default-alarm-vevent-datetime>").next())
        .unwrap_or_else(|| panic!("оповещения: {}", r.body));
    for part in ["BEGIN:VALARM\n", "-PT15M", "ACTION:DISPLAY", "END:VALARM\n"] {
        assert!(datetime.contains(part), "{part}: {datetime}");
    }
    assert!(
        r.body.contains("<c:default-alarm-vevent-date/>"),
        "не задано — пусто: {}",
        r.body
    );

    // PROPPATCH (Календарь macOS присылает компоненты VALARM) — оповещение в коллекции, видно и при чтении.
    const ALARM: &str = "BEGIN:VALARM\nACTION:AUDIO\nTRIGGER:-PT9H\nEND:VALARM";
    let patch = format!(
        r#"<d:propertyupdate xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:set><d:prop>
        <c:default-alarm-vevent-date>{ALARM}</c:default-alarm-vevent-date></d:prop></d:set></d:propertyupdate>"#
    );
    let r = env.send("PROPPATCH", &path, &[], &patch).await;
    assert!(r.body.contains("HTTP/1.1 200 OK"), "{}", r.body);
    let stored = env
        .collection(CollectionKind::Calendar, "work")
        .await
        .expect("календарь");
    let json = stored.alerts_without_time.expect("оповещения без времени");
    assert!(json.contains("-PT9H") && json.contains("Alert"), "{json}");
    assert!(
        stored.alerts_with_time.is_some(),
        "другое свойство не изменилось"
    );
    let r = env.propfind(&path, "0", body).await;
    assert!(
        r.body.contains("TRIGGER") && r.body.contains("-PT9H"),
        "{}",
        r.body
    );

    // Неразбираемое значение — 409, ничего не записано; удаление — оповещений нет.
    let bad = patch.replace(ALARM, "мусор");
    let r = env.send("PROPPATCH", &path, &[], &bad).await;
    assert!(r.body.contains("HTTP/1.1 409 Conflict"), "{}", r.body);
    let remove = r#"<d:propertyupdate xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:remove><d:prop>
        <c:default-alarm-vevent-datetime/></d:prop></d:remove></d:propertyupdate>"#;
    let r = env.send("PROPPATCH", &path, &[], remove).await;
    assert!(r.body.contains("HTTP/1.1 200 OK"), "{}", r.body);
    let stored = env
        .collection(CollectionKind::Calendar, "work")
        .await
        .expect("календарь");
    assert_eq!(stored.alerts_with_time, None);
    assert!(stored.alerts_without_time.is_some());
}
