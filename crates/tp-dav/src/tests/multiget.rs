//! DAV-07: `REPORT calendar-multiget` и задачи VTODO (своя коллекция, `time-range` по RFC 4791 §9.9, снимки
//! запросов синхронизации задач Thunderbird).

use axum::http::StatusCode;
use tp_contracts::dav::CollectionKind;

use super::query::{ical, names};
use super::{CALS, Env, Resp};
use crate::DavConfig;

fn multiget_body(prop: &str, hrefs: &[&str]) -> String {
    let hrefs: String = hrefs
        .iter()
        .map(|h| format!("<D:href>{h}</D:href>"))
        .collect();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-multiget xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop>{prop}</D:prop>{hrefs}
</C:calendar-multiget>"#
    )
}

async fn report(env: &Env, coll: &str, body: &str) -> Resp {
    env.send("REPORT", &format!("{CALS}{coll}/"), &[("depth", "1")], body)
        .await
}

#[tokio::test]
async fn multiget_mixed() {
    let env = Env::new();
    env.calendar("work").await;
    env.calendar("other").await;
    env.put_event("work", "a.ics", "a", "Первая").await;
    env.put_event("work", "b%20c.ics", "b", "Вторая").await;
    env.put_event("other", "x.ics", "x", "Чужой календарь").await;
    let hrefs = [
        "/dav/calendars/ivan@example.org/work/b%20c.ics",
        "/dav/calendars/ivan@example.org/work/missing.ics",
        "https://mail.example.org/dav/calendars/ivan@example.org/work/a.ics",
        "/dav/calendars/ivan@example.org/other/x.ics",
        "/dav/calendars/petr@example.org/work/a.ics",
        "/somewhere/else.ics",
    ];
    let r = report(
        &env,
        "work",
        &multiget_body("<D:getetag/><C:calendar-data/>", &hrefs),
    )
    .await;
    insta::assert_snapshot!("multiget_mixed", r.show());

    // Без свойств — только существующие ссылки и 404.
    let r = report(&env, "work", &multiget_body("", &hrefs[..2])).await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert_eq!(r.body.matches("404 Not Found").count(), 1, "{}", r.body);
    assert_eq!(r.body.matches("200 OK").count(), 1, "{}", r.body);
}

#[tokio::test]
async fn multiget_limit_and_expand() {
    let env = Env::with_config(DavConfig {
        max_multiget_hrefs: 2,
        ..DavConfig::default()
    });
    env.calendar("work").await;
    let daily = ical(
        "BEGIN:VEVENT\r\nUID:daily\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260105T100000Z\r\n\
         DTEND:20260105T103000Z\r\nRRULE:FREQ=DAILY;COUNT=5\r\nSUMMARY:Зарядка\r\nEND:VEVENT\r\n",
    );
    let path = format!("{CALS}work/daily.ics");
    assert!(env.send("PUT", &path, &[], &daily).await.status.is_success());
    let three = [path.as_str(), path.as_str(), path.as_str()];
    let r = report(&env, "work", &multiget_body("<D:getetag/>", &three)).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(
        r.body.contains("<d:number-of-matches-within-limits/>"),
        "{}",
        r.body
    );
    let data = r#"<C:calendar-data><C:expand start="20260106T000000Z" end="20260108T000000Z"/></C:calendar-data>"#;
    let r = report(&env, "work", &multiget_body(data, &three[..1])).await;
    assert_eq!(r.body.matches("BEGIN:VEVENT").count(), 2, "{}", r.body);
    assert!(r.body.contains("RECURRENCE-ID:20260106T100000Z"));
    assert!(!r.body.contains("RRULE"), "{}", r.body);
}

fn vtodo(uid: &str, props: &str) -> String {
    ical(&format!(
        "BEGIN:VTODO\r\nUID:{uid}\r\nDTSTAMP:20260101T000000Z\r\nSUMMARY:{uid}\r\n{props}END:VTODO\r\n"
    ))
}

/// Список задач по умолчанию с задачами на все строки таблицы RFC 4791 §9.9.
async fn tasks() -> Env {
    let env = Env::new();
    env.propfind(CALS, "0", "").await;
    for (uid, props) in [
        ("start-due", "DTSTART:20260110T090000Z\r\nDUE:20260112T180000Z\r\n"),
        ("start-duration", "DTSTART:20260110T090000Z\r\nDURATION:PT2H\r\n"),
        ("start", "DTSTART:20260110T090000Z\r\n"),
        ("due", "DUE:20260115T120000Z\r\n"),
        (
            "done",
            "CREATED:20260103T000000Z\r\nCOMPLETED:20260104T000000Z\r\nSTATUS:COMPLETED\r\n",
        ),
        ("completed", "COMPLETED:20260120T000000Z\r\n"),
        ("created", "CREATED:20260108T000000Z\r\n"),
        ("no-dates", ""),
        (
            "weekly",
            "DTSTART:20260105T080000Z\r\nDUE:20260105T090000Z\r\nRRULE:FREQ=WEEKLY;COUNT=3\r\n",
        ),
    ] {
        let r = env
            .send(
                "PUT",
                &format!("{CALS}tasks/{uid}.ics"),
                &[("content-type", "text/calendar")],
                &vtodo(uid, props),
            )
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{uid}: {}", r.body);
    }
    env
}

#[tokio::test]
async fn tasks_collection_accepts_only_vtodo() {
    let env = tasks().await;
    let r = env
        .send(
            "PUT",
            &format!("{CALS}tasks/event.ics"),
            &[],
            &super::event("ev", "Событие"),
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(
        r.body.contains("<c:supported-calendar-component/>"),
        "{}",
        r.body
    );
    let r = env
        .send("PUT", &format!("{CALS}calendar/t.ics"), &[], &vtodo("t", ""))
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "задача в календаре событий");
    let c = env
        .collection(CollectionKind::Calendar, "tasks")
        .await
        .expect("tasks");
    assert_eq!(c.display_name, "Задачи");
}

#[tokio::test]
async fn vtodo_time_range() {
    let env = tasks().await;
    let query = |start: &'static str, end: &'static str| {
        let env = &env;
        async move {
            let body = format!(
                r#"<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/></D:prop>
  <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VTODO">
    <C:time-range start="{start}" end="{end}"/></C:comp-filter></C:comp-filter></C:filter>
</C:calendar-query>"#
            );
            names(&report(env, "tasks", &body).await)
        }
    };
    // Задача без дат и задача только с CREATED (до конца интервала) подходят всегда.
    let cases = [
        (
            "20260110T100000Z",
            "20260110T110000Z",
            vec!["created.ics", "no-dates.ics", "start-due.ics", "start-duration.ics"],
        ),
        // DTSTART без срока — только момент начала; DTSTART+DURATION — до 11:00.
        (
            "20260110T090000Z",
            "20260110T090001Z",
            vec![
                "created.ics",
                "no-dates.ics",
                "start-due.ics",
                "start-duration.ics",
                "start.ics",
            ],
        ),
        // DUE: start < DUE ≤ end.
        (
            "20260115T110000Z",
            "20260115T120000Z",
            vec!["created.ics", "due.ics", "no-dates.ics"],
        ),
        (
            "20260115T120000Z",
            "20260115T130000Z",
            vec!["created.ics", "no-dates.ics"],
        ),
        // CREATED и COMPLETED: между ними; до CREATED задача «created» не подходит.
        (
            "20260103T120000Z",
            "20260103T130000Z",
            vec!["done.ics", "no-dates.ics"],
        ),
        // Только COMPLETED: интервал содержит момент завершения (и третье вхождение повтора 19 января).
        (
            "20260119T000000Z",
            "20260121T000000Z",
            vec!["completed.ics", "created.ics", "no-dates.ics", "weekly.ics"],
        ),
        // Повтор: второе вхождение 12 января 08:00–09:00.
        (
            "20260112T083000Z",
            "20260112T084000Z",
            vec!["created.ics", "no-dates.ics", "start-due.ics", "weekly.ics"],
        ),
    ];
    for (start, end, want) in cases {
        assert_eq!(query(start, end).await, want, "{start}–{end}");
    }
}

/// Thunderbird: список задач (`calendar-query` по VTODO), затем `calendar-multiget` изменённых.
#[tokio::test]
async fn thunderbird_tasks_sync_snapshot() {
    let env = Env::new();
    env.propfind(CALS, "0", "").await;
    for (uid, props) in [
        ("buy", "DUE;VALUE=DATE:20260115\r\nPRIORITY:1\r\n"),
        ("call", "STATUS:COMPLETED\r\nCOMPLETED:20260104T100000Z\r\nPERCENT-COMPLETE:100\r\n"),
    ] {
        let r = env
            .send("PUT", &format!("{CALS}tasks/{uid}.ics"), &[], &vtodo(uid, props))
            .await;
        assert_eq!(r.status, StatusCode::CREATED);
    }
    let query = r#"<?xml version="1.0" encoding="UTF-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/></D:prop>
  <C:filter><C:comp-filter name="VCALENDAR"><C:comp-filter name="VTODO"/></C:comp-filter></C:filter>
</C:calendar-query>"#;
    let r = report(&env, "tasks", query).await;
    insta::assert_snapshot!("thunderbird_tasks_query", r.show());
    let multiget = multiget_body(
        "<D:getetag/><C:calendar-data/>",
        &[
            "/dav/calendars/ivan%40example.org/tasks/buy.ics",
            "/dav/calendars/ivan%40example.org/tasks/call.ics",
        ],
    );
    let r = report(&env, "tasks", &multiget).await;
    insta::assert_snapshot!("thunderbird_tasks_multiget", r.show());
}
