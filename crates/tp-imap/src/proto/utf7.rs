//! Модифицированный UTF-7 имён папок IMAP4rev1 (RFC 3501, 5.1.3; RFC 9051, приложение A).
//!
//! Печатные ASCII (`0x20..=0x7e`) — как есть, `&` — `&-`; остальное — UTF-16BE в base64 с алфавитом `+,`
//! без выравнивания между `&` и `-`. Клиенты IMAP4rev2 передают имена в UTF-8 (сессия выбирает по `ENABLE`).

use base64::Engine;
use base64::alphabet::IMAP_MUTF7;
use base64::engine::general_purpose::{GeneralPurpose, NO_PAD};

const MUTF7: GeneralPurpose = GeneralPurpose::new(&IMAP_MUTF7, NO_PAD);

fn printable(c: char) -> bool {
    (' '..='~').contains(&c)
}

/// Имя в модифицированном UTF-7.
pub fn encode_utf7(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut wide: Vec<u16> = Vec::new();
    let flush = |wide: &mut Vec<u16>, out: &mut String| {
        if wide.is_empty() {
            return;
        }
        let bytes: Vec<u8> = wide.iter().flat_map(|u| u.to_be_bytes()).collect();
        out.push('&');
        out.push_str(&MUTF7.encode(bytes));
        out.push('-');
        wide.clear();
    };
    for c in name.chars() {
        if printable(c) {
            flush(&mut wide, &mut out);
            out.push(c);
            if c == '&' {
                out.push('-');
            }
        } else {
            let mut buf = [0u16; 2];
            wide.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    flush(&mut wide, &mut out);
    out
}

/// Имя из модифицированного UTF-7; `None` — не печатный ASCII, незакрытый или неверный base64-участок.
pub fn decode_utf7(name: &[u8]) -> Option<String> {
    let mut out = String::with_capacity(name.len());
    let mut rest = name;
    while let Some((&b, tail)) = rest.split_first() {
        if !(0x20..=0x7e).contains(&b) {
            return None;
        }
        if b != b'&' {
            out.push(char::from(b));
            rest = tail;
            continue;
        }
        let end = tail.iter().position(|&c| c == b'-')?;
        let (chunk, after) = (&tail[..end], &tail[end + 1..]);
        if chunk.is_empty() {
            out.push('&');
        } else {
            let bytes = MUTF7.decode(chunk).ok()?;
            let (pairs, odd) = bytes.as_chunks::<2>();
            if !odd.is_empty() {
                return None;
            }
            let wide: Vec<u16> = pairs.iter().map(|p| u16::from_be_bytes(*p)).collect();
            let text = String::from_utf16(&wide).ok()?;
            // Печатный ASCII внутри base64 — не каноническая форма (RFC 3501, 5.1.3).
            if text.chars().any(printable) {
                return None;
            }
            out.push_str(&text);
        }
        rest = after;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_examples() {
        // RFC 3501, 5.1.3: «~peter/mail/台北/日本語».
        let name = "~peter/mail/\u{53F0}\u{5317}/\u{65E5}\u{672C}\u{8A9E}";
        assert_eq!(encode_utf7(name), "~peter/mail/&U,BTFw-/&ZeVnLIqe-");
        assert_eq!(
            decode_utf7(b"~peter/mail/&U,BTFw-/&ZeVnLIqe-").as_deref(),
            Some(name)
        );
        assert_eq!(
            encode_utf7("Отправленные"),
            "&BB4EQgQ,BEAEMAQyBDsENQQ9BD0ESwQ1-"
        );
        assert_eq!(encode_utf7("A&B"), "A&-B");
        assert_eq!(decode_utf7(b"A&-B").as_deref(), Some("A&B"));
    }

    #[test]
    fn rejects_malformed() {
        assert_eq!(decode_utf7(b"&U,BTF"), None);
        assert_eq!(decode_utf7("Папка".as_bytes()), None);
        // «A» в base64 — не каноническая форма.
        assert_eq!(decode_utf7(b"&AEE-"), None);
    }

    #[test]
    fn roundtrip() {
        for s in ["", "INBOX", "Входящие/Архив 2026", "a&b&-c", "😀 смайл"] {
            assert_eq!(decode_utf7(encode_utf7(s).as_bytes()).as_deref(), Some(s));
        }
    }
}
