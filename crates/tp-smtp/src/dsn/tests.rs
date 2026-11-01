//! Снимки уведомлений (insta) и разбор их `mail-parser`: структура `multipart/report`, поля DSN.

use mail_parser::{MessageParser, MimeHeaders, PartType};

use super::*;

const ORIGINAL: &[u8] =
    b"From: alice@example.org\r\nTo: x@example.net\r\nSubject: hello\r\nMessage-ID: <m1@example.org>\r\n\r\nbody\r\n";

fn report<'a>(
    rcpts: &'a [Recipient],
    ret: DsnRet,
    retry_until: Option<OffsetDateTime>,
) -> Report<'a> {
    Report {
        hostname: "mx.example.org",
        to: "alice@example.org",
        envid: Some("QQ314159"),
        arrival: at(1_767_261_600),
        now: at(1_767_276_000),
        retry_until,
        rcpts,
        original: ORIGINAL,
        ret,
        id: "0001",
    }
}

fn at(ts: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(ts).expect("ts")
}

/// Части отчёта: тип каждой и текст `message/delivery-status`.
fn parts(raw: &[u8]) -> (Vec<String>, String) {
    let msg = MessageParser::default().parse(raw).expect("parse");
    let root = msg.content_type().expect("content-type");
    assert_eq!(root.ctype(), "multipart");
    assert_eq!(root.subtype(), Some("report"));
    assert_eq!(root.attribute("report-type"), Some("delivery-status"));
    let mut types = Vec::new();
    let mut status = String::new();
    for part in msg.parts.iter().skip(1) {
        let ct = part.content_type().expect("part content-type");
        let t = format!("{}/{}", ct.ctype(), ct.subtype().unwrap_or_default());
        if t == "message/delivery-status" {
            status = match &part.body {
                PartType::Text(t) => t.to_string(),
                PartType::Binary(b) | PartType::InlineBinary(b) => {
                    String::from_utf8_lossy(b).into_owned()
                }
                _ => String::new(),
            };
        }
        types.push(t);
    }
    (types, status)
}

#[test]
fn failure_report() {
    let rcpts = [
        Recipient::new(
            "x@example.net",
            Action::Failed,
            "550 5.1.1 <x@example.net>: no such user",
        ),
        Recipient {
            orcpt: Some("rfc822;y@example.net".into()),
            ..Recipient::new("y@example.net", Action::Failed, "connection refused")
        },
    ];
    let raw = build(&report(&rcpts, DsnRet::Headers, None));
    let text = String::from_utf8(raw.clone()).expect("utf8");
    insta::assert_snapshot!("failure", text);
    let msg = MessageParser::default().parse(&raw).expect("parse");
    assert_eq!(msg.subject(), Some("Undelivered Mail Returned to Sender"));
    assert_eq!(
        msg.header_raw("Auto-Submitted").map(str::trim),
        Some("auto-replied")
    );
    let (types, status) = parts(&raw);
    assert_eq!(
        types,
        [
            "text/plain",
            "message/delivery-status",
            "text/rfc822-headers"
        ]
    );
    assert!(status.contains("Reporting-MTA: dns; mx.example.org\r\n"));
    assert!(status.contains("Original-Envelope-Id: QQ314159\r\n"));
    assert!(status.contains("Final-Recipient: rfc822; x@example.net\r\nAction: failed\r\nStatus: 5.1.1\r\nDiagnostic-Code: smtp; 550 5.1.1"));
    assert!(status.contains("Original-Recipient: rfc822;y@example.net\r\nFinal-Recipient: rfc822; y@example.net\r\nAction: failed\r\nStatus: 5.0.0\r\nLast-Attempt-Date"));
    // Тело исходного письма не возвращается.
    assert!(!text.contains("\r\nbody\r\n"));
}

#[test]
fn delay_report() {
    let rcpts = [Recipient::new(
        "x@example.net",
        Action::Delayed,
        "451 4.3.0 mailbox busy",
    )];
    let raw = build(&report(&rcpts, DsnRet::Headers, Some(at(1_767_693_600))));
    insta::assert_snapshot!("delay", String::from_utf8(raw.clone()).expect("utf8"));
    let (_, status) = parts(&raw);
    assert!(status.contains("Action: delayed\r\nStatus: 4.3.0\r\n"));
    assert!(status.contains("Will-Retry-Until: "));
}

#[test]
fn success_report_with_full_message() {
    let rcpts = [Recipient::new(
        "x@example.net",
        Action::Delivered,
        "250 2.0.0 OK",
    )];
    let raw = build(&report(&rcpts, DsnRet::Full, None));
    insta::assert_snapshot!("success", String::from_utf8(raw.clone()).expect("utf8"));
    let (types, status) = parts(&raw);
    assert_eq!(types[1..], ["message/delivery-status", "message/rfc822"]);
    assert!(status.contains("Action: delivered\r\nStatus: 2.0.0\r\n"));
    assert!(!status.contains("Last-Attempt-Date"));
    let msg = MessageParser::default().parse(&raw).expect("parse");
    let inner = msg
        .parts
        .iter()
        .find_map(|p| match &p.body {
            PartType::Message(m) => Some(m),
            _ => None,
        })
        .expect("message/rfc822");
    assert_eq!(inner.subject(), Some("hello"));
}

#[test]
fn status_from_reply() {
    assert_eq!(status_of("550 5.7.1 denied", Action::Failed), "5.7.1");
    assert_eq!(status_of("452 mailbox full", Action::Delayed), "4.0.0");
    assert_eq!(status_of("timeout", Action::Failed), "5.0.0");
    assert_eq!(status_of("timeout", Action::Delayed), "4.0.0");
}
