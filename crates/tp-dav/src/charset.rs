//! Перекод vCard с сырыми 8-битными байтами в UTF-8 перед разбором calcard (DAV-33).
//!
//! Выгрузки старых телефонов и Outlook (vCard 2.1, иногда 3.0) пишут значения байтами кодировки из параметра
//! `CHARSET` без `ENCODING`. calcard принимает только `&str` и учитывает `CHARSET` лишь вместе с `ENCODING`, поэтому
//! такое тело сначала перекодируется здесь. Тело в UTF-8 — как есть. Иначе строка за строкой: строка из ASCII
//! (в том числе в `QUOTED-PRINTABLE` — её раскодирует calcard по тому же `CHARSET`) — как есть; остальные — по
//! `CHARSET` своего свойства (строки продолжения — по свойству, которое продолжают), а у свойства без `CHARSET` — по
//! первому `CHARSET` тела. Декодеры — mail-parser (те же, что calcard применяет к `CHARSET` с `ENCODING`).
//!
//! Одна функция на CardDAV и JMAP: `tp-jmap` зависит от `tp-dav` и берёт её отсюда.

use std::borrow::Cow;

use mail_parser::decoders::charsets::map::charset_decoder;

/// Текст vCard в UTF-8: тело как есть или перекод по `CHARSET` (см. описание модуля). Тело не в UTF-8 без
/// `CHARSET` или с неизвестной кодировкой — `None`.
pub fn vcard_utf8(raw: &[u8]) -> Option<Cow<'_, str>> {
    if let Ok(text) = std::str::from_utf8(raw) {
        return Some(Cow::Borrowed(text));
    }
    let first = raw
        .split(|b| *b == b'\n')
        .filter(|l| !is_continuation(l))
        .find_map(charset_of);
    let mut out = String::with_capacity(raw.len() * 2);
    let mut current = None;
    for line in raw.split_inclusive(|b| *b == b'\n') {
        if !is_continuation(line) {
            current = charset_of(line);
        }
        if line.is_ascii() {
            out.push_str(&String::from_utf8_lossy(line));
        } else {
            let decode = charset_decoder(current.or(first)?)?;
            out.push_str(&decode(line));
        }
    }
    Some(Cow::Owned(out))
}

/// Строка продолжения свёрнутого свойства (RFC 6350 §3.2).
fn is_continuation(line: &[u8]) -> bool {
    matches!(line.first(), Some(b' ' | b'\t'))
}

/// Значение параметра `CHARSET` свойства из его первой строки (имя и параметры — до первого `:`).
fn charset_of(line: &[u8]) -> Option<&[u8]> {
    let head = &line[..line.iter().position(|b| *b == b':')?];
    head.split(|b| *b == b';').find_map(|param| {
        let (name, value) = param.split_at(param.iter().position(|b| *b == b'=')?);
        name.eq_ignore_ascii_case(b"CHARSET")
            .then(|| value[1..].trim_ascii())
            .map(|v| v.strip_prefix(b"\"").unwrap_or(v))
            .map(|v| v.strip_suffix(b"\"").unwrap_or(v))
            .filter(|v| !v.is_empty())
    })
}
