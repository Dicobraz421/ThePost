//! Метаданные письма для протоколов (CONTRACTS.md#MailStore `MessageMeta`; SRS 4.2), вычисляются при приёме.
//!
//! Разбор — `mail-parser`; значения заголовков декодированы (encoded-word, RFC 2231).
//! - [`Envelope`] (IMAP ENVELOPE): поля — как в заголовках письма, без подстановок IMAP (`Sender` по `From`
//!   добавляет протокол); дата — со смещением из заголовка (JMAP `sentAt`); `Message-ID` и `In-Reply-To` —
//!   идентификаторы в угловых скобках, несколько — через пробел; адреса групп RFC 5322 — участниками группы.
//! - [`BodyPart`] (IMAP BODYSTRUCTURE, JMAP bodyStructure): пути — как у частей [`crate::split`]: корень письма —
//!   `""`, дети составной части — `p.1`, `p.2`, ...; вложенное `message/rfc822` с путём `p` содержит корень письма:
//!   составной — с путём `p`, несоставной — `p.1`. Размер — тела части в кодировке передачи; строки — у `text/*`
//!   и `message/rfc822`. Тип по умолчанию — `text/plain`.
//! - `preview` — начало текста (`text/plain` или текст из HTML) с пробелами, сжатыми до одного, не длиннее
//!   [`PREVIEW_CHARS`] символов; тело в UTF-8 с ошибочной меткой `charset` читается как UTF-8.
//!
//! Кодировки — полный набор `mail-parser` (`full_encoding`, encoding_rs): результат не зависит от того,
//! включили ли его другие крейты сборки.
//! - `thread_key` — первый идентификатор `References`, иначе `In-Reply-To`, иначе собственный `Message-ID`
//!   (письмо — начало цепочки), в угловых скобках; без идентификаторов — `subject:` и тема без `Re:`/`Fwd:`
//!   в нижнем регистре; пустая строка — нет ни идентификаторов, ни темы.
//! - `has_attachment` — есть части-вложения (в том числе встроенные изображения).

use std::borrow::Cow;

use mail_parser::{Encoding, HeaderValue, Message, MessageParser, MimeHeaders, PartType};
use time::{OffsetDateTime, UtcOffset};
use tp_contracts::meta::{Address, BodyPart, Envelope, MessageMeta};

/// Предел превью, символов (JMAP `preview`, RFC 8621 4.1.4; CONTRACTS.md `MessageMeta::preview`).
pub const PREVIEW_CHARS: usize = 256;

/// Предел вложенности MIME (как в [`crate::split`]): глубже части не описываются.
const MAX_DEPTH: usize = 64;

/// Метаданные письма. Не падает на битом MIME: что не разобралось, остаётся пустым.
pub fn message_meta(raw: &[u8]) -> MessageMeta {
    let Some(msg) = MessageParser::default().parse(raw) else {
        return MessageMeta {
            body: leaf_default(String::new()),
            ..MessageMeta::default()
        };
    };
    MessageMeta {
        envelope: envelope(&msg),
        body: message_body(&msg, "", 0),
        preview: preview(&msg),
        thread_key: thread_key(&msg),
        has_attachment: msg.attachment_count() > 0,
    }
}

fn envelope(msg: &Message<'_>) -> Envelope {
    let addrs = |a: Option<&mail_parser::Address<'_>>| -> Vec<Address> {
        a.map(|a| {
            a.iter()
                .filter_map(|addr| {
                    Some(Address {
                        name: addr.name().map(str::to_owned),
                        email: addr.address()?.to_owned(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
    };
    let in_reply_to = ids(msg.in_reply_to());
    Envelope {
        date: msg.date().and_then(datetime),
        subject: msg.subject().map(str::to_owned),
        from: addrs(msg.from()),
        sender: addrs(msg.sender()),
        reply_to: addrs(msg.reply_to()),
        to: addrs(msg.to()),
        cc: addrs(msg.cc()),
        bcc: addrs(msg.bcc()),
        in_reply_to: (!in_reply_to.is_empty()).then(|| bracketed(&in_reply_to)),
        message_id: msg.message_id().map(|id| bracketed(&[id])),
    }
}

/// Дата заголовка со смещением из него; недопустимая — `None`.
fn datetime(d: &mail_parser::DateTime) -> Option<OffsetDateTime> {
    if !d.is_valid() {
        return None;
    }
    let sign = if d.tz_before_gmt { -1 } else { 1 };
    let offset = sign * (i32::from(d.tz_hour) * 3600 + i32::from(d.tz_minute) * 60);
    let offset = UtcOffset::from_whole_seconds(offset).ok()?;
    OffsetDateTime::from_unix_timestamp(d.to_timestamp())
        .ok()?
        .checked_to_offset(offset)
}

/// Идентификаторы сообщений заголовка (без угловых скобок).
fn ids<'a>(value: &'a HeaderValue<'_>) -> Vec<&'a str> {
    match value {
        HeaderValue::Text(id) => vec![id.as_ref()],
        HeaderValue::TextList(list) => list.iter().map(|id| id.as_ref()).collect(),
        _ => Vec::new(),
    }
}

fn bracketed(ids: &[&str]) -> String {
    ids.iter()
        .map(|id| format!("<{id}>"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Корень письма (верхнего или вложенного с путём `base`).
fn message_body(msg: &Message<'_>, base: &str, depth: usize) -> BodyPart {
    let Some(root) = msg.parts.first() else {
        return leaf_default(base.to_owned());
    };
    match &root.body {
        PartType::Multipart(_) => node(msg, 0, base.to_owned(), base, depth),
        _ => {
            let body = child_path(base, 1);
            let path = if base.is_empty() {
                String::new()
            } else {
                body.clone()
            };
            node(msg, 0, path, &body, depth)
        }
    }
}

/// Узел части `id` с путём `path`; `base` — путь, от которого нумеруются дети составной части
/// и части вложенного письма (у корня несоставного письма отличается от `path`, как в split).
fn node(msg: &Message<'_>, id: usize, path: String, base: &str, depth: usize) -> BodyPart {
    let Some(part) = msg.parts.get(id) else {
        return leaf_default(path);
    };
    let (media_type, subtype, params) = match part.content_type() {
        Some(ct) => (
            ct.c_type.to_ascii_lowercase(),
            ct.c_subtype
                .as_deref()
                .map_or_else(|| default_subtype(&ct.c_type), str::to_ascii_lowercase),
            attributes(ct.attributes()),
        ),
        None => ("text".to_owned(), "plain".to_owned(), Vec::new()),
    };
    // Смещения частей — в буфере `raw_message` (у вложенного письма без кодирования передачи — буфер
    // верхнего письма), а `raw_message()` — лишь срез этого письма: по нему строки вложенных частей терялись.
    let body = msg
        .raw_message
        .get(part.offset_body as usize..part.offset_end as usize)
        .unwrap_or_default();
    let lines = (media_type == "text" || (media_type == "message" && subtype == "rfc822"))
        .then(|| u32::try_from(body.iter().filter(|&&b| b == b'\n').count()).unwrap_or(u32::MAX));
    let mut children = Vec::new();
    if depth < MAX_DEPTH {
        match &part.body {
            PartType::Multipart(ids) => {
                let mut n = 0;
                for &child in ids {
                    let child = child as usize;
                    // Дети идут после родителя; иное — битая структура (защита от циклов, как в split).
                    if child <= id || child >= msg.parts.len() {
                        continue;
                    }
                    n += 1;
                    let p = child_path(base, n);
                    children.push(node(msg, child, p.clone(), &p, depth + 1));
                }
            }
            PartType::Message(nested) => children.push(message_body(nested, base, depth + 1)),
            _ => {}
        }
    }
    BodyPart {
        part: path,
        media_type,
        subtype,
        params,
        content_id: part.content_id().map(|id| bracketed(&[id])),
        description: part.content_description().map(str::to_owned),
        encoding: part.content_transfer_encoding().map(str::to_owned),
        disposition: part
            .content_disposition()
            .map(|d| (d.c_type.to_ascii_lowercase(), attributes(d.attributes()))),
        language: ids(part.content_language())
            .into_iter()
            .map(str::to_owned)
            .collect(),
        location: part.content_location().map(str::to_owned),
        size: u64::from(part.offset_end.saturating_sub(part.offset_body)),
        lines,
        children,
    }
}

fn attributes(attrs: Option<&[mail_parser::Attribute<'_>]>) -> Vec<(String, String)> {
    attrs
        .unwrap_or_default()
        .iter()
        .map(|a| (a.name.to_ascii_lowercase(), a.value.to_string()))
        .collect()
}

fn default_subtype(media_type: &str) -> String {
    if media_type.eq_ignore_ascii_case("text") {
        "plain"
    } else {
        "octet-stream"
    }
    .to_owned()
}

fn leaf_default(part: String) -> BodyPart {
    BodyPart {
        part,
        media_type: "text".to_owned(),
        subtype: "plain".to_owned(),
        ..BodyPart::default()
    }
}

fn child_path(base: &str, n: usize) -> String {
    if base.is_empty() {
        n.to_string()
    } else {
        format!("{base}.{n}")
    }
}

/// Начало текста письма: пробельные символы сжаты до одного пробела, не длиннее [`PREVIEW_CHARS`].
fn preview(msg: &Message<'_>) -> String {
    let text: Cow<'_, str> = match mislabeled_utf8(msg) {
        Some(text) => Cow::Borrowed(text),
        None => msg.body_preview(usize::MAX).unwrap_or_default(),
    };
    let mut out = String::new();
    let mut n = 0;
    'words: for (i, word) in text.split_whitespace().enumerate() {
        for ch in (i > 0).then_some(' ').into_iter().chain(word.chars()) {
            if n == PREVIEW_CHARS {
                break 'words;
            }
            out.push(ch);
            n += 1;
        }
    }
    out.truncate(out.trim_end().len());
    out
}

/// Текст первой текстовой части, если он записан в UTF-8 вопреки метке `charset`: тело без кодирования
/// передачи не декодировалось по метке (есть U+FFFD), но как UTF-8 корректно. Частая ошибка почтовых
/// клиентов; без проверки превью зависело бы от набора кодировок `mail-parser`.
fn mislabeled_utf8<'a>(msg: &'a Message<'_>) -> Option<&'a str> {
    let part = msg.text_part(0)?;
    let PartType::Text(text) = &part.body else {
        return None;
    };
    if part.encoding != Encoding::None || !text.contains(char::REPLACEMENT_CHARACTER) {
        return None;
    }
    let raw = msg
        .raw_message()
        .get(part.offset_body as usize..part.offset_end as usize)?;
    std::str::from_utf8(raw).ok()
}

fn thread_key(msg: &Message<'_>) -> String {
    let root = ids(msg.references())
        .first()
        .copied()
        .or_else(|| ids(msg.in_reply_to()).first().copied())
        .or_else(|| msg.message_id());
    if let Some(id) = root {
        return bracketed(&[id]);
    }
    match msg.thread_name().map(str::trim) {
        Some(subject) if !subject.is_empty() => format!("subject:{}", subject.to_lowercase()),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn golden_corpus() {
        for mail in tp_testkit::corpus::corpus().expect("corpus") {
            let meta = message_meta(&mail.raw);
            assert!(
                meta.preview.chars().count() <= PREVIEW_CHARS,
                "{}",
                mail.name
            );
            insta::assert_json_snapshot!(mail.name.clone(), meta);
        }
    }

    #[tokio::test]
    async fn ingest_stores_meta() {
        use tp_contracts::mailstore::{Delivery, IngestRequest, MailStore};
        use tp_contracts::meta::{Flags, MessageStore, SpecialUse};

        let env = crate::testenv::Env::new();
        let scope = env.meta.account("a@example.org").await.expect("acc");
        let inbox = env
            .meta
            .mailbox_by_role(scope, SpecialUse::Inbox)
            .expect("inbox");
        let raw = tp_testkit::corpus::corpus_mail("rfc822-with-attachment")
            .expect("mail")
            .raw;
        let done = env
            .store
            .ingest(IngestRequest {
                tenant: scope.tenant,
                mail_from: String::new(),
                raw: crate::testenv::stream(&raw, 1024),
                deliveries: vec![Delivery {
                    account: scope.account,
                    mailbox: inbox,
                    extra_headers: vec![("Delivered-To".into(), "a@example.org".into())],
                    flags: Flags::new(),
                }],
                received_at: OffsetDateTime::now_utc(),
            })
            .await
            .expect("ingest");
        let m = MessageStore::get(&env.meta, scope, done.delivered[0].message)
            .await
            .expect("get")
            .expect("msg");
        assert_eq!(m.meta, message_meta(&raw));
        assert!(m.meta.has_attachment);
    }

    #[test]
    fn thread_key_links_replies() {
        let root = message_meta(b"Message-ID: <a@x>\r\nSubject: Plan\r\n\r\nbody\r\n");
        let reply = message_meta(
            b"Message-ID: <b@x>\r\nIn-Reply-To: <a@x>\r\nSubject: Re: Plan\r\n\r\nok\r\n",
        );
        let deep = message_meta(
            b"Message-ID: <c@x>\r\nReferences: <a@x> <b@x>\r\nIn-Reply-To: <b@x>\r\n\r\nok\r\n",
        );
        let other = message_meta(b"Message-ID: <d@x>\r\nSubject: Plan\r\n\r\nbody\r\n");
        assert_eq!(root.thread_key, "<a@x>");
        assert_eq!(reply.thread_key, root.thread_key);
        assert_eq!(deep.thread_key, root.thread_key);
        assert_ne!(other.thread_key, root.thread_key);
        assert_eq!(reply.envelope.in_reply_to.as_deref(), Some("<a@x>"));
        let anon = message_meta(b"Subject: Fwd: Re: Plan \r\n\r\nx\r\n");
        assert_eq!(anon.thread_key, "subject:plan");
        assert_eq!(message_meta(b"\r\n\r\n").thread_key, "");
    }

    #[test]
    fn nested_parts_count_lines() {
        let inner = "Subject: in

one
two
";
        let wrap = |cte: &str, body: &str| {
            format!(
                "Content-Type: multipart/mixed; boundary=b

--b
                 Content-Type: message/rfc822
{cte}
{body}
--b--
"
            )
        };
        let plain = message_meta(wrap("", inner).as_bytes());
        assert_eq!(plain.body.children[0].children[0].lines, Some(2));
        // Вложение в base64: смещения — в декодированных байтах вложения.
        let b64 = "U3ViamVjdDogaW4NCg0Kb25lDQp0d28NCg==";
        let encoded = message_meta(
            wrap(
                "Content-Transfer-Encoding: base64
",
                b64,
            )
            .as_bytes(),
        );
        assert_eq!(encoded.body.children[0].children[0].lines, Some(2));
    }

    #[test]
    fn preview_is_compact() {
        let body = format!(
            "Subject: x\r\n\r\n  one\r\n\r\n\ttwo   {}\r\n",
            "щ".repeat(400)
        );
        let meta = message_meta(body.as_bytes());
        assert!(meta.preview.starts_with("one two щщ"));
        assert_eq!(meta.preview.chars().count(), PREVIEW_CHARS);
    }
}
