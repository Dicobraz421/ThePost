//! DAV-03: MKCALENDAR, расширенный MKCOL, PUT с условиями, GET/HEAD, ETag, DELETE объекта и коллекции.

use std::collections::BTreeSet;

use axum::http::StatusCode;
use tp_contracts::dav::{Collection, CollectionKind, Component};

use super::{BOOKS, CALS, Env, event, vcard};
use crate::DavConfig;

const MKCALENDAR: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<C:mkcalendar xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:A="http://apple.com/ns/ical/">
  <D:set><D:prop>
    <D:displayname>Отпуск</D:displayname>
    <A:calendar-color>#00FF00</A:calendar-color>
    <C:calendar-description>Планы</C:calendar-description>
    <C:supported-calendar-component-set><C:comp name="VEVENT"/></C:supported-calendar-component-set>
    <C:calendar-timezone>BEGIN:VCALENDAR&#13;
END:VCALENDAR</C:calendar-timezone>
    <C:schedule-calendar-transp><C:opaque/></C:schedule-calendar-transp>
  </D:prop></D:set>
</C:mkcalendar>"#;

#[tokio::test]
async fn mkcalendar() {
    let env = Env::new();
    let path = format!("{CALS}vacation/");
    let r = env.send("MKCALENDAR", &path, &[], MKCALENDAR).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let c = env
        .collection(CollectionKind::Calendar, "vacation")
        .await
        .expect("создан");
    assert_eq!(c.display_name, "Отпуск");
    assert_eq!(c.color.as_deref(), Some("#00FF00"));
    assert_eq!(c.description.as_deref(), Some("Планы"));
    assert_eq!(c.components, BTreeSet::from([Component::Event]));

    let r = env.send("MKCALENDAR", &path, &[], MKCALENDAR).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED, "имя занято");
    let r = env
        .send("MKCALENDAR", &format!("{CALS}plain"), &[], "")
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "без тела");
    let plain = env
        .collection(CollectionKind::Calendar, "plain")
        .await
        .expect("plain");
    assert_eq!(
        plain.components,
        BTreeSet::from([Component::Event, Component::Todo])
    );
    let r = env
        .send("MKCALENDAR", &format!("{BOOKS}cal/"), &[], "")
        .await;
    assert_eq!(
        r.status,
        StatusCode::FORBIDDEN,
        "календарь в доме адресных книг"
    );
    let r = env.send("MKCALENDAR", CALS, &[], "").await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED, "дом уже есть");

    let bad = MKCALENDAR.replace("name=\"VEVENT\"", "name=\"VCARD\"");
    let r = env
        .send("MKCALENDAR", &format!("{CALS}bad/"), &[], &bad)
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("<c:mkcalendar-response"), "{}", r.body);
    assert!(r.body.contains(
        "<c:supported-calendar-component-set/></d:prop><d:status>HTTP/1.1 403 Forbidden</d:status>"
    ));
    assert!(
        r.body
            .contains("<d:status>HTTP/1.1 424 Failed Dependency</d:status>")
    );
    assert!(
        env.collection(CollectionKind::Calendar, "bad")
            .await
            .is_none()
    );
}

#[tokio::test]
async fn extended_mkcol() {
    let env = Env::new();
    let body = r#"<D:mkcol xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:carddav"><D:set><D:prop>
      <D:resourcetype><D:collection/><C:addressbook/></D:resourcetype>
      <D:displayname>Контакты</D:displayname><C:addressbook-description>Личные</C:addressbook-description>
    </D:prop></D:set></D:mkcol>"#;
    let r = env
        .send("MKCOL", &format!("{BOOKS}contacts/"), &[], body)
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let c = env
        .collection(CollectionKind::AddressBook, "contacts")
        .await
        .expect("создана");
    assert_eq!(c.display_name, "Контакты");
    assert_eq!(c.description.as_deref(), Some("Личные"));
    assert_eq!(c.components, BTreeSet::from([Component::Card]));

    let calendar = body.replace(
        "<C:addressbook/>",
        "<X:calendar xmlns:X=\"urn:ietf:params:xml:ns:caldav\"/>",
    );
    let r = env
        .send("MKCOL", &format!("{BOOKS}other/"), &[], &calendar)
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("<d:valid-resourcetype/>"), "{}", r.body);
    let r = env
        .send(
            "MKCOL",
            &format!("{BOOKS}x/"),
            &[],
            "<D:other xmlns:D=\"DAV:\"/>",
        )
        .await;
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let r = env
        .send("MKCOL", &format!("{BOOKS}contacts/"), &[], "")
        .await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn put_get_etag() {
    let env = Env::new();
    env.calendar("work").await;
    let path = format!("{CALS}work/ev1.ics");
    let body = event("ev-1", "Планёрка");
    let r = env
        .send("PUT", &path, &[("if-none-match", "*")], &body)
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    assert_eq!(r.header("etag"), Some("\"2\""));
    let r = env
        .send("PUT", &path, &[("if-none-match", "*")], &body)
        .await;
    assert_eq!(r.status, StatusCode::PRECONDITION_FAILED, "уже есть");

    let r = env.send("GET", &path, &[], "").await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.body, body, "тело без изменений");
    assert_eq!(r.header("etag"), Some("\"2\""));
    assert_eq!(
        r.header("content-type"),
        Some("text/calendar; charset=utf-8")
    );
    let r = env.send("HEAD", &path, &[], "").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.is_empty());
    assert_eq!(
        r.header("content-length"),
        Some(body.len().to_string().as_str())
    );

    let changed = event("ev-1", "Планёрка перенесена");
    let r = env
        .send("PUT", &path, &[("if-match", "\"2\"")], &changed)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert_eq!(r.header("etag"), Some("\"3\""));
    let r = env
        .send("PUT", &path, &[("if-match", "\"2\"")], &body)
        .await;
    assert_eq!(r.status, StatusCode::PRECONDITION_FAILED, "конфликт ETag");
    let r = env
        .send("PUT", &path, &[("if-match", "W/\"1\", \"3\"")], &body)
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT, "список ETag");
    let r = env.send("PUT", &path, &[("if-match", "*")], &body).await;
    assert_eq!(
        (r.status, r.header("etag")),
        (StatusCode::NO_CONTENT, Some("\"5\""))
    );
    let other = format!("{CALS}work/none.ics");
    let r = env.send("PUT", &other, &[("if-match", "*")], &body).await;
    assert_eq!(
        r.status,
        StatusCode::PRECONDITION_FAILED,
        "If-Match: * без ресурса"
    );

    let r = env
        .send("DELETE", &path, &[("if-match", "\"999\"")], "")
        .await;
    assert_eq!(r.status, StatusCode::PRECONDITION_FAILED);
    let r = env
        .send("DELETE", &path, &[("if-match", "\"5\"")], "")
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let r = env.send("GET", &path, &[], "").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = env.send("DELETE", &path, &[], "").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = env.send("GET", &format!("{CALS}work/"), &[], "").await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED, "GET коллекции");
}

#[tokio::test]
async fn put_preconditions() {
    let env = Env::with_config(DavConfig {
        max_object_size: 2_000,
        ..DavConfig::default()
    });
    env.calendar("work").await;
    env.put_event("work", "ev1.ics", "ev-1", "Первое").await;
    let cases = [
        (
            "bad.ics",
            "не календарь".to_owned(),
            "<c:valid-calendar-data/>",
        ),
        ("card.ics", vcard("c-1", "Иван"), "<c:valid-calendar-data/>"),
        (
            "big.ics",
            event("big", &"x".repeat(2_000)),
            "<c:max-resource-size/>",
        ),
        (
            "ev2.ics",
            event("ev-1", "Тот же UID"),
            "<c:no-uid-conflict><d:href>/dav/calendars/ivan@example.org/work/ev1.ics</d:href></c:no-uid-conflict>",
        ),
    ];
    for (name, body, want) in cases {
        let r = env
            .send("PUT", &format!("{CALS}work/{name}"), &[], &body)
            .await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{name}");
        assert!(r.body.contains(want), "{name}: {}", r.body);
    }
    let todo = event("t-1", "Задача").replace("VEVENT", "VTODO");
    env.create(Collection {
        components: BTreeSet::from([Component::Event]),
        ..Collection::new(CollectionKind::Calendar, "events")
    })
    .await;
    let r = env
        .send("PUT", &format!("{CALS}events/t.ics"), &[], &todo)
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("<c:supported-calendar-component/>"));
    let r = env
        .send("PUT", &format!("{CALS}none/t.ics"), &[], &todo)
        .await;
    assert_eq!(r.status, StatusCode::CONFLICT, "нет коллекции");
    let r = env.send("PUT", &format!("{CALS}work/"), &[], &todo).await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED, "PUT коллекции");
}

#[tokio::test]
async fn vcard_roundtrip() {
    let env = Env::new();
    let r = env
        .send("MKCOL", &format!("{BOOKS}contacts/"), &[], "")
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let path = format!("{BOOKS}contacts/ivan.vcf");
    let card = vcard("c-1", "Иван Петров");
    let r = env
        .send("PUT", &path, &[("content-type", "text/vcard")], &card)
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
    let r = env.send("GET", &path, &[], "").await;
    assert_eq!((r.status, r.body.as_str()), (StatusCode::OK, card.as_str()));
    assert_eq!(r.header("content-type"), Some("text/vcard; charset=utf-8"));
    let r = env
        .send(
            "PUT",
            &format!("{BOOKS}contacts/e.vcf"),
            &[],
            &event("e", "x"),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("<card:valid-address-data/>"));
}

#[tokio::test]
async fn delete_collection_with_objects() {
    let env = Env::new();
    env.calendar("work").await;
    env.put_event("work", "a.ics", "a", "A").await;
    env.put_event("work", "b.ics", "b", "B").await;
    let r = env.send("DELETE", &format!("{CALS}work/"), &[], "").await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    assert!(
        env.collection(CollectionKind::Calendar, "work")
            .await
            .is_none()
    );
    let r = env.propfind(&format!("{CALS}work/"), "0", "").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = env.send("GET", &format!("{CALS}work/a.ics"), &[], "").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = env.send("DELETE", &format!("{CALS}work/"), &[], "").await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let r = env.send("DELETE", CALS, &[], "").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "дом не удаляется");
}
