//! Приглашения iTIP в письмах (RFC 6047, SRS 4.4): письмо с частью `text/calendar; method=…` локальному получателю
//! после доставки передаётся приёму iTIP учётной записи (`ItipDelivery`, [`crate::deliver::Delivery::with_itip`]) —
//! извне (iMIP, SMTP-33) и из почтового клиента (submission, JMAP; решение 78, SMTP-36).
//!
//! Здесь — только то, что нужно до вызова: часть письма, метод и сверка `From` с участниками. От организатора
//! (`REQUEST`, `CANCEL`, …) `From` должен совпадать с `ORGANIZER`, от участника (`REPLY`, `COUNTER`, `REFRESH`) — с
//! одним из `ATTENDEE`. Доверие к самому `From` (DMARC у iMIP, принадлежность вошедшей учётной записи у письма из
//! клиента) проверяет вызывающий. iCalendar целиком не разбирается (это делает реализация `ItipDelivery` на
//! calcard): из объекта читаются только строки `METHOD`, `ORGANIZER` и `ATTENDEE`.

use mail_parser::{MessageParser, MimeHeaders};
use tp_contracts::schedule::ItipMethod;

/// Сообщение iTIP из письма, `From` которого совпал с организатором или участником.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Imip {
    /// Адрес `From` (строчными), совпавший с `ORGANIZER` или `ATTENDEE`.
    pub from: String,
    pub method: ItipMethod,
    /// Объект iCalendar как получен.
    pub ics: String,
}

/// Сообщение iTIP письма `raw`. `None` — письмо без iTIP (нет части `text/calendar` с параметром `method`);
/// `Err` — причина, по которой сообщение не применяется (для журнала, без содержимого).
pub(crate) fn extract(raw: &[u8]) -> Option<Result<Imip, &'static str>> {
    let msg = MessageParser::new().parse(raw)?;
    let (param, ics) = msg.parts.iter().find_map(|p| {
        let ct = p.content_type()?;
        let calendar = ct.ctype().eq_ignore_ascii_case("text")
            && ct
                .subtype()
                .is_some_and(|s| s.eq_ignore_ascii_case("calendar"));
        let method = ct.attribute("method").filter(|_| calendar)?;
        Some((
            method.trim().to_owned(),
            String::from_utf8_lossy(p.contents()).into_owned(),
        ))
    })?;
    Some((|| {
        let method: ItipMethod = param.parse().map_err(|_| "неизвестный метод iTIP")?;
        // Ровно один адрес `From`: DMARC и права отправки проверяют один адрес, а не выбор из нескольких.
        let from = msg
            .from()
            .map(|a| a.iter().filter_map(|x| x.address.as_deref()).collect())
            .unwrap_or_else(Vec::new);
        let [from] = from.as_slice() else {
            return Err("у письма не один адрес From");
        };
        let lines = unfold(&ics);
        let mut methods = lines.iter().filter_map(|l| property(l, "METHOD"));
        match (methods.next(), methods.next()) {
            (Some(m), None) if m.trim().eq_ignore_ascii_case(method.as_str()) => {}
            _ => return Err("METHOD объекта не совпадает с параметром method части письма"),
        }
        let role = if method.from_organizer() {
            "ORGANIZER"
        } else {
            "ATTENDEE"
        };
        let known = lines
            .iter()
            .filter_map(|l| property(l, role))
            .any(|v| address(v).eq_ignore_ascii_case(from));
        if !known {
            return Err(if method.from_organizer() {
                "отправитель письма — не организатор"
            } else {
                "отправитель письма — не участник"
            });
        }
        Ok(Imip {
            from: from.to_lowercase(),
            method,
            ics: ics.clone(),
        })
    })())
}

/// Строки содержимого без переносов (RFC 5545 §3.1: перенос — конец строки и пробел или табуляция).
fn unfold(ics: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in ics.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        match (line.strip_prefix([' ', '\t']), out.last_mut()) {
            (Some(rest), Some(last)) => last.push_str(rest),
            _ => out.push(line.to_owned()),
        }
    }
    out
}

/// Значение свойства `name` строки `line` (`NAME;параметры:значение`); двоеточие в кавычках параметра — не
/// разделитель.
fn property<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let head = line.get(..name.len())?;
    let rest = &line[name.len()..];
    if !head.eq_ignore_ascii_case(name) || !rest.starts_with([':', ';']) {
        return None;
    }
    let mut quoted = false;
    let colon = rest.char_indices().find_map(|(i, c)| {
        quoted ^= c == '"';
        (c == ':' && !quoted).then_some(i)
    })?;
    Some(&rest[colon + 1..])
}

/// Адрес календарного пользователя без `mailto:`.
fn address(value: &str) -> &str {
    let v = value.trim();
    match v.get(..7) {
        Some(p) if p.eq_ignore_ascii_case("mailto:") => &v[7..],
        _ => v,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mail(method: &str, ics: &str) -> String {
        format!(
            "From: Organizer <Org@Example.com>\r\nTo: bob@example.org\r\nSubject: s\r\nMIME-Version: 1.0\r\n\
             Content-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nhi\r\n\
             --b\r\nContent-Type: text/calendar; method={method}; charset=utf-8\r\n\r\n{ics}\r\n--b--\r\n"
        )
    }

    const REQUEST: &str = "BEGIN:VCALENDAR\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:u1\r\n\
        ORGANIZER;CN=\"Org: boss\";SENT-BY=\"mailto:x@example.net\":MAILTO:org@exam\r\n ple.com\r\n\
        ATTENDEE:mailto:bob@example.org\r\nEND:VEVENT\r\nEND:VCALENDAR";

    #[test]
    fn request_from_organizer() {
        let m = extract(mail("REQUEST", REQUEST).as_bytes())
            .expect("itip")
            .expect("ok");
        assert_eq!(m.from, "org@example.com");
        assert_eq!(m.method, ItipMethod::Request);
        assert!(m.ics.starts_with("BEGIN:VCALENDAR") && m.ics.contains("UID:u1"));
    }

    #[test]
    fn reply_needs_attendee() {
        let reply = REQUEST.replace("METHOD:REQUEST", "METHOD:REPLY");
        // Отправитель — организатор, а не участник.
        assert_eq!(
            extract(mail("REPLY", &reply).as_bytes()),
            Some(Err("отправитель письма — не участник"))
        );
        let reply = reply.replace("bob@example.org", "ORG@example.com");
        let m = extract(mail("reply", &reply).as_bytes())
            .expect("itip")
            .expect("ok");
        assert_eq!(m.method, ItipMethod::Reply);
    }

    #[test]
    fn ignored() {
        let other = REQUEST.replace("org@exam\r\n ple.com", "other@example.com");
        for (method, ics) in [
            ("REQUEST", other.as_str()),
            ("CANCEL", REQUEST),
            ("BOGUS", REQUEST),
        ] {
            let got = extract(mail(method, ics).as_bytes()).expect("itip");
            assert!(got.is_err(), "{method}: {got:?}");
        }
        // Адрес в SENT-BY и в имени — не организатор.
        let sent_by = REQUEST
            .replace("org@exam\r\n ple.com", "boss@example.net")
            .replace("x@example.net", "org@example.com");
        assert!(matches!(
            extract(mail("REQUEST", &sent_by).as_bytes()),
            Some(Err(_))
        ));
    }

    #[test]
    fn not_itip() {
        assert_eq!(
            extract(
                b"From: a@example.com\r\nSubject: s\r\n\r\nMETHOD:REQUEST\r\n",
                true
            ),
            None
        );
        // `text/calendar` без параметра `method` — вложение-календарь, не iMIP (RFC 6047 §2.4).
        let plain = mail("REQUEST", REQUEST).replace(" method=REQUEST;", "");
        assert_eq!(extract(plain.as_bytes()), None);
    }
}
