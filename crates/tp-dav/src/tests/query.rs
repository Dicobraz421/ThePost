//! DAV-06: `REPORT calendar-query` (фильтры, `time-range` с повторами, исключениями, датами «весь день» и
//! часовыми поясами, `expand`, `limit-recurrence-set`) и `free-busy-query`.

use axum::http::StatusCode;

use super::{CALS, Env, Resp};

/// VCALENDAR с одним или несколькими компонентами.
pub(crate) fn ical(components: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//RU\r\n{components}END:VCALENDAR\r\n"
    )
}

/// VEVENT с UID и свойствами `props` (строки с `\r\n`).
pub(crate) fn vevent(uid: &str, props: &str) -> String {
    format!("BEGIN:VEVENT\r\nUID:{uid}\r\nDTSTAMP:20260101T000000Z\r\n{props}END:VEVENT\r\n")
}

/// Еженедельная встреча по вторникам (6, 13, 20, 27 января 09:00Z): 13-е исключено, 20-е перенесено на 21-е 15:00Z.
fn weekly() -> String {
    ical(&format!(
        "{}{}",
        vevent(
            "weekly",
            "DTSTART:20260106T090000Z\r\nDTEND:20260106T100000Z\r\nRRULE:FREQ=WEEKLY;COUNT=4\r\n\
             EXDATE:20260113T090000Z\r\nSUMMARY:Статус\r\n"
        ),
        vevent(
            "weekly",
            "RECURRENCE-ID:20260120T090000Z\r\nDTSTART:20260121T150000Z\r\nDTEND:20260121T160000Z\r\n\
             SUMMARY:Перенос\r\n"
        )
    ))
}

async fn put(env: &Env, coll: &str, name: &str, body: &str) {
    let r = env
        .send("PUT", &format!("{CALS}{coll}/{name}"), &[], body)
        .await;
    assert!(r.status.is_success(), "PUT {name}: {} {}", r.status, r.body);
}

/// Календарь `work` с набором событий.
async fn dataset() -> Env {
    let env = Env::new();
    env.calendar("work").await;
    env.put_event("work", "single.ics", "ev-1", "Планёрка").await;
    put(&env, "work", "weekly.ics", &weekly()).await;
    put(
        &env,
        "work",
        "allday.ics",
        &ical(&vevent(
            "allday",
            "DTSTART;VALUE=DATE:20260110\r\nDTEND;VALUE=DATE:20260111\r\nSUMMARY:Выходной\r\n",
        )),
    )
    .await;
    put(
        &env,
        "work",
        "moscow.ics",
        &ical(&vevent(
            "moscow",
            "DTSTART;TZID=Europe/Moscow:20260112T100000\r\nDTEND;TZID=Europe/Moscow:20260112T110000\r\n\
             SUMMARY:Встреча с заказчиком\r\nLOCATION:Переговорная 3\r\n\
             ATTENDEE;PARTSTAT=NEEDS-ACTION;CN=Пётр:mailto:petr@example.org\r\n",
        )),
    )
    .await;
    env
}

fn query_body(filter: &str, data: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<C:calendar-query xmlns:D="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav">
  <D:prop><D:getetag/>{data}</D:prop>
  <C:filter><C:comp-filter name="VCALENDAR">{filter}</C:comp-filter></C:filter>
</C:calendar-query>"#
    )
}

async fn query(env: &Env, coll: &str, filter: &str, data: &str) -> Resp {
    env.send(
        "REPORT",
        &format!("{CALS}{coll}/"),
        &[("depth", "1")],
        &query_body(filter, data),
    )
    .await
}

/// Имена ресурсов ответа по порядку.
pub(crate) fn names(r: &Resp) -> Vec<String> {
    assert_eq!(r.status, StatusCode::MULTI_STATUS, "{}", r.body);
    let mut out: Vec<String> = r
        .body
        .split("<d:href>")
        .skip(1)
        .filter_map(|s| s.split("</d:href>").next())
        .filter_map(|h| h.rsplit('/').next())
        .map(str::to_owned)
        .collect();
    out.sort();
    out
}

fn range(comp: &str, start: &str, end: &str) -> String {
    format!(
        r#"<C:comp-filter name="{comp}"><C:time-range start="{start}" end="{end}"/></C:comp-filter>"#
    )
}

async fn in_range(env: &Env, start: &str, end: &str) -> Vec<String> {
    names(&query(env, "work", &range("VEVENT", start, end), "").await)
}

#[tokio::test]
async fn time_range_recurrence() {
    let env = dataset().await;
    let cases = [
        ("20260105T000000Z", "20260106T000000Z", vec!["single.ics"]),
        ("20260106T093000Z", "20260106T094000Z", vec!["weekly.ics"]),
        // Конец вхождения — не в интервале, начало интервала — граница.
        ("20260106T100000Z", "20260106T120000Z", vec![]),
        // 13-е исключено EXDATE.
        ("20260113T000000Z", "20260114T000000Z", vec![]),
        // 20-е перенесено RECURRENCE-ID на 21-е.
        ("20260120T000000Z", "20260121T000000Z", vec![]),
        ("20260121T120000Z", "20260121T180000Z", vec!["weekly.ics"]),
        ("20260127T000000Z", "20260128T000000Z", vec!["weekly.ics"]),
        // COUNT=4: дальше вхождений нет.
        ("20260203T000000Z", "20260301T000000Z", vec![]),
        (
            "20260101T000000Z",
            "20260201T000000Z",
            vec!["allday.ics", "moscow.ics", "single.ics", "weekly.ics"],
        ),
    ];
    for (start, end, want) in cases {
        assert_eq!(in_range(&env, start, end).await, want, "{start}–{end}");
    }
    // Открытые границы.
    let open = r#"<C:comp-filter name="VEVENT"><C:time-range start="20260127T000000Z"/></C:comp-filter>"#;
    assert_eq!(names(&query(&env, "work", open, "").await), ["weekly.ics"]);
    let open = r#"<C:comp-filter name="VEVENT"><C:time-range end="20260105T110000Z"/></C:comp-filter>"#;
    assert_eq!(names(&query(&env, "work", open, "").await), ["single.ics"]);
}

#[tokio::test]
async fn time_range_all_day_and_time_zones() {
    let env = dataset().await;
    // Событие в поясе Europe/Moscow: 10:00 MSK = 07:00Z.
    assert_eq!(
        in_range(&env, "20260112T063000Z", "20260112T073000Z").await,
        ["moscow.ics"]
    );
    assert!(
        in_range(&env, "20260112T090000Z", "20260112T120000Z")
            .await
            .is_empty()
    );
    // «Весь день» 10 января: в календаре без пояса — сутки UTC.
    let late = ("20260110T213000Z", "20260110T220000Z");
    assert_eq!(in_range(&env, late.0, late.1).await, ["allday.ics"]);
    // В поясе календаря Europe/Moscow тот же день — 9 января 21:00Z … 10 января 21:00Z.
    let patch = r#"<d:propertyupdate xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
      <d:set><d:prop><c:calendar-timezone-id>Europe/Moscow</c:calendar-timezone-id></d:prop></d:set>
    </d:propertyupdate>"#;
    env.send("PROPPATCH", &format!("{CALS}work/"), &[], patch)
        .await;
    assert!(in_range(&env, late.0, late.1).await.is_empty());
    assert_eq!(
        in_range(&env, "20260109T213000Z", "20260109T220000Z").await,
        ["allday.ics"]
    );
    // Пояс запроса важнее пояса календаря.
    let body = query_body(&range("VEVENT", late.0, late.1), "").replace(
        "<C:filter>",
        "<C:timezone>BEGIN:VCALENDAR\nVERSION:2.0\nBEGIN:VTIMEZONE\nTZID:Etc/UTC\nEND:VTIMEZONE\nEND:VCALENDAR\n</C:timezone><C:filter>",
    );
    let r = env
        .send("REPORT", &format!("{CALS}work/"), &[("depth", "1")], &body)
        .await;
    assert_eq!(names(&r), ["allday.ics"]);
}

fn prop(name: &str, inner: &str) -> String {
    format!(
        r#"<C:comp-filter name="VEVENT"><C:prop-filter name="{name}">{inner}</C:prop-filter></C:comp-filter>"#
    )
}

#[tokio::test]
async fn prop_param_and_text_filters() {
    let env = dataset().await;
    let all = ["allday.ics", "moscow.ics", "single.ics", "weekly.ics"];
    let cases: Vec<(String, Vec<&str>)> = vec![
        // Без условий и только по виду компонента.
        (String::new(), all.to_vec()),
        (r#"<C:comp-filter name="VEVENT"/>"#.to_owned(), all.to_vec()),
        (r#"<C:comp-filter name="VTODO"/>"#.to_owned(), vec![]),
        (
            r#"<C:comp-filter name="VTODO"><C:is-not-defined/></C:comp-filter>"#.to_owned(),
            all.to_vec(),
        ),
        // i;unicode-casemap: регистр кириллицы не важен.
        (
            prop(
                "SUMMARY",
                r#"<C:text-match collation="i;unicode-casemap">ПЛАНЁР</C:text-match>"#,
            ),
            vec!["single.ics"],
        ),
        // i;ascii-casemap (по умолчанию): сворачиваются только буквы ASCII.
        (
            prop("SUMMARY", "<C:text-match>ПЛАНЁР</C:text-match>"),
            vec![],
        ),
        (
            prop("SUMMARY", "<C:text-match>ланёрка</C:text-match>"),
            vec!["single.ics"],
        ),
        // Подходит любой компонент объекта: переопределение «Перенос».
        (
            prop("SUMMARY", "<C:text-match>Перенос</C:text-match>"),
            vec!["weekly.ics"],
        ),
        (
            prop(
                "SUMMARY",
                r#"<C:text-match negate-condition="yes">Статус</C:text-match>"#,
            ),
            all.to_vec(),
        ),
        (
            prop(
                "SUMMARY",
                r#"<C:text-match collation="i;octet">Выходной</C:text-match>"#,
            ),
            vec!["allday.ics"],
        ),
        (prop("LOCATION", ""), vec!["moscow.ics"]),
        (
            prop("LOCATION", "<C:is-not-defined/>"),
            vec!["allday.ics", "single.ics", "weekly.ics"],
        ),
        (
            prop(
                "ATTENDEE",
                r#"<C:text-match>PETR@example.org</C:text-match>
                   <C:param-filter name="PARTSTAT"><C:text-match>needs-action</C:text-match></C:param-filter>"#,
            ),
            vec!["moscow.ics"],
        ),
        (
            prop(
                "ATTENDEE",
                r#"<C:param-filter name="PARTSTAT"><C:text-match>ACCEPTED</C:text-match></C:param-filter>"#,
            ),
            vec![],
        ),
        (
            prop(
                "ATTENDEE",
                r#"<C:param-filter name="ROLE"><C:is-not-defined/></C:param-filter>"#,
            ),
            vec!["moscow.ics"],
        ),
        // Свойство с датой: текст — запись iCalendar, time-range — по времени UTC.
        (
            prop("RRULE", "<C:text-match>FREQ=WEEKLY</C:text-match>"),
            vec!["weekly.ics"],
        ),
        (
            prop(
                "DTSTART",
                r#"<C:time-range start="20260112T000000Z" end="20260113T000000Z"/>"#,
            ),
            vec!["moscow.ics"],
        ),
    ];
    for (filter, want) in cases {
        let r = query(&env, "work", &filter, "").await;
        assert_eq!(names(&r), want, "{filter}");
    }
}

#[tokio::test]
async fn expand_and_limit_recurrence_set() {
    let env = dataset().await;
    let filter = range("VEVENT", "20260101T000000Z", "20260201T000000Z");
    let data = r#"<C:calendar-data><C:expand start="20260107T000000Z" end="20260201T000000Z"/></C:calendar-data>"#;
    let body = query_body(
        &prop("UID", "<C:text-match>weekly</C:text-match>"),
        data,
    );
    let r = env
        .send("REPORT", &format!("{CALS}work/"), &[("depth", "1")], &body)
        .await;
    insta::assert_snapshot!("expand_weekly", r.show());

    // «Весь день» остаётся датой.
    let data = r#"<C:calendar-data><C:expand start="20260101T000000Z" end="20260201T000000Z"/></C:calendar-data>"#;
    let body = query_body(&prop("UID", "<C:text-match>allday</C:text-match>"), data);
    let r = env
        .send("REPORT", &format!("{CALS}work/"), &[("depth", "1")], &body)
        .await;
    for part in [
        "DTSTART;VALUE=DATE:20260110",
        "DTEND;VALUE=DATE:20260111",
    ] {
        assert!(r.body.contains(part), "{part}: {}", r.body);
    }
    assert!(!r.body.contains("RECURRENCE-ID"), "{}", r.body);

    // limit-recurrence-set: переопределение 21 января вне интервала — остаётся основной компонент.
    let data = r#"<C:calendar-data><C:limit-recurrence-set start="20260106T000000Z" end="20260107T000000Z"/></C:calendar-data>"#;
    let r = query(&env, "work", &filter, data).await;
    assert!(r.body.contains("RRULE:FREQ=WEEKLY;COUNT=4"), "{}", r.body);
    assert!(!r.body.contains("Перенос"), "{}", r.body);
    let data = r#"<C:calendar-data><C:limit-recurrence-set start="20260121T000000Z" end="20260122T000000Z"/></C:calendar-data>"#;
    let r = query(&env, "work", &filter, data).await;
    assert!(r.body.contains("Перенос"), "{}", r.body);
    // Без параметров тело возвращается как записано.
    let r = query(&env, "work", &filter, "<C:calendar-data/>").await;
    assert!(r.body.contains("EXDATE:20260113T090000Z"), "{}", r.body);
}

#[tokio::test]
async fn errors_and_limits() {
    let env = dataset().await;
    let report = |body: String| {
        let env = &env;
        async move {
            env.send("REPORT", &format!("{CALS}work/"), &[("depth", "1")], &body)
                .await
        }
    };
    let cases = [
        (
            r#"<C:comp-filter name="VEVENT"><C:time-range start="2026-01-01"/></C:comp-filter>"#,
            "<c:valid-filter/>",
        ),
        (
            r#"<C:comp-filter name="VEVENT"><C:time-range start="20260201T000000Z" end="20260101T000000Z"/></C:comp-filter>"#,
            "<c:valid-filter/>",
        ),
        (
            r#"<C:comp-filter name="VEVENT"><C:comp-filter name="VALARM"><C:time-range start="20260101T000000Z"/></C:comp-filter></C:comp-filter>"#,
            "<c:supported-filter/>",
        ),
        (
            r#"<C:comp-filter name="VEVENT"><C:prop-filter name="SUMMARY"><C:text-match collation="i;klingon">x</C:text-match></C:prop-filter></C:comp-filter>"#,
            "<c:supported-collation/>",
        ),
    ];
    for (filter, want) in cases {
        let r = report(query_body(filter, "")).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{filter}");
        assert!(r.body.contains(want), "{filter}: {}", r.body);
    }
    let no_filter = r#"<C:calendar-query xmlns:C="urn:ietf:params:xml:ns:caldav"/>"#;
    let r = report(no_filter.to_owned()).await;
    assert!(r.body.contains("<c:valid-filter/>"), "{}", r.body);
    let top = query_body("", "").replace("VCALENDAR", "VEVENT");
    assert!(report(top).await.body.contains("<c:valid-filter/>"));
    let unknown = r#"<C:calendar-search xmlns:C="urn:ietf:params:xml:ns:caldav"/>"#;
    let r = report(unknown.to_owned()).await;
    assert!(r.body.contains("<d:supported-report/>"), "{}", r.body);
    // Отчёт CalDAV у адресной книги и у дома не поддерживается.
    let r = env
        .send("REPORT", CALS, &[("depth", "1")], &query_body("", ""))
        .await;
    assert!(r.body.contains("<d:supported-report/>"), "{}", r.body);
    let r = env
        .send(
            "REPORT",
            &format!("{CALS}none/"),
            &[("depth", "1")],
            &query_body("", ""),
        )
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    // Лимит ответа.
    let small = Env::with_config(crate::DavConfig {
        max_response_items: 2,
        ..crate::DavConfig::default()
    });
    small.calendar("work").await;
    for n in 0..3 {
        small
            .put_event("work", &format!("e{n}.ics"), &format!("u{n}"), "x")
            .await;
    }
    let r = query(&small, "work", "", "").await;
    assert!(
        r.body.contains("<d:number-of-matches-within-limits/>"),
        "{}",
        r.body
    );
    assert_eq!(r.body.matches("<d:getetag>").count(), 2);
}

#[tokio::test]
async fn free_busy() {
    let env = dataset().await;
    for (name, props) in [
        // Пересекается с «Планёркой» (10:00–11:00): периоды сливаются.
        ("overlap", "DTSTART:20260105T103000Z\r\nDTEND:20260105T120000Z\r\n"),
        (
            "tentative",
            "DTSTART:20260105T140000Z\r\nDTEND:20260105T150000Z\r\nSTATUS:TENTATIVE\r\n",
        ),
        (
            "transparent",
            "DTSTART:20260105T160000Z\r\nDTEND:20260105T170000Z\r\nTRANSP:TRANSPARENT\r\n",
        ),
        (
            "cancelled",
            "DTSTART:20260105T180000Z\r\nDTEND:20260105T190000Z\r\nSTATUS:CANCELLED\r\n",
        ),
        // Выходит за конец интервала: обрезается.
        ("late", "DTSTART:20260106T080000Z\r\nDTEND:20260106T093000Z\r\n"),
    ] {
        put(&env, "work", &format!("{name}.ics"), &ical(&vevent(name, props))).await;
    }
    let body = r#"<C:free-busy-query xmlns:C="urn:ietf:params:xml:ns:caldav">
      <C:time-range start="20260105T000000Z" end="20260106T091500Z"/></C:free-busy-query>"#;
    let r = env
        .send("REPORT", &format!("{CALS}work/"), &[("depth", "1")], body)
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert_eq!(
        r.header("content-type"),
        Some("text/calendar; charset=utf-8")
    );
    let unfolded = r.body.replace("\r\n ", "");
    let lines: Vec<&str> = unfolded
        .lines()
        .filter(|l| !l.starts_with("DTSTAMP"))
        .collect();
    assert_eq!(
        lines,
        [
            "BEGIN:VCALENDAR",
            "VERSION:2.0",
            "PRODID:-//The Post//DAV//RU",
            "BEGIN:VFREEBUSY",
            "DTSTART:20260105T000000Z",
            "DTEND:20260106T091500Z",
            "FREEBUSY;FBTYPE=BUSY:20260105T100000Z/20260105T120000Z,20260106T080000Z/20260106T091500Z",
            "FREEBUSY;FBTYPE=BUSY-TENTATIVE:20260105T140000Z/20260105T150000Z",
            "END:VFREEBUSY",
            "END:VCALENDAR",
        ]
    );
    let bad = r#"<C:free-busy-query xmlns:C="urn:ietf:params:xml:ns:caldav">
      <C:time-range start="20260105T000000Z"/></C:free-busy-query>"#;
    let r = env
        .send("REPORT", &format!("{CALS}work/"), &[], bad)
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}
