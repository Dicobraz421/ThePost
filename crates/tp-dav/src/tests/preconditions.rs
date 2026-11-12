//! DAV-08: предусловия `PUT` объекта календаря (RFC 4791 §5.3.2.1) — тест на каждое.

use axum::http::StatusCode;
use tp_contracts::dav::DavStore;

use super::query::{ical, vevent};
use super::{CALS, Env, Resp, event};
use crate::DavConfig;

async fn put(env: &Env, name: &str, body: &str) -> Resp {
    env.send(
        "PUT",
        &format!("{CALS}calendar/{name}"),
        &[("content-type", "text/calendar; charset=utf-8")],
        body,
    )
    .await
}

/// Дом с календарём по умолчанию (`calendar`, только VEVENT).
async fn env() -> Env {
    let env = Env::new();
    env.propfind(CALS, "0", "").await;
    env
}

/// Ответ — 403 с предусловием `inner`, объект не записан.
async fn rejected(env: &Env, name: &str, body: &str, inner: &str) {
    let r = put(env, name, body).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{name}: {}", r.body);
    assert!(r.body.contains(inner), "{name}: нет {inner}: {}", r.body);
    assert!(r.body.contains("<d:error"), "{name}: {}", r.body);
    let c = env
        .collection(tp_contracts::dav::CollectionKind::Calendar, "calendar")
        .await
        .expect("calendar");
    let stored = env
        .store
        .get_object(env.scope, c.id, name)
        .await
        .expect("хранилище");
    assert!(stored.is_none(), "{name} записан");
}

fn dated(uid: &str, start: &str, end: &str, extra: &str) -> String {
    ical(&vevent(
        uid,
        &format!("DTSTART:{start}\r\nDTEND:{end}\r\n{extra}"),
    ))
}

#[tokio::test]
async fn valid_calendar_data() {
    let env = env().await;
    let inner = "<c:valid-calendar-data/>";
    rejected(&env, "text.ics", "просто текст", inner).await;
    rejected(
        &env,
        "card.ics",
        "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:c\r\nFN:X\r\nEND:VCARD\r\n",
        inner,
    )
    .await;
    rejected(&env, "empty.ics", &ical(""), inner).await;
    // Не UTF-8.
    let req = axum::http::Request::builder()
        .method("PUT")
        .uri(format!("{CALS}calendar/bytes.ics"))
        .header("authorization", super::basic(super::LOGIN, "secret"))
        .body(axum::body::Body::from(vec![0xff, 0xfe, 0x00]))
        .expect("запрос");
    let r = env.raw(req).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains(inner), "{}", r.body);
}

#[tokio::test]
async fn valid_calendar_object_resource() {
    let env = env().await;
    let inner = "<c:valid-calendar-object-resource/>";
    let two_uids = ical(&format!(
        "{}{}",
        vevent("a", "DTSTART:20260105T100000Z\r\n"),
        vevent("b", "DTSTART:20260106T100000Z\r\n")
    ));
    rejected(&env, "two.ics", &two_uids, inner).await;
    let method = event("m", "Приглашение").replace("VERSION:2.0", "VERSION:2.0\r\nMETHOD:REQUEST");
    rejected(&env, "method.ics", &method, inner).await;
    let no_uid = event("x", "Без UID").replace("UID:x\r\n", "");
    rejected(&env, "nouid.ics", &no_uid, inner).await;
    let mixed = ical(&format!(
        "{}BEGIN:VTODO\r\nUID:a\r\nDTSTAMP:20260101T000000Z\r\nEND:VTODO\r\n",
        vevent("a", "DTSTART:20260105T100000Z\r\n")
    ));
    rejected(&env, "mixed.ics", &mixed, inner).await;
    // Основной компонент и переопределение с одним UID — один ресурс.
    let series = ical(&format!(
        "{}{}",
        vevent(
            "s",
            "DTSTART:20260106T090000Z\r\nDTEND:20260106T100000Z\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\n"
        ),
        vevent(
            "s",
            "RECURRENCE-ID:20260113T090000Z\r\nDTSTART:20260113T120000Z\r\nDTEND:20260113T130000Z\r\n"
        )
    ));
    assert_eq!(
        put(&env, "series.ics", &series).await.status,
        StatusCode::CREATED
    );
}

#[tokio::test]
async fn supported_calendar_component() {
    let env = env().await;
    let todo =
        ical("BEGIN:VTODO\r\nUID:t\r\nDTSTAMP:20260101T000000Z\r\nSUMMARY:Задача\r\nEND:VTODO\r\n");
    rejected(&env, "todo.ics", &todo, "<c:supported-calendar-component/>").await;
    let journal = ical(
        "BEGIN:VJOURNAL\r\nUID:j\r\nDTSTAMP:20260101T000000Z\r\nDTSTART:20260105T100000Z\r\nEND:VJOURNAL\r\n",
    );
    rejected(
        &env,
        "journal.ics",
        &journal,
        "<c:supported-calendar-component/>",
    )
    .await;
    let r = env
        .send("PUT", &format!("{CALS}tasks/todo.ics"), &[], &todo)
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "задача в списке задач");
}

#[tokio::test]
async fn no_uid_conflict() {
    let env = env().await;
    assert_eq!(
        put(&env, "first.ics", &event("same", "Первое"))
            .await
            .status,
        StatusCode::CREATED
    );
    rejected(
        &env,
        "second.ics",
        &event("same", "Второе"),
        "<c:no-uid-conflict><d:href>/dav/calendars/ivan@example.org/calendar/first.ics</d:href></c:no-uid-conflict>",
    )
    .await;
    // Тот же UID в том же ресурсе — правка; в другом календаре — отдельный объект.
    assert_eq!(
        put(&env, "first.ics", &event("same", "Правка"))
            .await
            .status,
        StatusCode::NO_CONTENT
    );
    env.calendar("work").await;
    let r = env
        .send(
            "PUT",
            &format!("{CALS}work/copy.ics"),
            &[],
            &event("same", "Копия"),
        )
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
}

#[tokio::test]
async fn max_resource_size() {
    let env = Env::with_config(DavConfig {
        max_object_size: 400,
        ..DavConfig::default()
    });
    env.propfind(CALS, "0", "").await;
    assert_eq!(
        put(&env, "small.ics", &event("small", "Коротко"))
            .await
            .status,
        StatusCode::CREATED
    );
    let big = event("big", &"Длинное описание ".repeat(40));
    rejected(&env, "big.ics", &big, "<c:max-resource-size/>").await;
    let get = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
      <d:prop><c:max-resource-size/></d:prop></d:propfind>"#;
    let r = env.propfind(&format!("{CALS}calendar/"), "0", get).await;
    assert!(
        r.body
            .contains("<c:max-resource-size>400</c:max-resource-size>"),
        "{}",
        r.body
    );
}

#[tokio::test]
async fn min_and_max_date_time() {
    let env = env().await;
    rejected(
        &env,
        "old.ics",
        &dated("old", "18991231T230000Z", "19000101T010000Z", ""),
        "<c:min-date-time/>",
    )
    .await;
    rejected(
        &env,
        "far.ics",
        &dated("far", "21010101T000000Z", "21010101T010000Z", ""),
        "<c:max-date-time/>",
    )
    .await;
    for (name, body) in [
        (
            "first.ics",
            dated("first", "19000101T000000Z", "19000101T010000Z", ""),
        ),
        (
            "last.ics",
            dated("last", "21001231T220000Z", "21001231T230000Z", ""),
        ),
        // Повтор без конца допустим.
        (
            "forever.ics",
            dated(
                "forever",
                "20260105T100000Z",
                "20260105T110000Z",
                "RRULE:FREQ=YEARLY\r\n",
            ),
        ),
    ] {
        assert_eq!(
            put(&env, name, &body).await.status,
            StatusCode::CREATED,
            "{name}"
        );
    }
    let get = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
      <d:prop><c:min-date-time/><c:max-date-time/></d:prop></d:propfind>"#;
    let r = env.propfind(&format!("{CALS}calendar/"), "0", get).await;
    for part in [
        "<c:min-date-time>19000101T000000Z</c:min-date-time>",
        "<c:max-date-time>21010101T000000Z</c:max-date-time>",
    ] {
        assert!(r.body.contains(part), "{part}: {}", r.body);
    }

    // Границы — из конфигурации.
    let narrow = Env::with_config(DavConfig {
        min_date_year: 2020,
        max_date_year: 2030,
        ..DavConfig::default()
    });
    narrow.propfind(CALS, "0", "").await;
    rejected(
        &narrow,
        "y2019.ics",
        &dated("y2019", "20190601T100000Z", "20190601T110000Z", ""),
        "<c:min-date-time/>",
    )
    .await;
    rejected(
        &narrow,
        "y2031.ics",
        &dated("y2031", "20310601T100000Z", "20310601T110000Z", ""),
        "<c:max-date-time/>",
    )
    .await;
}
