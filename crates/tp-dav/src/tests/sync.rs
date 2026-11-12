//! DAV-04: REPORT sync-collection — начальная синхронизация, изменения, удаления, страницы по `limit`,
//! устаревший токен; `getctag` и `sync-token` в PROPFIND.

use axum::http::StatusCode;

use super::{CALS, Env};

fn sync(token: &str, limit: Option<usize>) -> String {
    let limit = limit
        .map(|n| format!("<d:limit><d:nresults>{n}</d:nresults></d:limit>"))
        .unwrap_or_default();
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<d:sync-collection xmlns:d="DAV:"><d:sync-token>{token}</d:sync-token><d:sync-level>1</d:sync-level>{limit}<d:prop><d:getetag/></d:prop></d:sync-collection>"#
    )
}

async fn report(env: &Env, body: &str) -> super::Resp {
    env.send("REPORT", &format!("{CALS}work/"), &[("depth", "0")], body)
        .await
}

fn ok(name: &str, etag: u64) -> String {
    format!(
        "<d:response><d:href>/dav/calendars/ivan@example.org/work/{name}</d:href><d:propstat><d:prop><d:getetag>\"{etag}\"</d:getetag></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response>"
    )
}

fn gone(name: &str) -> String {
    format!(
        "<d:response><d:href>/dav/calendars/ivan@example.org/work/{name}</d:href><d:status>HTTP/1.1 404 Not Found</d:status></d:response>"
    )
}

const MORE: &str = "<d:status>HTTP/1.1 507 Insufficient Storage</d:status><d:error><d:number-of-matches-within-limits/></d:error>";

#[tokio::test]
async fn initial_changes_and_deletes() {
    let env = Env::new();
    env.calendar("work").await;
    env.put_event("work", "a.ics", "a", "A").await;
    env.put_event("work", "b.ics", "b", "B").await;

    let r = report(&env, &sync("", None)).await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    let tail = r.body.split_once("ical/\">").expect("корень").1;
    assert_eq!(
        tail,
        format!(
            "{}{}<d:sync-token>urn:thepost:sync:3</d:sync-token></d:multistatus>",
            ok("a.ics", 2),
            ok("b.ics", 3)
        )
    );

    env.put_event("work", "a.ics", "a", "A2").await;
    let r = env
        .send("DELETE", &format!("{CALS}work/b.ics"), &[], "")
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    env.put_event("work", "c.ics", "c", "C").await;

    let r = report(&env, &sync("urn:thepost:sync:3", None)).await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    for part in [ok("a.ics", 4), ok("c.ics", 6), gone("b.ics")] {
        assert!(r.body.contains(&part), "{part}\n{}", r.body);
    }
    assert!(
        r.body
            .contains("<d:sync-token>urn:thepost:sync:6</d:sync-token>")
    );
    assert!(!r.body.contains("507"));

    let r = report(&env, &sync("urn:thepost:sync:6", None)).await;
    let tail = r.body.split_once("ical/\">").expect("корень").1;
    assert_eq!(
        tail, "<d:sync-token>urn:thepost:sync:6</d:sync-token></d:multistatus>",
        "без изменений"
    );

    let props = r#"<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"><d:prop><cs:getctag/><d:sync-token/></d:prop></d:propfind>"#;
    let r = env.propfind(&format!("{CALS}work/"), "0", props).await;
    assert!(
        r.body
            .contains("<cs:getctag>6</cs:getctag><d:sync-token>urn:thepost:sync:6</d:sync-token>"),
        "{}",
        r.body
    );
}

#[tokio::test]
async fn pages_by_limit() {
    let env = Env::new();
    env.calendar("work").await;
    for n in ["a", "b", "c"] {
        env.put_event("work", &format!("{n}.ics"), n, n).await;
    }
    let r = report(&env, &sync("", Some(2))).await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert!(r.body.contains(&ok("a.ics", 2)) && r.body.contains(&ok("b.ics", 3)));
    assert!(!r.body.contains("c.ics"));
    assert!(
        r.body.contains(&format!(
            "<d:response><d:href>/dav/calendars/ivan@example.org/work/</d:href>{MORE}</d:response>"
        )),
        "{}",
        r.body
    );
    assert!(
        r.body
            .contains("<d:sync-token>urn:thepost:sync:3</d:sync-token>")
    );

    let r = report(&env, &sync("urn:thepost:sync:3", Some(2))).await;
    assert!(r.body.contains(&ok("c.ics", 4)), "{}", r.body);
    assert!(!r.body.contains("507"));
    assert!(
        r.body
            .contains("<d:sync-token>urn:thepost:sync:4</d:sync-token>")
    );
}

#[tokio::test]
async fn invalid_tokens_and_reports() {
    let env = Env::new();
    env.calendar("work").await;
    env.put_event("work", "a.ics", "a", "A").await;
    env.put_event("work", "b.ics", "b", "B").await;
    let r = env
        .send("DELETE", &format!("{CALS}work/b.ics"), &[], "")
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    env.store.purge_tombstones(env.scope);
    for token in [
        "urn:thepost:sync:2",
        "urn:thepost:sync:999",
        "http://example.org/sync/1",
        "urn:thepost:sync:x",
    ] {
        let r = report(&env, &sync(token, None)).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{token}");
        assert!(
            r.body.contains("<d:valid-sync-token/>"),
            "{token}: {}",
            r.body
        );
    }
    let r = report(&env, &sync("", None)).await;
    assert_eq!(
        r.status,
        StatusCode::MULTI_STATUS,
        "полная синхронизация после очистки"
    );
    assert!(r.body.contains(&ok("a.ics", 2)));

    let infinite = sync("", None).replace("<d:sync-level>1<", "<d:sync-level>infinite<");
    let r = report(&env, &infinite).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("<d:sync-traversal-supported/>"));

    let query = r#"<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/></d:prop></c:calendar-query>"#;
    let r = report(&env, query).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("<d:supported-report/>"));
    let r = env.send("REPORT", CALS, &[], &sync("", None)).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "отчёт дома");
    let r = env
        .send("REPORT", &format!("{CALS}none/"), &[], &sync("", None))
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
    let empty_limit = sync("", Some(1)).replace("<d:nresults>1</d:nresults>", "");
    let r = report(&env, &empty_limit).await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS, "limit без nresults");
    let bad_limit = sync("", Some(1)).replace(">1</d:nresults>", ">много</d:nresults>");
    let r = report(&env, &bad_limit).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}
