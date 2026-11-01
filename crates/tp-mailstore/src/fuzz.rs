//! Проверки разбора MIME на произвольных данных (MS-13; SRS 6.8): цели cargo-fuzz `crates/tp-mailstore/fuzz`
//! и proptest крейта. Каждая функция паникует, только если нарушен инвариант разбора.
//!
//! - [`split`] — скелет и части: сборка обратно побайтова, с заголовками получателя и без, в пределах [`MimeLimits`];
//! - [`meta`] — ENVELOPE и BODYSTRUCTURE ([`crate::meta`]): дерево частей не глубже и не больше пределов;
//! - [`skeleton`] — разбор объекта-скелета из хранилища: битые данные — ошибка, не паника и не огромное выделение памяти.

use std::collections::HashMap;

use bytes::Bytes;
use tp_contracts::meta::BodyPart;

use crate::meta::{PREVIEW_CHARS, message_meta, message_meta_with};
use crate::split::{MimeLimits, Skeleton, Split};

/// Малые пределы: фаззер быстрее доходит до их краёв.
pub const SMALL: MimeLimits = MimeLimits {
    max_depth: 4,
    max_parts: 16,
};

/// Скелет и части: сборка равна исходному письму (с заголовками получателя перед ним).
pub fn split(data: &[u8]) {
    let prefix: &[u8] = b"Delivered-To: a@example.org\r\n";
    for limits in [MimeLimits::default(), SMALL] {
        let split = Split::with_limits(Bytes::copy_from_slice(data), limits);
        assert!(
            split.cuts().len() <= limits.max_parts,
            "частей больше предела"
        );
        let skel = match Skeleton::decode(split.skeleton(prefix)) {
            Ok(skel) => skel,
            Err(e) => panic!("свой скелет не разбирается: {e}"),
        };
        assert_eq!(skel.size(), (prefix.len() + data.len()) as u64);
        for s in skel.sections() {
            assert!(s.range.start <= s.range.end && s.range.end <= skel.size());
        }
        let parts: HashMap<&str, Bytes> = split
            .cuts()
            .iter()
            .map(|c| (c.path.as_str(), split.body(c)))
            .collect();
        let joined = match skel.join(|p| parts.get(p).cloned()) {
            Ok(joined) => joined,
            Err(e) => panic!("сборка: {e}"),
        };
        assert!(
            joined.starts_with(prefix) && joined[prefix.len()..] == *data,
            "сборка не совпала"
        );
    }
}

/// ENVELOPE и BODYSTRUCTURE: без паники, дерево частей в пределах.
pub fn meta(data: &[u8]) {
    let _ = message_meta(data);
    let m = message_meta_with(data, SMALL);
    let (levels, nodes) = shape(&m.body);
    assert!(levels <= SMALL.max_depth + 1, "глубина {levels}");
    assert!(nodes <= SMALL.max_parts, "частей {nodes}");
    assert!(m.preview.chars().count() <= PREVIEW_CHARS);
}

/// Объект-скелет из произвольных байтов: ошибка или согласованные сегменты.
pub fn skeleton(data: &[u8]) {
    let Ok(skel) = Skeleton::decode(Bytes::copy_from_slice(data)) else {
        return;
    };
    let mut at = 0;
    for seg in skel.segments() {
        let r = seg.range();
        assert_eq!(r.start, at, "сегменты с пропуском");
        at = r.end;
    }
    assert_eq!(at, skel.size());
    for s in skel.sections() {
        assert_eq!(skel.section(&s.path).map(|r| r.start <= r.end), Some(true));
    }
    // Частей нет: сборка — ошибка (если есть вырезанные части), без выделения памяти под `size`.
    let _ = skel.join(|_| None);
}

/// Уровней и узлов в дереве частей.
fn shape(part: &BodyPart) -> (usize, usize) {
    part.children
        .iter()
        .map(shape)
        .fold((1, 1), |(levels, nodes), (l, n)| {
            (levels.max(l + 1), nodes + n)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    /// Корпус KIT-04 — он же начальный корпус целей cargo-fuzz.
    #[test]
    fn corpus_passes_checks() {
        for mail in tp_testkit::corpus().expect("corpus") {
            split(&mail.raw);
            meta(&mail.raw);
            skeleton(&Split::new(Bytes::from(mail.raw.clone())).skeleton(b""));
        }
    }

    #[test]
    fn limits_cut_deep_and_wide_mail() {
        let deep = tp_testkit::corpus::corpus_mail("broken-deep-nesting")
            .expect("mail")
            .raw;
        let wide = tp_testkit::corpus::corpus_mail("many-parts")
            .expect("mail")
            .raw;
        let tight = MimeLimits {
            max_depth: 1,
            max_parts: 2,
        };
        for raw in [deep, wide] {
            let full = Split::new(Bytes::from(raw.clone()));
            let cut = Split::with_limits(Bytes::from(raw.clone()), tight);
            assert!(cut.cuts().len() <= 2 && cut.cuts().len() <= full.cuts().len());
            assert!(full.cuts().len() <= 2 || cut.cuts().len() < full.cuts().len());
            let (levels, nodes) = shape(&message_meta_with(&raw, tight).body);
            let (all_levels, all_nodes) = shape(&message_meta(&raw).body);
            assert!(levels <= 2 && nodes <= 2, "{levels} {nodes}");
            assert!(levels < all_levels || nodes < all_nodes);
        }
    }

    /// Письмо с деревом `multipart/mixed` случайной формы.
    fn tree() -> impl Strategy<Value = String> {
        let leaf =
            "[a-zA-Z0-9 ]{0,40}".prop_map(|t| format!("Content-Type: text/plain\r\n\r\n{t}"));
        leaf.prop_recursive(8, 64, 6, |inner| {
            (
                proptest::collection::vec(inner, 0..6),
                0u32..1000,
                any::<bool>(),
            )
                .prop_map(|(children, b, rfc822)| {
                    let mut out = format!(
                        "Content-Type: multipart/mixed; boundary=\"b{b}\"\r\n\r\npreamble\r\n"
                    );
                    for c in children {
                        out.push_str(&format!("--b{b}\r\n"));
                        if rfc822 {
                            out.push_str("Content-Type: message/rfc822\r\n\r\nSubject: n\r\n");
                        }
                        out.push_str(&c);
                        out.push_str("\r\n");
                    }
                    out.push_str(&format!("--b{b}--\r\n"));
                    out
                })
        })
    }

    proptest! {
        #[test]
        fn arbitrary_bytes(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
            split(&data);
            meta(&data);
            skeleton(&data);
        }

        #[test]
        fn nested_mime(body in tree()) {
            let raw = format!("Subject: t\r\nMIME-Version: 1.0\r\n{body}");
            split(raw.as_bytes());
            meta(raw.as_bytes());
        }

        #[test]
        fn mangled_skeleton(
            body in tree(),
            flips in proptest::collection::vec((any::<prop::sample::Index>(), any::<u8>()), 1..8),
        ) {
            let mut skel = Split::new(Bytes::from(body.into_bytes())).skeleton(b"").to_vec();
            for (i, b) in flips {
                let i = i.index(skel.len());
                skel[i] = b;
            }
            skeleton(&skel);
        }
    }
}
