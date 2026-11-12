//! Профили Milter (`smtp.milter.profiles`, SMTP-40): профиль политики вызывает свой набор фильтров; без профиля и
//! с неизвестным профилем — все фильтры направления письма.

use bytes::Bytes;

use super::proto;
use super::tests::{MAIL, Mock, envelope, on_eob, reply_of};
use super::*;
use crate::server::Mode;

/// Два фильтра (`rspamd`, `dlp`) и профиль `antispam` из одного `rspamd`.
async fn profiled() -> (Milters, Mock, Mock) {
    let rspamd = Mock::start(0, on_eob(vec![reply_of(proto::R_ACCEPT)])).await;
    let dlp = Mock::start(0, on_eob(vec![reply_of(proto::R_ACCEPT)])).await;
    let mut profiles = std::collections::BTreeMap::new();
    profiles.insert("antispam".to_owned(), vec!["rspamd".to_owned()]);
    let m = Milters::new(
        MilterConfig {
            filters: vec![rspamd.filter("rspamd"), dlp.filter("dlp")],
            profiles,
        },
        "mx.example.org",
    );
    (m, rspamd, dlp)
}

#[tokio::test]
async fn profile_calls_its_filters() {
    let (m, rspamd, dlp) = profiled().await;
    let out = m
        .check(
            &Mail::of(&envelope(Mode::Inbound)),
            Bytes::from_static(MAIL),
            Some("antispam"),
        )
        .await;
    assert_eq!(out.verdict, Verdict::Accept);
    assert!(!rspamd.codes().await.is_empty());
    assert_eq!(dlp.codes().await, "");
}

#[tokio::test]
async fn no_or_unknown_profile_calls_all_filters() {
    for profile in [None, Some("unknown")] {
        let (m, rspamd, dlp) = profiled().await;
        let out = m
            .check(
                &Mail::of(&envelope(Mode::Inbound)),
                Bytes::from_static(MAIL),
                profile,
            )
            .await;
        assert_eq!(out.verdict, Verdict::Accept);
        assert!(!rspamd.codes().await.is_empty(), "{profile:?}");
        assert!(!dlp.codes().await.is_empty(), "{profile:?}");
    }
}

#[tokio::test]
async fn profile_keeps_filter_direction() {
    let rspamd = Mock::start(0, on_eob(vec![reply_of(proto::R_ACCEPT)])).await;
    let dlp = Mock::start(0, on_eob(vec![reply_of(proto::R_ACCEPT)])).await;
    let mut profiles = std::collections::BTreeMap::new();
    profiles.insert(
        "all".to_owned(),
        vec!["rspamd".to_owned(), "dlp".to_owned()],
    );
    let m = Milters::new(
        MilterConfig {
            filters: vec![
                rspamd.filter("rspamd"),
                MilterFilter {
                    direction: MilterDirection::Outbound,
                    ..dlp.filter("dlp")
                },
            ],
            profiles,
        },
        "mx.example.org",
    );
    // Профиль сужает набор, но не меняет направление: приём не проходит фильтр исходящей почты.
    let out = m
        .check(
            &Mail::of(&envelope(Mode::Inbound)),
            Bytes::from_static(MAIL),
            Some("all"),
        )
        .await;
    assert_eq!(out.verdict, Verdict::Accept);
    assert!(!rspamd.codes().await.is_empty());
    assert_eq!(dlp.codes().await, "");
}
