//! FIX-08 (issue #469): вход фаззера цели `milter` и соседние места разбора ответов фильтра.
//!
//! Вход `crash-2fabe10d…` (84 байта): согласование со всеми шагами без команд, затем `replycode`
//! `"555555555555\r"` — одиночный `\r` уходил в текст ответа SMTP (проверка цели паниковала).

use bytes::Bytes;

use super::{Packet, R_REPLYCODE, parse_replycode};
use crate::milter::{HeaderMod, apply};

/// Вход `crash-2fabe10d992a6b9ae3f5dad635076e5c9f085f78` из issue #469.
const CRASH_469: [u8; 84] = [
    0x00, 0x00, 0x00, 0x18, 0x4f, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0xb7,
    0xff, 0xff, 0xff, 0xff, 0xdb, 0xff, 0xff, 0x00, 0xff, 0xff, 0x25, 0x18, 0x00, 0x00, 0x00, 0x2a,
    0x79, 0x35, 0x35, 0x35, 0x35, 0x35, 0x35, 0x35, 0x35, 0x35, 0x35, 0x35, 0x35, 0x0d, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x34, 0xb0, 0xc9, 0xc8, 0x70, 0xe6, 0x49, 0x24, 0x36, 0x78, 0x00,
    0x00, 0x37, 0x37, 0x37, 0x37, 0xa5, 0x00, 0x0a, 0x00, 0x00, 0x10, 0x00, 0xff, 0xff, 0xff, 0xff,
    0xff, 0xff, 0x01, 0x13,
];

fn replycode(text: &[u8]) -> Option<(u16, [u8; 3], String)> {
    parse_replycode(&Packet::new(R_REPLYCODE, Bytes::copy_from_slice(text)))
}

#[test]
fn crash_469_does_not_panic() {
    crate::fuzz::milter(&CRASH_469);
}

#[test]
fn replycode_text_has_no_control_characters() {
    let (code, esc, msg) = replycode(b"555555555555\r\0garbage").expect("reply");
    assert_eq!((code, esc), (555, [5, 7, 1]));
    assert_eq!(msg, "555555555");
    // Многострочный ответ — первая строка, без `\r`.
    let (code, esc, msg) = replycode(b"550-5.7.1 first\r\n550 5.7.1 second\0").expect("reply");
    assert_eq!((code, esc, msg.as_str()), (550, [5, 7, 1], "first"));
    let (_, _, msg) = replycode(b"451 4.7.1 a\rb\x1bc\td\0").expect("reply");
    assert_eq!(msg, "a");
    let (_, _, msg) = replycode(b"451 4.7.1 a\x1bc\td\x7f\0").expect("reply");
    assert!(!msg.contains(|c: char| c.is_control()), "{msg:?}");
    // Неверный код — ошибка протокола у вызывающего, не паника.
    for bad in [
        &b""[..],
        b"\r550 x",
        b"55",
        b"5\xd0\xb0",
        b"+55 x",
        b"999 x",
        b"\xff\xfe\xfd",
    ] {
        assert_eq!(replycode(bad), None, "{bad:?}");
    }
}

#[test]
fn header_mods_cannot_inject_fields() {
    let raw = b"From: a@example.net\r\nSubject: x\r\n\r\nbody\r\n";
    let out = apply(
        raw,
        &[
            HeaderMod::Add("X-Bad\r\nEvil".into(), "1".into()),
            HeaderMod::Add("X Bad".into(), "1".into()),
            HeaderMod::Add(String::new(), "1".into()),
            HeaderMod::Insert(0, "X:Bad".into(), "1".into()),
            HeaderMod::Change(1, "Subject\n".into(), String::new()),
            HeaderMod::Add("X-Fold".into(), "a\nEvil: b\n\tc\r\rd".into()),
        ],
    );
    assert_eq!(
        &out[..],
        b"From: a@example.net\r\nSubject: x\r\nX-Fold: a\r\n Evil: b\r\n\tcd\r\n\r\nbody\r\n"
    );
}
