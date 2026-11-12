//! DAV-01: вход Basic и Bearer, 401, чужой принципал — 403, корень и принципал, OPTIONS; DAV-26: адрес клиента.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tp_contracts::Error;

use super::{Env, LOGIN, basic};
use crate::{ALLOW, DAV_HEADER};

const DISCOVERY: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><d:current-user-principal/><d:displayname/><c:calendar-home-set/><d:principal-URL/></d:prop>
</d:propfind>"#;

fn propfind(path: &str, auth: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("PROPFIND")
        .uri(path)
        .header("depth", "0");
    if let Some(a) = auth {
        b = b.header("authorization", a);
    }
    b.body(Body::from(DISCOVERY)).expect("запрос")
}

#[tokio::test]
async fn unauthorized() {
    let env = Env::new();
    let r = env.raw(propfind("/dav/", None)).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(
        r.header("www-authenticate"),
        Some("Basic realm=\"dav\", charset=\"UTF-8\"")
    );
    let wrong = [
        basic(LOGIN, "wrong"),
        basic("nobody@example.org", "secret"),
        "Basic !!!".to_owned(),
        "Digest username=\"x\"".to_owned(),
        "Bearer nope".to_owned(),
        "Basic".to_owned(),
    ];
    for a in wrong {
        let r = env.raw(propfind("/dav/", Some(a.as_str()))).await;
        assert_eq!(r.status, StatusCode::UNAUTHORIZED, "{a}");
    }
}

#[tokio::test]
async fn basic_and_bearer() {
    let env = Env::new();
    for a in [basic(LOGIN, "secret"), "Bearer token-1".to_owned()] {
        let r = env
            .raw(propfind(
                "/dav/principals/ivan@example.org/",
                Some(a.as_str()),
            ))
            .await;
        assert_eq!(r.status, StatusCode::MULTI_STATUS, "{a}");
        assert_eq!(
            r.header("content-type"),
            Some("application/xml; charset=utf-8")
        );
        for part in [
            "<d:href>/dav/principals/ivan@example.org/</d:href><d:propstat><d:prop>",
            "<d:current-user-principal><d:href>/dav/principals/ivan@example.org/</d:href></d:current-user-principal>",
            "<d:displayname>ivan@example.org</d:displayname>",
            "<c:calendar-home-set><d:href>/dav/calendars/ivan@example.org/</d:href></c:calendar-home-set>",
            "<d:principal-URL><d:href>/dav/principals/ivan@example.org/</d:href></d:principal-URL>",
        ] {
            assert!(r.body.contains(part), "{part}\n{}", r.body);
        }
    }
}

#[tokio::test]
async fn foreign_principal_forbidden() {
    let env = Env::new();
    let auth = basic(LOGIN, "secret");
    for path in [
        "/dav/principals/petr@example.org/",
        "/dav/calendars/petr@example.org/",
        "/dav/addressbooks/petr@example.org/contacts/",
        "/dav/calendars/petr@example.org/work/ev.ics",
    ] {
        let r = env.raw(propfind(path, Some(auth.as_str()))).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{path}");
        let r = env.send("GET", path, &[], "").await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "GET {path}");
    }
    let own = env
        .raw(propfind(
            "/dav/principals/IVAN@example.org/",
            Some(auth.as_str()),
        ))
        .await;
    assert_eq!(
        own.status,
        StatusCode::MULTI_STATUS,
        "логин без учёта регистра"
    );
}

#[tokio::test]
async fn root_points_to_principal() {
    let env = Env::new();
    for path in ["/dav", "/dav/"] {
        let r = env.propfind(path, "0", DISCOVERY).await;
        assert_eq!(r.status, StatusCode::MULTI_STATUS, "{path}");
        assert!(r.body.contains(
            "<d:href>/dav/</d:href><d:propstat><d:prop><d:current-user-principal><d:href>/dav/principals/ivan@example.org/</d:href></d:current-user-principal></d:prop>"
        ), "{}", r.body);
        assert!(
            r.body.contains("<d:displayname/>"),
            "у корня нет displayname"
        );
    }
    let r = env.propfind("/dav/principals/", "1", DISCOVERY).await;
    assert!(
        r.body
            .contains("<d:href>/dav/principals/ivan@example.org/</d:href>"),
        "Depth 1 коллекции принципалов — свой принципал"
    );
    assert!(!r.body.contains("petr"));
}

#[tokio::test]
async fn options_without_auth() {
    let env = Env::new();
    let req = Request::builder()
        .method("OPTIONS")
        .uri("/dav/calendars/ivan@example.org/")
        .body(Body::empty())
        .expect("запрос");
    let r = env.raw(req).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("dav"), Some(DAV_HEADER));
    assert_eq!(
        DAV_HEADER,
        "1, 3, access-control, calendar-access, calendar-auto-schedule, addressbook, extended-mkcol"
    );
    assert_eq!(r.header("allow"), Some(ALLOW));
    assert!(ALLOW.contains("MKCALENDAR") && ALLOW.contains("REPORT"));
}

#[tokio::test]
async fn unavailable_unknown_and_methods() {
    let env = Env::new();
    env.auth.set_failure(Some(Error::UNAVAILABLE));
    let r = env.propfind("/dav/", "0", "").await;
    assert_eq!(r.status, StatusCode::SERVICE_UNAVAILABLE);
    env.auth.set_failure(None);
    for path in [
        "/dav/other/",
        "/dav/calendars/",
        "/dav/calendars/ivan@example.org/a/b/c",
    ] {
        let r = env.propfind(path, "0", "").await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{path}");
    }
    let r = env.send("LOCK", "/dav/", &[], "").await;
    assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(r.header("allow"), Some(ALLOW));
}

/// DAV-26: вход Basic и Bearer — в проверку учётных данных пришёл адрес соединения (`ConnectInfo`, как его ставит
/// сервер `tp_platform::service_http`), и при неверном пароле тоже; без соединения — адрес неизвестен.
#[tokio::test]
async fn verify_gets_connection_address() {
    let env = Env::new();
    let peer = std::net::SocketAddr::from(([192, 0, 2, 7], 50_000));
    for (a, code) in [
        (basic(LOGIN, "secret"), StatusCode::MULTI_STATUS),
        (basic(LOGIN, "wrong"), StatusCode::UNAUTHORIZED),
        ("Bearer token-1".to_owned(), StatusCode::MULTI_STATUS),
    ] {
        let mut req = propfind("/dav/", Some(a.as_str()));
        req.extensions_mut()
            .insert(axum::extract::ConnectInfo(peer));
        assert_eq!(env.raw(req).await.status, code, "{a}");
    }
    let own = basic(LOGIN, "secret");
    env.raw(propfind("/dav/", Some(own.as_str()))).await;
    let ip = Some(peer.ip());
    assert_eq!(env.auth.peers(), [ip, ip, ip, None]);
}
