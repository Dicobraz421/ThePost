//! Тесты автонастройки: снимки документов Thunderbird и `.mobileconfig`, перенаправления well-known, HTTP.

use axum::http::{HeaderMap, HeaderValue, header};

use super::*;
use crate::JmapConfig;
use crate::core::config::AutoconfigConfig;
use crate::testenv::{Env, http10, serve};

fn servers() -> Servers {
    Servers {
        host: "mail.example.org".into(),
        caldav: "https://mail.example.org/dav/".into(),
        carddav: "https://dav.example.org:8443/contacts/".into(),
        imap: MailEndpoint {
            port: 993,
            security: Security::Tls,
        },
        submission: MailEndpoint {
            port: 587,
            security: Security::Starttls,
        },
        organization: Some("ООО «Рога & Копыта»".into()),
    }
}

fn host(h: &str) -> HeaderMap {
    let mut m = HeaderMap::new();
    m.insert(header::HOST, HeaderValue::from_str(h).expect("host"));
    m
}

/// Снимок автонастройки Thunderbird; спецсимволы экранированы.
#[test]
fn thunderbird_snapshot() {
    let xml = thunderbird_xml("example.org", &servers());
    assert!(xml.contains("Рога &amp; Копыта"));
    insta::assert_snapshot!(xml);
}

/// Снимок профиля `.mobileconfig`; UUID стабильны и различны.
#[test]
fn mobileconfig_snapshot() {
    let plist = mobileconfig_plist("user@example.org", "Иван <Петров>", "example.org", &servers());
    assert!(plist.contains("Иван &lt;Петров&gt;"));
    let again = mobileconfig_plist("User@Example.org", "x", "example.org", &servers());
    let uuids = |p: &str| -> Vec<String> {
        p.lines()
            .zip(p.lines().skip(1))
            .filter(|(k, _)| k.contains("<key>PayloadUUID</key>"))
            .map(|(_, v)| v.trim().to_owned())
            .collect()
    };
    let ids = uuids(&plist);
    assert_eq!(ids.len(), 4);
    assert_eq!(ids, uuids(&again), "UUID не зависят от регистра входа");
    let mut uniq = ids.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), 4);
    insta::assert_snapshot!(plist);
}

/// UUID: формат 8-4-4-4-12, версия 8, вариант RFC 9562.
#[test]
fn uuid_format() {
    let u = uuid("x");
    let parts: Vec<&str> = u.split('-').collect();
    assert_eq!(
        parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
        [8, 4, 4, 4, 12]
    );
    assert!(parts[2].starts_with('8'));
    assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'A' | b'B'));
    assert_eq!(u, uuid("x"));
    assert_ne!(u, uuid("y"));
}

/// Серверы: явный `host`, узел `public_url`, заголовок `Host` без `autoconfig.`; пути DAV и полные URL.
#[tokio::test]
async fn servers_resolution() {
    let env = Env::new().await;
    let s = env.jmap.servers(&host("autoconfig.example.org:8443"));
    assert_eq!(s.host, "example.org");
    assert_eq!(s.caldav, "https://example.org/dav/");

    let cfg = JmapConfig {
        public_url: Some("https://web.example.org:8443/".into()),
        autoconfig: AutoconfigConfig {
            caldav: "cal/".into(),
            carddav: "https://dav.example.net/card/".into(),
            ..AutoconfigConfig::default()
        },
        ..JmapConfig::default()
    };
    let env = Env::with(cfg, Vec::new()).await;
    let s = env.jmap.servers(&host("autoconfig.example.org"));
    assert_eq!(s.host, "web.example.org");
    assert_eq!(s.caldav, "https://web.example.org:8443/cal/");
    assert_eq!(s.carddav, "https://dav.example.net/card/");

    let cfg = JmapConfig {
        public_url: Some("https://web.example.org".into()),
        autoconfig: AutoconfigConfig {
            host: Some("imap.example.org".into()),
            ..AutoconfigConfig::default()
        },
        ..JmapConfig::default()
    };
    let env = Env::with(cfg, Vec::new()).await;
    let s = env.jmap.servers(&HeaderMap::new());
    assert_eq!(s.host, "imap.example.org");
    assert_eq!(s.caldav, "https://web.example.org/dav/");
}

/// Разбор адресов, портов и доменов.
#[test]
fn parsing() {
    assert_eq!(
        url_parts("http://[::1]:8080/x"),
        Some(UrlParts {
            host: "[::1]".into(),
            port: 8080,
            tls: false
        })
    );
    assert_eq!(url_parts("https://A.example").map(|p| p.port), Some(443));
    assert_eq!(url_parts("ftp://x"), None);
    assert_eq!(split_port("[::1]"), ("[::1]", None));
    assert_eq!(split_port("host:x"), ("host:x", None));
    assert_eq!(valid_domain("Example.ORG."), Some("example.org".into()));
    assert_eq!(valid_domain("a..b"), None);
    assert_eq!(valid_domain("a<b>.org"), None);
    assert_eq!(valid_domain(""), None);
}

/// HTTP: перенаправления well-known (GET и PROPFIND), Thunderbird по адресу и по имени `autoconfig.`,
/// профиль — только с аутентификацией.
#[tokio::test]
async fn http_endpoints() {
    let cfg = JmapConfig {
        public_url: Some("https://mail.example.org".into()),
        ..JmapConfig::default()
    };
    let env = Env::with(cfg, Vec::new()).await;
    let addr = serve(env.jmap.clone()).await;
    let req = |method: &str, path: &str, host: &str, extra: &str| {
        format!("{method} {path} HTTP/1.0\r\nHost: {host}\r\n{extra}\r\n").into_bytes()
    };

    for (method, path, location) in [
        ("GET", "/.well-known/caldav", "https://mail.example.org/dav/"),
        ("PROPFIND", "/.well-known/caldav", "https://mail.example.org/dav/"),
        ("GET", "/.well-known/carddav", "https://mail.example.org/dav/"),
    ] {
        let (code, head, _) = http10(addr, &req(method, path, "mail.example.org", "")).await;
        assert_eq!(code, 301, "{method} {path}");
        assert!(
            head.to_lowercase()
                .contains(&format!("location: {location}")),
            "{head}"
        );
    }

    let (code, head, body) = http10(
        addr,
        &req(
            "GET",
            "/mail/config-v1.1.xml?emailaddress=user%40Example.org",
            "autoconfig.example.org",
            "",
        ),
    )
    .await;
    assert_eq!(code, 200);
    assert!(head.contains("application/xml"));
    let body = String::from_utf8(body).expect("utf-8");
    assert!(body.contains("<emailProvider id=\"example.org\">"));
    assert!(body.contains("<hostname>mail.example.org</hostname>"));

    let (code, _, body) = http10(
        addr,
        &req(
            "GET",
            "/.well-known/autoconfig/mail/config-v1.1.xml",
            "autoconfig.corp.example",
            "",
        ),
    )
    .await;
    assert_eq!(code, 200);
    assert!(String::from_utf8_lossy(&body).contains("<domain>corp.example</domain>"));

    let (code, _, _) = http10(
        addr,
        &req(
            "GET",
            "/mail/config-v1.1.xml?emailaddress=nobody",
            "autoconfig.example.org",
            "",
        ),
    )
    .await;
    assert_eq!(code, 400);

    let (code, head, _) = http10(
        addr,
        &req("GET", "/mail/profile.mobileconfig", "mail.example.org", ""),
    )
    .await;
    assert_eq!(code, 401);
    assert!(head.to_lowercase().contains("www-authenticate: basic"));
    let (code, head, body) = http10(
        addr,
        &req(
            "GET",
            "/mail/profile.mobileconfig",
            "mail.example.org",
            "Authorization: Bearer token-1\r\n",
        ),
    )
    .await;
    assert_eq!(code, 200);
    let lower = head.to_lowercase();
    assert!(lower.contains("content-type: application/x-apple-aspen-config"));
    assert!(lower.contains("filename=\"example.org.mobileconfig\""));
    let body = String::from_utf8(body).expect("utf-8");
    assert!(body.contains("<string>user@example.org</string>"));
    assert!(body.contains("<string>com.apple.carddav.account</string>"));
}
