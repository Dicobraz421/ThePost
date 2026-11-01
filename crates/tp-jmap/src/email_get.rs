//! Email/get и Thread (RFC 8621 §3, §4.2–4.3): `Email/get`, `Email/changes`, `Thread/get`, `Thread/changes`.
//!
//! Свойства из метаданных приёма (`MessageMeta`: конверт, структура MIME, превью, ключ цепочки) не читают
//! письмо. `references`, `headers` и `header:*` разбираются `mail-parser` из начала письма
//! (`JmapConfig::header_window`), `bodyValues` — из тел частей (`MailStore::fetch_part`).
//!
//! Письмо лежит ровно в одной папке (`maxMailboxesPerEmail` = 1). Идентификаторы: письмо — `MessageId`,
//! цепочка — `T` + хэш ключа цепочки, blob письма — `M<id>`, части — `M<id>_<путь IMAP через _>`.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use futures::TryStreamExt;
use mail_parser::{HeaderValue, MessageParser};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::format_description::well_known::Rfc3339;
use time::{OffsetDateTime, UtcOffset};
use tp_contracts::blob::ByteStream;
use tp_contracts::meta::{Address, BodyPart, Changes, Flag, Message};
use tp_contracts::{MessageId, Result};

use crate::core::error::{MethodError, MethodResult, changes_error};
use crate::core::request::{
    ChangesArgs, GetArgs, changes_response, map_changes, object, parse_args, parse_state,
    state_string,
};
use crate::core::{Ctx, thread_id};

/// Свойства Email по умолчанию (RFC 8621 §4.2).
const DEFAULT_PROPS: &[&str] = &[
    "id",
    "blobId",
    "threadId",
    "mailboxIds",
    "keywords",
    "size",
    "receivedAt",
    "messageId",
    "inReplyTo",
    "references",
    "sender",
    "from",
    "to",
    "cc",
    "bcc",
    "replyTo",
    "subject",
    "sentAt",
    "hasAttachment",
    "preview",
    "bodyValues",
    "textBody",
    "htmlBody",
    "attachments",
];

/// Остальные поддерживаемые свойства (кроме `header:*`).
const OTHER_PROPS: &[&str] = &["headers", "bodyStructure"];

/// Свойства частей по умолчанию; `subParts` — у составных частей `bodyStructure`.
const DEFAULT_BODY_PROPS: &[&str] = &[
    "partId",
    "blobId",
    "size",
    "name",
    "type",
    "charset",
    "disposition",
    "cid",
    "language",
    "location",
];

/// Ключевое слово JMAP флага; `\Deleted` в JMAP не отображается.
pub(crate) fn keyword(f: &Flag) -> Option<String> {
    Some(match f {
        Flag::Seen => "$seen".into(),
        Flag::Answered => "$answered".into(),
        Flag::Flagged => "$flagged".into(),
        Flag::Draft => "$draft".into(),
        Flag::Deleted => return None,
        Flag::Keyword(k) => k.to_lowercase(),
    })
}

/// Есть ли у письма ключевое слово (без учёта регистра).
pub(crate) fn has_keyword(m: &Message, kw: &str) -> bool {
    let kw = kw.to_lowercase();
    m.flags.iter().filter_map(keyword).any(|k| k == kw)
}

/// Идентификатор blob письма или части.
pub(crate) fn blob_id(m: MessageId, part: Option<&str>) -> String {
    match part {
        Some(p) => format!("M{m}_{}", p.replace('.', "_")),
        None => format!("M{m}"),
    }
}

/// `UTCDate`: секунды, без долей.
pub(crate) fn utc_date(t: OffsetDateTime) -> Value {
    let t = t
        .to_offset(UtcOffset::UTC)
        .replace_nanosecond(0)
        .unwrap_or(t);
    t.format(&Rfc3339).map_or(Value::Null, Value::String)
}

/// `Date`: со смещением отправителя.
fn date(t: OffsetDateTime) -> Value {
    t.replace_nanosecond(0)
        .unwrap_or(t)
        .format(&Rfc3339)
        .map_or(Value::Null, Value::String)
}

fn mp_date(d: &mail_parser::DateTime) -> Option<OffsetDateTime> {
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

fn addresses(list: &[Address]) -> Value {
    if list.is_empty() {
        return Value::Null;
    }
    json!(
        list.iter()
            .map(|a| json!({"name": a.name, "email": a.email}))
            .collect::<Vec<_>>()
    )
}

/// `<a> <b>` → `["a", "b"]`.
fn message_ids(s: Option<&str>) -> Value {
    let ids: Vec<&str> = s
        .unwrap_or_default()
        .split_whitespace()
        .map(|i| i.trim_matches(|c| c == '<' || c == '>'))
        .filter(|i| !i.is_empty())
        .collect();
    if ids.is_empty() {
        Value::Null
    } else {
        json!(ids)
    }
}

fn header_ids(v: &HeaderValue<'_>) -> Value {
    match v {
        HeaderValue::Text(id) => json!([id]),
        HeaderValue::TextList(list) => json!(list),
        _ => Value::Null,
    }
}

async fn collect(s: ByteStream) -> Result<Vec<u8>> {
    s.try_fold(Vec::new(), |mut acc, chunk| async move {
        acc.extend_from_slice(&chunk);
        Ok(acc)
    })
    .await
}

// ---------- Структура MIME ----------

fn param<'a>(params: &'a [(String, String)], name: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

fn mime(bp: &BodyPart) -> String {
    format!("{}/{}", bp.media_type, bp.subtype)
}

fn is_multipart(bp: &BodyPart) -> bool {
    bp.media_type == "multipart"
}

/// Путь листовой части для `fetch_part`: тело несоставного письма — `1`.
fn leaf_path(bp: &BodyPart) -> String {
    if bp.part.is_empty() {
        "1".into()
    } else {
        bp.part.clone()
    }
}

fn part_name(bp: &BodyPart) -> Option<&str> {
    bp.disposition
        .as_ref()
        .and_then(|(_, p)| param(p, "filename"))
        .or_else(|| param(&bp.params, "name"))
}

fn is_inline_media(bp: &BodyPart) -> bool {
    matches!(bp.media_type.as_str(), "image" | "audio" | "video")
}

/// Части JMAP: `message/rfc822` — лист, его структура не раскрывается.
fn sub_parts(bp: &BodyPart) -> &[BodyPart] {
    if is_multipart(bp) { &bp.children } else { &[] }
}

/// Объект EmailBodyPart.
fn part_json(m: &Message, bp: &BodyPart, props: &[String], tree: bool) -> Value {
    let multi = is_multipart(bp);
    let path = leaf_path(bp);
    let mut o = Map::new();
    for p in props {
        let v = match p.as_str() {
            "partId" if multi => Value::Null,
            "partId" => json!(path),
            "blobId" if multi => Value::Null,
            "blobId" => json!(blob_id(m.id, Some(&path))),
            "size" => {
                let base64 = bp
                    .encoding
                    .as_deref()
                    .is_some_and(|e| e.eq_ignore_ascii_case("base64"));
                json!(if base64 { bp.size / 4 * 3 } else { bp.size })
            }
            "name" => json!(part_name(bp)),
            "type" => json!(mime(bp)),
            "charset" => match param(&bp.params, "charset") {
                Some(c) => json!(c),
                None if bp.media_type == "text" => json!("us-ascii"),
                None => Value::Null,
            },
            "disposition" => json!(bp.disposition.as_ref().map(|(d, _)| d)),
            "cid" => json!(
                bp.content_id
                    .as_deref()
                    .map(|c| c.trim_matches(|c| c == '<' || c == '>'))
            ),
            "language" if bp.language.is_empty() => Value::Null,
            "language" => json!(bp.language),
            "location" => json!(bp.location),
            _ => continue,
        };
        o.insert(p.clone(), v);
    }
    if tree && multi {
        let subs: Vec<Value> = sub_parts(bp)
            .iter()
            .map(|c| part_json(m, c, props, true))
            .collect();
        o.insert("subParts".into(), json!(subs));
    }
    Value::Object(o)
}

/// Тела и вложения (RFC 8621 §4.1.4, алгоритм `parseStructure`).
#[derive(Default)]
struct Bodies<'a> {
    text: Vec<&'a BodyPart>,
    html: Vec<&'a BodyPart>,
    attachments: Vec<&'a BodyPart>,
}

impl<'a> Bodies<'a> {
    fn of(root: &'a BodyPart) -> Self {
        let mut b = Bodies::default();
        b.walk(std::slice::from_ref(root), "mixed", false, true, true);
        b
    }

    fn walk(
        &mut self,
        parts: &'a [BodyPart],
        multi: &str,
        in_alt: bool,
        mut text_on: bool,
        mut html_on: bool,
    ) {
        let (text_len, html_len) = (self.text.len(), self.html.len());
        for (i, part) in parts.iter().enumerate() {
            let t = mime(part);
            let inline = part
                .disposition
                .as_ref()
                .is_none_or(|(d, _)| d != "attachment")
                && (t == "text/plain" || t == "text/html" || is_inline_media(part))
                && (i == 0
                    || (multi != "related"
                        && (is_inline_media(part) || part_name(part).is_none())));
            if is_multipart(part) {
                let sub = part.subtype.as_str();
                self.walk(
                    &part.children,
                    sub,
                    in_alt || sub == "alternative",
                    text_on,
                    html_on,
                );
            } else if inline {
                if multi == "alternative" {
                    match t.as_str() {
                        "text/plain" => self.text.push(part),
                        "text/html" => self.html.push(part),
                        _ => self.attachments.push(part),
                    }
                    continue;
                } else if in_alt {
                    if t == "text/plain" {
                        html_on = false;
                    }
                    if t == "text/html" {
                        text_on = false;
                    }
                }
                if text_on {
                    self.text.push(part);
                }
                if html_on {
                    self.html.push(part);
                }
                if (!text_on || !html_on) && is_inline_media(part) {
                    self.attachments.push(part);
                }
            } else {
                self.attachments.push(part);
            }
        }
        if multi == "alternative" && text_on && html_on {
            if text_len == self.text.len() && html_len != self.html.len() {
                let extra: Vec<_> = self.html[html_len..].to_vec();
                self.text.extend(extra);
            }
            if html_len == self.html.len() && text_len != self.text.len() {
                let extra: Vec<_> = self.text[text_len..].to_vec();
                self.html.extend(extra);
            }
        }
    }
}

// ---------- Заголовки ----------

/// Разобранное свойство `header:Имя[:asФорма][:all]`.
struct HeaderProp<'a> {
    name: &'a str,
    form: &'a str,
    all: bool,
}

fn header_prop(p: &str) -> Option<HeaderProp<'_>> {
    let rest = p.strip_prefix("header:")?;
    let mut it = rest.split(':');
    let name = it.next().filter(|n| !n.is_empty())?;
    let mut form = "asRaw";
    let mut all = false;
    for x in it {
        match x {
            "all" if !all => all = true,
            "asRaw" | "asText" | "asAddresses" | "asGroupedAddresses" | "asMessageIds"
            | "asDate" | "asURLs"
                if !all && form == "asRaw" =>
            {
                form = x;
            }
            _ => return None,
        }
    }
    Some(HeaderProp { name, form, all })
}

/// Заголовки письма: имя и значение как в письме (после `:`, без завершающего CRLF).
fn raw_headers(raw: &[u8]) -> Vec<(String, String)> {
    let Some(msg) = MessageParser::default().parse_headers(raw) else {
        return Vec::new();
    };
    msg.headers()
        .iter()
        .map(|h| {
            let v = raw
                .get(h.offset_start() as usize..h.offset_end() as usize)
                .map(String::from_utf8_lossy)
                .unwrap_or_default();
            (
                h.name().to_owned(),
                v.trim_end_matches(['\r', '\n']).to_owned(),
            )
        })
        .collect()
}

/// Значение заголовка в форме JMAP (RFC 8621 §4.1.2): разбор `mail-parser` под именем заголовка этого вида.
fn header_form(form: &str, raw: &str) -> Value {
    let parse = |name: &str| {
        let data = format!("{name}:{raw}\r\n\r\n");
        MessageParser::default()
            .parse_headers(data.as_bytes())
            .map(|m| m.into_owned())
    };
    let addr = |a: &mail_parser::Addr<'_>| json!({"name": a.name(), "email": a.address().unwrap_or_default()});
    match form {
        "asRaw" => json!(raw),
        "asText" => json!(
            parse("Subject")
                .as_ref()
                .and_then(|m| m.subject().map(str::to_owned))
        ),
        "asAddresses" => {
            let list: Vec<Value> = parse("To")
                .as_ref()
                .and_then(|m| m.to().map(|a| a.iter().map(addr).collect()))
                .unwrap_or_default();
            json!(list)
        }
        "asGroupedAddresses" => {
            let groups: Vec<Value> = match parse("To").as_ref().and_then(|m| m.to().cloned()) {
                Some(mail_parser::Address::Group(gs)) => gs
                    .iter()
                    .map(|g| json!({"name": g.name, "addresses": g.addresses.iter().map(addr).collect::<Vec<_>>()}))
                    .collect(),
                Some(mail_parser::Address::List(l)) => {
                    vec![json!({"name": null, "addresses": l.iter().map(addr).collect::<Vec<_>>()})]
                }
                None => Vec::new(),
            };
            json!(groups)
        }
        "asMessageIds" => parse("References").map_or(Value::Null, |m| header_ids(m.references())),
        "asDate" => parse("Date")
            .as_ref()
            .and_then(|m| m.date().and_then(mp_date))
            .map_or(Value::Null, date),
        "asURLs" => {
            let urls: Vec<&str> = raw
                .split('<')
                .skip(1)
                .filter_map(|s| s.split_once('>').map(|(u, _)| u.trim()))
                .collect();
            if urls.is_empty() {
                Value::Null
            } else {
                json!(urls)
            }
        }
        _ => Value::Null,
    }
}

// ---------- Email/get ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EmailGetArgs {
    #[serde(flatten)]
    get: GetArgs,
    body_properties: Option<Vec<String>>,
    #[serde(default)]
    fetch_text_body_values: bool,
    #[serde(default, rename = "fetchHTMLBodyValues")]
    fetch_html_body_values: bool,
    #[serde(default)]
    fetch_all_body_values: bool,
    #[serde(default)]
    max_body_value_bytes: u64,
}

/// Текст части: тело в кодировке передачи декодируется `mail-parser` (кодировка передачи и `charset`).
fn decode_text(bp: &BodyPart, body: &[u8]) -> Option<String> {
    let charset = param(&bp.params, "charset")
        .map(|c| format!("; charset=\"{c}\""))
        .unwrap_or_default();
    let enc = bp.encoding.as_deref().unwrap_or("7bit");
    let mut data = format!(
        "Content-Type: {}{charset}\r\nContent-Transfer-Encoding: {enc}\r\n\r\n",
        mime(bp)
    )
    .into_bytes();
    data.extend_from_slice(body);
    let msg = MessageParser::default().parse(&data)?;
    let text = msg.parts.first()?.text_contents()?;
    // Тело без кодирования передачи в UTF-8 вопреки метке `charset` — частая ошибка клиентов
    // (как у превью в `tp_mailstore::meta`).
    let plain = matches!(
        enc.to_ascii_lowercase().as_str(),
        "7bit" | "8bit" | "binary"
    );
    if plain
        && text.contains(char::REPLACEMENT_CHARACTER)
        && let Ok(utf8) = std::str::from_utf8(body)
    {
        return Some(utf8.to_owned());
    }
    Some(text.to_owned())
}

async fn body_value(ctx: &Ctx<'_>, m: &Message, bp: &BodyPart, max: u64) -> MethodResult<Value> {
    let body = collect(
        ctx.jmap
            .d
            .store
            .fetch_part(ctx.scope, m.id, &leaf_path(bp))
            .await?,
    )
    .await?;
    let (mut value, mut problem) = match decode_text(bp, &body) {
        Some(t) => {
            let bad = t.contains(char::REPLACEMENT_CHARACTER);
            (t, bad)
        }
        None => (String::from_utf8_lossy(&body).into_owned(), true),
    };
    problem |= value.is_empty() && !body.is_empty();
    let mut truncated = false;
    if let Ok(max) = usize::try_from(max)
        && max > 0
        && value.len() > max
    {
        let mut cut = max;
        while !value.is_char_boundary(cut) {
            cut -= 1;
        }
        value.truncate(cut);
        truncated = true;
    }
    Ok(json!({"value": value, "isEncodingProblem": problem, "isTruncated": truncated}))
}

fn check_props(props: &[String]) -> MethodResult<()> {
    for p in props {
        let ok = DEFAULT_PROPS.contains(&p.as_str())
            || OTHER_PROPS.contains(&p.as_str())
            || header_prop(p).is_some();
        if !ok {
            return Err(MethodError::invalid(format!("unknown property: {p}")));
        }
    }
    Ok(())
}

/// Объект Email с запрошенными свойствами.
async fn email_json(
    ctx: &Ctx<'_>,
    m: &Message,
    props: &[String],
    a: &EmailGetArgs,
    body_props: &[String],
) -> MethodResult<Value> {
    let need_headers = props
        .iter()
        .any(|p| p == "references" || p == "headers" || p.starts_with("header:"));
    let raw = if need_headers {
        let end = m.size.min(ctx.jmap.cfg.header_window);
        collect(
            ctx.jmap
                .d
                .store
                .fetch_raw(ctx.scope, m.id, Some(0..end))
                .await?,
        )
        .await?
    } else {
        Vec::new()
    };
    let headers = if need_headers {
        raw_headers(&raw)
    } else {
        Vec::new()
    };
    let env = &m.meta.envelope;
    let bodies = Bodies::of(&m.meta.body);
    let parts = |list: &[&BodyPart]| {
        json!(
            list.iter()
                .map(|b| part_json(m, b, body_props, false))
                .collect::<Vec<_>>()
        )
    };
    let mut o = Map::new();
    for p in props {
        let v = match p.as_str() {
            "id" => json!(m.id.to_string()),
            "blobId" => json!(blob_id(m.id, None)),
            "threadId" => json!(thread_id(m)),
            "mailboxIds" => json!({ m.mailbox.to_string(): true }),
            "keywords" => {
                let kw: Map<String, Value> = m
                    .flags
                    .iter()
                    .filter_map(keyword)
                    .map(|k| (k, json!(true)))
                    .collect();
                Value::Object(kw)
            }
            "size" => json!(m.size),
            "receivedAt" => utc_date(m.received_at),
            "messageId" => message_ids(env.message_id.as_deref()),
            "inReplyTo" => message_ids(env.in_reply_to.as_deref()),
            "references" => MessageParser::default()
                .parse_headers(&raw)
                .map_or(Value::Null, |msg| header_ids(msg.references())),
            "sender" => addresses(&env.sender),
            "from" => addresses(&env.from),
            "to" => addresses(&env.to),
            "cc" => addresses(&env.cc),
            "bcc" => addresses(&env.bcc),
            "replyTo" => addresses(&env.reply_to),
            "subject" => json!(env.subject),
            "sentAt" => env.date.map_or(Value::Null, date),
            "hasAttachment" => json!(m.meta.has_attachment),
            "preview" => json!(m.meta.preview),
            "headers" => json!(
                headers
                    .iter()
                    .map(|(n, v)| json!({"name": n, "value": v}))
                    .collect::<Vec<_>>()
            ),
            "bodyStructure" => part_json(m, &m.meta.body, body_props, true),
            "textBody" => parts(&bodies.text),
            "htmlBody" => parts(&bodies.html),
            "attachments" => parts(&bodies.attachments),
            "bodyValues" => {
                let mut wanted: Vec<&BodyPart> = Vec::new();
                if a.fetch_text_body_values || a.fetch_all_body_values {
                    wanted.extend(&bodies.text);
                }
                if a.fetch_html_body_values || a.fetch_all_body_values {
                    wanted.extend(&bodies.html);
                }
                let mut seen = HashSet::new();
                let mut values = Map::new();
                for bp in wanted {
                    if bp.media_type == "text" && seen.insert(leaf_path(bp)) {
                        values.insert(
                            leaf_path(bp),
                            body_value(ctx, m, bp, a.max_body_value_bytes).await?,
                        );
                    }
                }
                Value::Object(values)
            }
            other => match header_prop(other) {
                Some(h) => {
                    let vals: Vec<Value> = headers
                        .iter()
                        .filter(|(n, _)| n.eq_ignore_ascii_case(h.name))
                        .map(|(_, v)| header_form(h.form, v))
                        .collect();
                    if h.all {
                        json!(vals)
                    } else {
                        vals.last().cloned().unwrap_or(Value::Null)
                    }
                }
                None => continue,
            },
        };
        o.insert(p.clone(), v);
    }
    Ok(Value::Object(o))
}

/// `Email/get`.
pub(crate) async fn get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: EmailGetArgs = parse_args(args)?;
    ctx.check_account(&a.get.account_id)?;
    ctx.check_get_limit(a.get.ids.as_ref())?;
    let props: Vec<String> = match &a.get.properties {
        Some(p) => {
            let mut p = p.clone();
            if !p.iter().any(|x| x == "id") {
                p.insert(0, "id".into());
            }
            p
        }
        None => DEFAULT_PROPS.iter().map(|s| (*s).to_owned()).collect(),
    };
    check_props(&props)?;
    let body_props: Vec<String> = match &a.body_properties {
        Some(b) => {
            if let Some(bad) = b
                .iter()
                .find(|p| !DEFAULT_BODY_PROPS.contains(&p.as_str()) && *p != "subParts")
            {
                return Err(MethodError::invalid(format!(
                    "unsupported body property: {bad}"
                )));
            }
            b.clone()
        }
        None => DEFAULT_BODY_PROPS.iter().map(|s| (*s).to_owned()).collect(),
    };
    let state = ctx.state().await?;
    let d = &ctx.jmap.d;
    let mut found = Vec::new();
    let mut not_found = Vec::new();
    match &a.get.ids {
        Some(ids) => {
            for id in ids {
                let msg = match MessageId::from_str(ctx.resolve_id(id)) {
                    Ok(mid) => d.messages.get(ctx.scope, mid).await?,
                    Err(_) => None,
                };
                match msg {
                    Some(m) => found.push(m),
                    None => not_found.push(id.clone()),
                }
            }
        }
        None => {
            found = ctx.jmap.messages_in(ctx.scope, None).await?;
            if found.len() > ctx.jmap.cfg.max_objects_in_get as usize {
                return Err(MethodError::new("requestTooLarge"));
            }
        }
    }
    let mut list = Vec::with_capacity(found.len());
    for m in &found {
        list.push(email_json(ctx, m, &props, &a, &body_props).await?);
    }
    Ok(object(json!({
        "accountId": a.get.account_id,
        "state": state_string(state),
        "list": list,
        "notFound": not_found,
    })))
}

/// `Email/changes` по журналу изменений писем учётной записи.
pub(crate) async fn changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: ChangesArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let since = parse_state(&a.since_state)?;
    let limit = a.limit(ctx.jmap.cfg.max_changes)?;
    let c = ctx
        .jmap
        .d
        .changes
        .message_changes(ctx.scope, None, since, limit)
        .await
        .map_err(changes_error)?;
    let c = map_changes(c, |m| m.id.to_string());
    Ok(changes_response(&a.account_id, &a.since_state, &c))
}

/// Цепочки учётной записи: идентификатор → письма по возрастанию `receivedAt`.
pub(crate) fn threads(msgs: &[Message]) -> HashMap<String, Vec<&Message>> {
    let mut map: HashMap<String, Vec<&Message>> = HashMap::new();
    for m in msgs {
        map.entry(thread_id(m)).or_default().push(m);
    }
    for list in map.values_mut() {
        list.sort_by_key(|m| (m.received_at, m.id));
    }
    map
}

/// `Thread/get`.
pub(crate) async fn thread_get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let state = ctx.state().await?;
    let msgs = ctx.jmap.messages_in(ctx.scope, None).await?;
    let map = threads(&msgs);
    let thread = |id: &str, list: &[&Message]| json!({"id": id, "emailIds": list.iter().map(|m| m.id.to_string()).collect::<Vec<_>>()});
    let mut list = Vec::new();
    let mut not_found = Vec::new();
    match &a.ids {
        Some(ids) => {
            for id in ids {
                match map.get(id) {
                    Some(l) => list.push(thread(id, l)),
                    None => not_found.push(id.clone()),
                }
            }
        }
        None => list.extend(map.iter().map(|(id, l)| thread(id, l))),
    }
    Ok(object(json!({
        "accountId": a.account_id,
        "state": state_string(state),
        "list": list,
        "notFound": not_found,
    })))
}

/// `Thread/changes`: цепочки изменённых писем; созданная — все её письма созданы после `sinceState`.
/// Цепочку удалённого письма журнал не хранит, поэтому при удалённых письмах — `cannotCalculateChanges`
/// (клиент перечитывает цепочки).
pub(crate) async fn thread_changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: ChangesArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let since = parse_state(&a.since_state)?;
    let limit = a.limit(ctx.jmap.cfg.max_changes)?;
    let c = ctx
        .jmap
        .d
        .changes
        .message_changes(ctx.scope, None, since, limit)
        .await
        .map_err(changes_error)?;
    if !c.destroyed.is_empty() {
        return Err(MethodError::new("cannotCalculateChanges"));
    }
    let created: HashSet<MessageId> = c.created.iter().map(|x| x.id).collect();
    let touched: HashSet<MessageId> = c.created.iter().chain(&c.updated).map(|x| x.id).collect();
    let mut out = Changes {
        created: Vec::new(),
        updated: Vec::new(),
        destroyed: Vec::new(),
        new_state: c.new_state,
        has_more: c.has_more,
    };
    if !touched.is_empty() {
        let msgs = ctx.jmap.messages_in(ctx.scope, None).await?;
        let mut ids: Vec<(String, Vec<&Message>)> = threads(&msgs).into_iter().collect();
        ids.sort_by(|x, y| x.0.cmp(&y.0));
        for (id, list) in ids {
            if !list.iter().any(|m| touched.contains(&m.id)) {
                continue;
            }
            if list.iter().all(|m| created.contains(&m.id)) {
                out.created.push(id);
            } else {
                out.updated.push(id);
            }
        }
    }
    Ok(changes_response(&a.account_id, &a.since_state, &out))
}

#[cfg(test)]
mod tests {
    use serde_json::{Map, Value, json};
    use tp_contracts::mailstore::MailStore;
    use tp_contracts::meta::{Flag, FlagOp, SpecialUse};

    use crate::testenv::{Env, mail};

    #[tokio::test]
    async fn email_get_properties() {
        let env = Env::new().await;
        let inbox = env.mailbox(SpecialUse::Inbox);
        let a = env
            .deliver(
                inbox,
                &mail(
                    "=?UTF-8?B?0J/RgNC40LLQtdGC?=",
                    "\"Ann\" <ann@example.com>",
                    "Message-ID: <m1@x>\r\nList-Post: <mailto:list@x>, <https://x/p>\r\n\
                     X-Tag: one\r\nX-Tag: two\r\nCc: Team: b@x.org, c@x.org;\r\n",
                    "Привет",
                ),
                [Flag::Seen, Flag::Keyword("$Forwarded".into())].into(),
            )
            .await;
        let b = env
            .deliver(
                inbox,
                &mail(
                    "Re: x",
                    "b@x.org",
                    "Message-ID: <m2@x>\r\nIn-Reply-To: <m1@x>\r\nReferences: <m1@x>\r\n",
                    "ok",
                ),
                Default::default(),
            )
            .await;
        let acc = env.account();
        let r = env
            .call(
                "Email/get",
                json!({"accountId": acc, "ids": [a.to_string(), b.to_string(), "nope"],
                    "fetchTextBodyValues": true}),
                "Email/get",
            )
            .await;
        let (ea, eb) = (&r["list"][0], &r["list"][1]);
        assert_eq!(r["notFound"], json!(["nope"]));
        assert_eq!(ea["subject"], "Привет");
        assert_eq!(
            ea["from"],
            json!([{"name": "Ann", "email": "ann@example.com"}])
        );
        assert_eq!(ea["keywords"], json!({"$seen": true, "$forwarded": true}));
        assert_eq!(ea["mailboxIds"], json!({inbox.to_string(): true}));
        assert_eq!(ea["messageId"], json!(["m1@x"]));
        assert_eq!(ea["references"], Value::Null);
        assert_eq!(eb["inReplyTo"], json!(["m1@x"]));
        assert_eq!(eb["references"], json!(["m1@x"]));
        assert_eq!(ea["threadId"], eb["threadId"]);
        assert_eq!(ea["sentAt"], "2026-10-30T10:00:00+03:00");
        assert!(ea["receivedAt"].as_str().is_some_and(|d| d.ends_with('Z')));
        assert_eq!(ea["textBody"][0]["type"], "text/plain");
        let part = ea["textBody"][0]["partId"]
            .as_str()
            .expect("partId")
            .to_owned();
        assert_eq!(ea["bodyValues"][&part]["value"], "Привет\r\n");
        assert_eq!(ea["bodyValues"][&part]["isTruncated"], false);
        assert_eq!(ea["htmlBody"], ea["textBody"]);
        assert_eq!(ea["attachments"], json!([]));
        assert_eq!(ea["hasAttachment"], false);
        assert!(ea["blobId"].as_str().is_some_and(|b| b.starts_with('M')));

        let r = env
            .call(
                "Email/get",
                json!({"accountId": acc, "ids": [a.to_string()], "properties": [
                    "header:Subject:asText", "header:X-Tag:all", "header:X-Tag", "header:List-Post:asURLs",
                    "header:Cc:asGroupedAddresses", "header:Cc:asAddresses", "header:Date:asDate",
                    "header:Message-ID:asMessageIds", "header:X-None", "headers", "bodyStructure"],
                    "bodyProperties": ["partId", "type", "size"], "fetchHTMLBodyValues": true,
                    "maxBodyValueBytes": 3}),
                "Email/get",
            )
            .await;
        let e = &r["list"][0];
        assert_eq!(e["id"], a.to_string());
        assert_eq!(e["header:Subject:asText"], "Привет");
        assert_eq!(e["header:X-Tag:all"], json!([" one", " two"]));
        assert_eq!(e["header:X-Tag"], " two");
        assert_eq!(
            e["header:List-Post:asURLs"],
            json!(["mailto:list@x", "https://x/p"])
        );
        assert_eq!(
            e["header:Cc:asGroupedAddresses"],
            json!([{"name": "Team", "addresses": [
                {"name": null, "email": "b@x.org"}, {"name": null, "email": "c@x.org"}]}])
        );
        assert_eq!(e["header:Cc:asAddresses"].as_array().map(Vec::len), Some(2));
        assert_eq!(e["header:Date:asDate"], "2026-10-30T10:00:00+03:00");
        assert_eq!(e["header:Message-ID:asMessageIds"], json!(["m1@x"]));
        assert_eq!(e["header:X-None"], Value::Null);
        assert!(
            e["headers"]
                .as_array()
                .is_some_and(|h| h.iter().any(|x| x["name"] == "List-Post"))
        );
        assert_eq!(e["bodyStructure"]["type"], "text/plain");
        assert!(e.get("subject").is_none());
        let r = env
            .calls(json!([
                ["Email/get", {"accountId": acc, "ids": [], "properties": ["bogus"]}, "a"],
                ["Email/get", {"accountId": acc, "ids": [], "bodyProperties": ["headers"]}, "b"]
            ]))
            .await;
        assert_eq!(r[0][1]["type"], "invalidArguments");
        assert_eq!(r[1][1]["type"], "invalidArguments");
    }

    /// Снимок Email/get на корпусе tp-testkit: адреса, даты, структура MIME, тела и вложения, тексты частей.
    #[tokio::test]
    async fn corpus_snapshot() {
        let env = Env::new().await;
        let inbox = env.mailbox(SpecialUse::Inbox);
        let corpus = tokio::task::spawn_blocking(tp_testkit::corpus::corpus)
            .await
            .expect("join")
            .expect("corpus");
        let mut ids = Vec::new();
        for m in &corpus {
            ids.push((
                m.name.clone(),
                env.deliver(inbox, &m.raw, Default::default()).await,
            ));
        }
        let props = [
            "size",
            "messageId",
            "inReplyTo",
            "references",
            "sender",
            "from",
            "to",
            "cc",
            "bcc",
            "replyTo",
            "subject",
            "sentAt",
            "hasAttachment",
            "preview",
            "bodyStructure",
            "textBody",
            "htmlBody",
            "attachments",
            "bodyValues",
        ];
        let body = [
            "partId",
            "size",
            "name",
            "type",
            "charset",
            "disposition",
            "cid",
            "language",
            "location",
        ];
        let mut out = Map::new();
        for (name, id) in ids {
            let r = env
                .call(
                    "Email/get",
                    json!({"accountId": env.account(), "ids": [id.to_string()], "properties": props,
                        "bodyProperties": body, "fetchAllBodyValues": true, "maxBodyValueBytes": 200}),
                    "Email/get",
                )
                .await;
            let mut e = r["list"][0].clone();
            if let Some(o) = e.as_object_mut() {
                o.remove("id");
            }
            out.insert(name, e);
        }
        insta::assert_json_snapshot!("corpus", Value::Object(out));
    }

    #[tokio::test]
    async fn email_and_thread_changes() {
        let env = Env::new().await;
        let acc = env.account();
        let inbox = env.mailbox(SpecialUse::Inbox);
        let s0 = env
            .call(
                "Email/get",
                json!({"accountId": acc, "ids": []}),
                "Email/get",
            )
            .await["state"]
            .clone();
        let a = env
            .deliver(
                inbox,
                &mail("t", "a@x.org", "Message-ID: <t1@x>\r\n", "1"),
                Default::default(),
            )
            .await;
        let c = env
            .call(
                "Email/changes",
                json!({"accountId": acc, "sinceState": s0}),
                "Email/changes",
            )
            .await;
        assert_eq!(c["created"], json!([a.to_string()]));
        let s1 = c["newState"].clone();
        let t = env
            .call(
                "Thread/changes",
                json!({"accountId": acc, "sinceState": s0}),
                "Thread/changes",
            )
            .await;
        assert_eq!(t["created"].as_array().map(Vec::len), Some(1));
        let tid = t["created"][0].clone();

        let b = env
            .deliver(
                inbox,
                &mail("Re: t", "b@x.org", "References: <t1@x>\r\n", "2"),
                Default::default(),
            )
            .await;
        let t = env
            .call(
                "Thread/changes",
                json!({"accountId": acc, "sinceState": s1}),
                "Thread/changes",
            )
            .await;
        assert_eq!(t["updated"], json!([tid]));
        assert_eq!(t["created"], json!([]));
        let g = env
            .call(
                "Thread/get",
                json!({"accountId": acc, "ids": [tid, "T0"]}),
                "Thread/get",
            )
            .await;
        assert_eq!(
            g["list"][0]["emailIds"],
            json!([a.to_string(), b.to_string()])
        );
        assert_eq!(g["notFound"], json!(["T0"]));

        env.store
            .set_flags(env.scope, &[a], FlagOp::Add([Flag::Flagged].into()), None)
            .await
            .expect("flags");
        let c = env
            .call(
                "Email/changes",
                json!({"accountId": acc, "sinceState": s1, "maxChanges": 10}),
                "Email/changes",
            )
            .await;
        assert_eq!(c["updated"], json!([a.to_string()]));
        assert_eq!(c["created"], json!([b.to_string()]));
        let s2 = c["newState"].clone();
        env.store
            .move_to(env.scope, &[b], env.mailbox(SpecialUse::Junk))
            .await
            .expect("move");
        let c = env
            .call(
                "Email/changes",
                json!({"accountId": acc, "sinceState": s2}),
                "Email/changes",
            )
            .await;
        assert_eq!(c["destroyed"], json!([b.to_string()]));
        let r = env
            .calls(json!([["Thread/changes", {"accountId": acc, "sinceState": s2}, "x"]]))
            .await;
        assert_eq!(r[0][1]["type"], "cannotCalculateChanges");
    }
}
