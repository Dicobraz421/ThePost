//! FIX-08 (issue #469): вход фаззера цели `milter` и соседние места разбора ответов фильтра.
//!
//! Вход `crash-2fabe10d…` (84 байта, в корпусе цели `fuzz/corpus/milter`): согласование со всеми шагами без
//! команд, затем `replycode` `"555555555555\r"` — одиночный `\r` уходил в текст ответа SMTP (проверка цели
//! паниковала).

use bytes::{BufMut, Bytes, BytesMut};

use super::{Packet, R_ADDHEADER, R_CHGHEADER, R_INSHEADER, R_REPLYCODE, parse_replycode};
use crate::milter::{HeaderMod, apply, parse_header_mod};

fn replycode(text: &[u8]) -> Option<(u16, [u8; 3], String)> {
    parse_replycode(&Packet::new(R_REPLYCODE, Bytes::copy_from_slice(text)))
}

#[test]
fn crash_469_does_not_panic() {
    crate::fuzz::milter(include_bytes!(
        "../../fuzz/corpus/milter/crash-2fabe10d992a6b9ae3f5dad635076e5c9f085f78"
    ));
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
fn replycode_enhanced_code_matches_class() {
    // Класс расширенного кода не совпадает с кодом ответа или не цифры — код по умолчанию, токен — в тексте.
    assert_eq!(
        replycode(b"550 4.7.1 spam\0"),
        Some((550, [5, 7, 1], "4.7.1 spam".into()))
    );
    assert_eq!(
        replycode(b"451 +4.7.1 later\0"),
        Some((451, [4, 7, 1], "+4.7.1 later".into()))
    );
    assert_eq!(
        replycode(b"451 5.7.1\0"),
        Some((451, [4, 7, 1], "5.7.1".into()))
    );
    assert_eq!(
        replycode(b"554 5.7.0 no\0"),
        Some((554, [5, 7, 0], "no".into()))
    );
}

fn packet(code: u8, index: Option<u32>, data: &[u8]) -> Packet {
    let mut b = BytesMut::new();
    if let Some(i) = index {
        b.put_u32(i);
    }
    b.put_slice(data);
    Packet::new(code, b.freeze())
}

#[test]
fn header_mod_bad_length_or_field_is_protocol_error() {
    assert_eq!(
        parse_header_mod(&packet(R_ADDHEADER, None, b"X-Spam\0yes\0")).ok(),
        Some(HeaderMod::Add("X-Spam".into(), "yes".into()))
    );
    assert_eq!(
        parse_header_mod(&packet(R_CHGHEADER, Some(2), b"Subject\0\0")).ok(),
        Some(HeaderMod::Change(2, "Subject".into(), String::new()))
    );
    assert_eq!(
        parse_header_mod(&packet(R_INSHEADER, Some(0), b"X-Top\0a\n\tb\0")).ok(),
        Some(HeaderMod::Insert(0, "X-Top".into(), "a\n\tb".into()))
    );
    for (code, data) in [
        // Индекс короче четырёх байт.
        (R_INSHEADER, &b""[..]),
        (R_CHGHEADER, b"\0\0\x01"),
        // Не два поля.
        (R_ADDHEADER, b""),
        (R_ADDHEADER, b"X-Spam"),
        (R_ADDHEADER, b"X-Spam\0yes\0extra\0"),
        (R_CHGHEADER, b"\0\0\0\x01Subject"),
        // Имя — не печатные ASCII без `:`.
        (R_ADDHEADER, b"X-Bad\r\nEvil\x001\0"),
        (R_ADDHEADER, b"X Bad\x001\0"),
        (R_ADDHEADER, b"\x001\0"),
        (R_INSHEADER, b"\0\0\0\0X:Bad\x001\0"),
        (R_CHGHEADER, b"\0\0\0\x01Subject\n\0\0"),
        (R_ADDHEADER, b"X-\xd0\xb0\x001\0"),
    ] {
        assert!(
            parse_header_mod(&Packet::new(code, Bytes::copy_from_slice(data))).is_err(),
            "{:?} {data:?}",
            char::from(code)
        );
    }
}

#[test]
fn header_values_cannot_inject_fields() {
    let raw = b"From: a@example.net\r\nSubject: x\r\n\r\nbody\r\n";
    let out = apply(
        raw,
        &[HeaderMod::Add(
            "X-Fold".into(),
            "a\nEvil: b\n\tc\r\rd".into(),
        )],
    );
    assert_eq!(
        &out[..],
        b"From: a@example.net\r\nSubject: x\r\nX-Fold: a\r\n Evil: b\r\n\tcd\r\n\r\nbody\r\n"
    );
}
