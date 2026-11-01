//! `Email/set`, `Email/import`, `Email/copy` (RFC 8621 §4.6–4.8).
//!
//! - Изменение: `keywords` (целиком или `keywords/<слово>`) — `MailStore::set_flags`; `mailboxIds` — одна папка
//!   (`maxMailboxesPerEmail` = 1), перенос — `MailStore::move_to`. Перенос создаёт новое письмо (CONTRACTS.md#Metadata),
//!   поэтому `updated` содержит новый `id`.
//! - Создание — черновик: MIME собирает `mail-builder` из адресов, темы, `textBody`/`htmlBody` (текст из `bodyValues`),
//!   вложений из blob и полей `header:<имя>`; письмо принимается `MailStore::ingest`. `bodyStructure` не поддерживается.
//! - Удаление — в корзину (`MailStore::delete_to_trash`); письмо уже в корзине — `notDestroyed`: окончательное
//!   удаление выполняет maintenance по сроку корзины (SRS 4.7).
//! - `Email/import` — письмо из blob, `MailStore::ingest` с одной доставкой.
//! - `Email/copy` — между учётными записями; доступна только своя, поэтому копировать неоткуда (`fromAccountNotFound`).

use std::collections::BTreeSet;
use std::str::FromStr;

use axum::body::Bytes;
use mail_builder::MessageBuilder;
use mail_builder::headers::HeaderType;
use mail_builder::headers::address::Address;
use mail_builder::headers::date::Date;
use mail_builder::headers::message_id::MessageId as MsgIdHeader;
use mail_builder::headers::raw::Raw;
use mail_builder::headers::text::Text;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tp_contracts::mailstore::{Delivery, IngestRequest};
use tp_contracts::meta::{Flag, FlagOp, Flags, SpecialUse};
use tp_contracts::{Error, MailboxId, MessageId};

use crate::core::error::{MethodError, MethodResult, set_error};
use crate::core::request::{object, parse_args, state_string};
use crate::core::{Ctx, thread_id};
use crate::email_get::blob_id;

/// Флаг по ключевому слову JMAP (RFC 8621 §4.1.1); недопустимое слово — `None`.
pub(crate) fn flag_of(kw: &str) -> Option<Flag> {
    let bad = |b: u8| !(0x21..=0x7e).contains(&b) || b"(){]%*\"\\".contains(&b);
    if kw.is_empty() || kw.len() > 255 || kw.bytes().any(bad) {
        return None;
    }
    let k = kw.to_ascii_lowercase();
    Some(match k.as_str() {
        "$seen" => Flag::Seen,
        "$answered" => Flag::Answered,
        "$flagged" => Flag::Flagged,
        "$draft" => Flag::Draft,
        _ => Flag::Keyword(k),
    })
}

/// Флаги из объекта `keywords` (`{слово: true}`).
fn keywords(v: &Value) -> Option<Flags> {
    let Value::Object(m) = v else {
        return None;
    };
    m.iter()
        .map(|(k, v)| (*v == Value::Bool(true)).then(|| flag_of(k)).flatten())
        .collect()
}

fn invalid(props: &[&str]) -> Value {
    set_error("invalidProperties", None, props)
}

fn or_null(m: Map<String, Value>) -> Value {
    if m.is_empty() {
        Value::Null
    } else {
        Value::Object(m)
    }
}

/// Папка по ссылке (`id` или `#cid`), только существующая.
async fn mailbox(ctx: &Ctx<'_>, id: &str) -> MethodResult<Option<MailboxId>> {
    let Ok(mid) = MailboxId::from_str(ctx.resolve_id(id)) else {
        return Ok(None);
    };
    Ok(ctx
        .jmap
        .d
        .mailboxes
        .get(ctx.scope, mid)
        .await?
        .map(|m| m.id))
}

/// Единственная папка из `mailboxIds`.
async fn single_mailbox(ctx: &Ctx<'_>, v: &Value) -> MethodResult<Result<MailboxId, Value>> {
    let Value::Object(m) = v else {
        return Ok(Err(invalid(&["mailboxIds"])));
    };
    let ids: Vec<&String> = m
        .iter()
        .filter(|(_, v)| **v == Value::Bool(true))
        .map(|(k, _)| k)
        .collect();
    if ids.len() != m.len() || ids.is_empty() {
        return Ok(Err(invalid(&["mailboxIds"])));
    }
    if ids.len() > 1 {
        return Ok(Err(set_error("tooManyMailboxes", None, &["mailboxIds"])));
    }
    Ok(mailbox(ctx, ids[0])
        .await?
        .ok_or_else(|| invalid(&["mailboxIds"])))
}

fn received_at(v: Option<&Value>) -> Result<OffsetDateTime, Value> {
    match v {
        None | Some(Value::Null) => Ok(OffsetDateTime::now_utc()),
        Some(Value::String(s)) => {
            OffsetDateTime::parse(s, &Rfc3339).map_err(|_| invalid(&["receivedAt"]))
        }
        Some(_) => Err(invalid(&["receivedAt"])),
    }
}

/// Принимает письмо в папку учётной записи; копии внешним адресам от точек принятия решений — в очередь.
async fn store_message(
    ctx: &Ctx<'_>,
    raw: Vec<u8>,
    mailbox: MailboxId,
    flags: Flags,
    received_at: OffsetDateTime,
) -> MethodResult<Result<MessageId, Value>> {
    let d = &ctx.jmap.d;
    let data = Bytes::from(raw);
    let body = data.clone();
    let got = d
        .store
        .ingest(IngestRequest {
            tenant: ctx.scope.tenant,
            mail_from: ctx.principal.login.clone(),
            raw: Box::pin(futures::stream::once(async move { Ok(body) })),
            deliveries: vec![Delivery {
                account: ctx.scope.account,
                mailbox,
                extra_headers: Vec::new(),
                flags,
            }],
            received_at,
        })
        .await;
    match got {
        Ok(got) => {
            crate::submission::forward(ctx, &ctx.principal.login, &data, &got.forward).await?;
            match got.delivered.first() {
                Some(d) => Ok(Ok(d.message)),
                None => Err(MethodError::server_fail("no delivery")),
            }
        }
        Err(Error::QuotaExceeded) => Ok(Err(set_error("overQuota", None, &[]))),
        Err(Error::Invalid(m)) => Ok(Err(set_error("invalidEmail", Some(&m), &[]))),
        Err(e) => Err(e.into()),
    }
}

/// Объект созданного письма: `id`, `blobId`, `threadId`, `size`.
async fn summary(ctx: &Ctx<'_>, id: MessageId) -> MethodResult<Value> {
    let m = ctx
        .jmap
        .d
        .messages
        .get(ctx.scope, id)
        .await?
        .ok_or_else(|| MethodError::server_fail("created email not found"))?;
    Ok(json!({
        "id": m.id.to_string(),
        "blobId": blob_id(m.id, None),
        "threadId": thread_id(&m),
        "size": m.size,
    }))
}

// ---------- Email/import ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ImportArgs {
    account_id: String,
    if_in_state: Option<String>,
    emails: Map<String, Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ImportEmail {
    blob_id: String,
    mailbox_ids: Value,
    #[serde(default)]
    keywords: Option<Value>,
    #[serde(default)]
    received_at: Option<Value>,
}

async fn import_one(ctx: &Ctx<'_>, obj: Value) -> MethodResult<Result<MessageId, Value>> {
    let e: ImportEmail = match serde_json::from_value(obj) {
        Ok(e) => e,
        Err(err) => {
            return Ok(Err(set_error(
                "invalidProperties",
                Some(&err.to_string()),
                &[],
            )));
        }
    };
    let mailbox = match single_mailbox(ctx, &e.mailbox_ids).await? {
        Ok(m) => m,
        Err(err) => return Ok(Err(err)),
    };
    let flags = match e.keywords.as_ref().filter(|v| !v.is_null()) {
        Some(v) => match keywords(v) {
            Some(f) => f,
            None => return Ok(Err(invalid(&["keywords"]))),
        },
        None => Flags::new(),
    };
    let at = match received_at(e.received_at.as_ref()) {
        Ok(t) => t,
        Err(err) => return Ok(Err(err)),
    };
    let raw = match ctx
        .jmap
        .read_blob(ctx.scope, &e.blob_id, ctx.jmap.cfg.max_size_upload)
        .await
    {
        Ok(Some(raw)) => raw,
        Ok(None) => {
            let mut err = set_error("blobNotFound", None, &[]);
            err["notFound"] = json!([e.blob_id]);
            return Ok(Err(err));
        }
        Err(Error::Invalid(_)) => return Ok(Err(set_error("tooLarge", None, &[]))),
        Err(err) => return Err(err.into()),
    };
    store_message(ctx, raw, mailbox, flags, at).await
}

/// `Email/import`.
pub(crate) async fn import(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: ImportArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    if a.emails.len() > ctx.jmap.cfg.max_objects_in_set as usize {
        return Err(MethodError::new("requestTooLarge"));
    }
    let old = state_string(ctx.state().await?);
    if a.if_in_state.as_ref().is_some_and(|s| *s != old) {
        return Err(MethodError::new("stateMismatch"));
    }
    let (mut created, mut not_created) = (Map::new(), Map::new());
    for (cid, obj) in a.emails {
        match import_one(ctx, obj).await? {
            Ok(id) => {
                created.insert(cid.clone(), summary(ctx, id).await?);
                ctx.created.insert(cid, id.to_string());
            }
            Err(e) => {
                not_created.insert(cid, e);
            }
        }
    }
    let new = state_string(ctx.state().await?);
    Ok(object(json!({
        "accountId": a.account_id,
        "oldState": old,
        "newState": new,
        "created": or_null(created),
        "notCreated": or_null(not_created),
    })))
}

// ---------- Email/copy ----------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CopyArgs {
    from_account_id: String,
    account_id: String,
}

/// `Email/copy` (RFC 8621 §4.7): копирование между разными учётными записями (RFC 8620 §5.4),
/// а доступна только своя — исходной учётной записи нет.
pub(crate) async fn copy(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: CopyArgs = parse_args(args)?;
    if a.from_account_id == a.account_id {
        return Err(MethodError::invalid(
            "fromAccountId and accountId must differ",
        ));
    }
    if a.from_account_id != ctx.account_id() {
        return Err(MethodError::new("fromAccountNotFound"));
    }
    Err(MethodError::new("accountNotFound"))
}

// ---------- Email/set: создание черновика ----------

/// Поля, задаваемые свойствами объекта, а не `header:<имя>`.
const MANAGED_HEADERS: &[&str] = &[
    "from",
    "to",
    "cc",
    "bcc",
    "reply-to",
    "sender",
    "subject",
    "date",
    "message-id",
    "in-reply-to",
    "references",
    "mime-version",
    "content-type",
    "content-transfer-encoding",
    "content-disposition",
];

fn header_name_ok(n: &str) -> bool {
    !n.is_empty()
        && n.bytes().all(|b| b.is_ascii_graphic() && b != b':')
        && !MANAGED_HEADERS.contains(&n.to_ascii_lowercase().as_str())
}

/// Адреса `EmailAddress[]` → заголовок.
fn addresses(v: &Value) -> Option<Option<Address<'static>>> {
    match v {
        Value::Null => Some(None),
        Value::Array(list) => {
            let mut out = Vec::with_capacity(list.len());
            for a in list {
                let email = a.get("email")?.as_str()?;
                if email.bytes().any(|b| b.is_ascii_control()) {
                    return None;
                }
                let name = match a.get("name") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(n)) => Some(n.clone()),
                    Some(_) => return None,
                };
                out.push(Address::new_address(name, email.to_owned()));
            }
            Some((!out.is_empty()).then(|| Address::new_list(out)))
        }
        _ => None,
    }
}

/// Идентификаторы сообщений `String[]` без угловых скобок.
fn message_ids(v: &Value) -> Option<Option<MsgIdHeader<'static>>> {
    match v {
        Value::Null => Some(None),
        Value::Array(list) => {
            let ids: Option<Vec<String>> = list
                .iter()
                .map(|s| {
                    let s = s
                        .as_str()?
                        .trim()
                        .trim_start_matches('<')
                        .trim_end_matches('>');
                    (!s.is_empty()
                        && !s
                            .bytes()
                            .any(|b| b.is_ascii_whitespace() || b == b'<' || b == b'>'))
                    .then(|| s.to_owned())
                })
                .collect();
            let ids = ids?;
            Some((!ids.is_empty()).then(|| MsgIdHeader::new_list(ids.into_iter())))
        }
        _ => None,
    }
}

/// Часть тела `textBody`/`htmlBody`: текст из `bodyValues` по `partId`.
fn body_text(parts: &Value, values: &Map<String, Value>) -> Option<Option<String>> {
    let list = parts.as_array()?;
    match list.as_slice() {
        [] => Some(None),
        [p] => {
            let id = p.get("partId")?.as_str()?;
            let text = values.get(id)?.get("value")?.as_str()?;
            Some(Some(text.to_owned()))
        }
        _ => None,
    }
}

/// Вложение: blob, тип, имя, `inline` с `cid`.
struct Attachment {
    blob: String,
    kind: String,
    name: Option<String>,
    cid: Option<String>,
    inline: bool,
}

fn attachment(v: &Value) -> Option<Attachment> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    Some(Attachment {
        blob: s("blobId")?,
        kind: s("type").unwrap_or_else(|| "application/octet-stream".into()),
        name: s("name"),
        cid: s("cid").map(|c| c.trim_start_matches('<').trim_end_matches('>').to_owned()),
        inline: v.get("disposition").and_then(Value::as_str) == Some("inline"),
    })
}

/// Разобранный черновик.
struct Draft {
    mailbox: MailboxId,
    flags: Flags,
    received_at: OffsetDateTime,
    builder: MessageBuilder<'static>,
    attachments: Vec<Attachment>,
}

async fn parse_draft(ctx: &Ctx<'_>, obj: &Value) -> MethodResult<Result<Draft, Value>> {
    let Value::Object(m) = obj else {
        return Ok(Err(invalid(&[])));
    };
    let Some(mb) = m.get("mailboxIds") else {
        return Ok(Err(invalid(&["mailboxIds"])));
    };
    let mailbox = match single_mailbox(ctx, mb).await? {
        Ok(id) => id,
        Err(e) => return Ok(Err(e)),
    };
    let empty = Map::new();
    let values = match m.get("bodyValues") {
        None | Some(Value::Null) => &empty,
        Some(Value::Object(v)) => v,
        Some(_) => return Ok(Err(invalid(&["bodyValues"]))),
    };
    let mut flags = Flags::new();
    let mut at = OffsetDateTime::now_utc();
    let mut b = MessageBuilder::new();
    let mut has_message_id = false;
    let (mut text, mut html, mut attachments) = (None, None, Vec::new());
    for (k, v) in m {
        let ok = match k.as_str() {
            "mailboxIds" | "bodyValues" => true,
            "keywords" => keywords(v).map(|f| flags = f).is_some(),
            "receivedAt" => match received_at(Some(v)) {
                Ok(t) => {
                    at = t;
                    true
                }
                Err(_) => false,
            },
            "from" | "to" | "cc" | "bcc" | "replyTo" | "sender" => match addresses(v) {
                Some(Some(a)) => {
                    let name = match k.as_str() {
                        "from" => "From",
                        "to" => "To",
                        "cc" => "Cc",
                        "bcc" => "Bcc",
                        "replyTo" => "Reply-To",
                        _ => "Sender",
                    };
                    b = b.header(name, HeaderType::from(a));
                    true
                }
                Some(None) => true,
                None => false,
            },
            "subject" => match v {
                Value::Null => true,
                Value::String(s) if !s.contains(['\r', '\n']) => {
                    b = b.header("Subject", HeaderType::from(Text::new(s.clone())));
                    true
                }
                _ => false,
            },
            "sentAt" => match v {
                Value::Null => true,
                Value::String(s) => match OffsetDateTime::parse(s, &Rfc3339) {
                    Ok(t) => {
                        b = b.header("Date", HeaderType::from(Date::new(t.unix_timestamp())));
                        true
                    }
                    Err(_) => false,
                },
                _ => false,
            },
            "messageId" | "inReplyTo" | "references" => match message_ids(v) {
                Some(Some(ids)) => {
                    let name = match k.as_str() {
                        "messageId" => {
                            has_message_id = true;
                            "Message-ID"
                        }
                        "inReplyTo" => "In-Reply-To",
                        _ => "References",
                    };
                    b = b.header(name, HeaderType::from(ids));
                    true
                }
                Some(None) => true,
                None => false,
            },
            "textBody" => body_text(v, values).map(|t| text = t).is_some(),
            "htmlBody" => body_text(v, values).map(|t| html = t).is_some(),
            "attachments" => match v.as_array() {
                Some(list) => match list.iter().map(attachment).collect::<Option<Vec<_>>>() {
                    Some(list) => {
                        attachments = list;
                        true
                    }
                    None => false,
                },
                None => v.is_null(),
            },
            h if h.starts_with("header:") => {
                let rest = &h["header:".len()..];
                let (name, form) = match rest.rsplit_once(':') {
                    Some((n, f @ ("asRaw" | "asText"))) => (n, f),
                    _ => (rest, "asRaw"),
                };
                match v {
                    Value::String(s) if header_name_ok(name) && !s.contains(['\r', '\n']) => {
                        let value = if form == "asText" {
                            HeaderType::from(Text::new(s.clone()))
                        } else {
                            HeaderType::from(Raw::new(s.trim_start().to_owned()))
                        };
                        b = b.header(name.to_owned(), value);
                        true
                    }
                    _ => false,
                }
            }
            _ => false,
        };
        if !ok {
            return Ok(Err(invalid(&[k])));
        }
    }
    if !has_message_id {
        let domain = ctx
            .principal
            .login
            .rsplit_once('@')
            .map_or("localhost", |(_, d)| d);
        b = b.header(
            "Message-ID",
            HeaderType::from(MsgIdHeader::new(format!("{}@{domain}", MessageId::new()))),
        );
    }
    if let Some(t) = text {
        b = b.text_body(t);
    }
    if let Some(h) = html {
        b = b.html_body(h);
    }
    Ok(Ok(Draft {
        mailbox,
        flags,
        received_at: at,
        builder: b,
        attachments,
    }))
}

async fn create_one(ctx: &Ctx<'_>, obj: &Value) -> MethodResult<Result<MessageId, Value>> {
    let draft = match parse_draft(ctx, obj).await? {
        Ok(d) => d,
        Err(e) => return Ok(Err(e)),
    };
    let mut b = draft.builder;
    let mut budget = ctx.jmap.cfg.max_size_upload;
    for a in draft.attachments {
        let data = match ctx.jmap.read_blob(ctx.scope, &a.blob, budget).await {
            Ok(Some(d)) => d,
            Ok(None) => {
                let mut err = set_error("blobNotFound", None, &[]);
                err["notFound"] = json!([a.blob]);
                return Ok(Err(err));
            }
            Err(Error::Invalid(_)) => {
                return Ok(Err(set_error("tooLarge", None, &["attachments"])));
            }
            Err(e) => return Err(e.into()),
        };
        budget = budget.saturating_sub(data.len() as u64);
        b = match (a.inline, a.cid) {
            (true, Some(cid)) => b.inline(a.kind, cid, data),
            _ => b.attachment(a.kind, a.name.unwrap_or_else(|| "attachment".into()), data),
        };
    }
    let raw = b
        .write_to_vec()
        .map_err(|e| MethodError::server_fail(e.to_string()))?;
    store_message(ctx, raw, draft.mailbox, draft.flags, draft.received_at).await
}

// ---------- Email/set: изменение и удаление ----------

/// Снимает экранирование JSON Pointer (`~1` → `/`, `~0` → `~`).
fn unescape(s: &str) -> String {
    s.replace("~1", "/").replace("~0", "~")
}

/// Изменение одного письма; `Ok(Ok(Some(id)))` — письмо перенесено и получило новый `id`.
async fn update_one(
    ctx: &Ctx<'_>,
    id: &str,
    patch: &Value,
) -> MethodResult<Result<Option<MessageId>, Value>> {
    let d = &ctx.jmap.d;
    let msg = match MessageId::from_str(ctx.resolve_id(id)) {
        Ok(mid) => d.messages.get(ctx.scope, mid).await?,
        Err(_) => None,
    };
    let Some(msg) = msg else {
        return Ok(Err(set_error("notFound", None, &[])));
    };
    let Value::Object(patch) = patch else {
        return Ok(Err(invalid(&[])));
    };
    let mut replace: Option<Flags> = None;
    let (mut add, mut remove) = (Flags::new(), Flags::new());
    let mut boxes: Option<BTreeSet<String>> = None;
    let mut box_add = BTreeSet::new();
    let mut box_remove = BTreeSet::new();
    for (k, v) in patch {
        if k == "keywords" {
            match keywords(v) {
                Some(f) => replace = Some(f),
                None => return Ok(Err(invalid(&["keywords"]))),
            }
        } else if let Some(kw) = k.strip_prefix("keywords/") {
            let Some(f) = flag_of(&unescape(kw)) else {
                return Ok(Err(invalid(&[k])));
            };
            match v {
                Value::Bool(true) => add.insert(f),
                Value::Null => remove.insert(f),
                _ => return Ok(Err(invalid(&[k]))),
            };
        } else if k == "mailboxIds" {
            let Value::Object(m) = v else {
                return Ok(Err(invalid(&["mailboxIds"])));
            };
            if m.values().any(|v| *v != Value::Bool(true)) {
                return Ok(Err(invalid(&["mailboxIds"])));
            }
            boxes = Some(m.keys().map(|k| ctx.resolve_id(k).to_owned()).collect());
        } else if let Some(mb) = k.strip_prefix("mailboxIds/") {
            let mb = ctx.resolve_id(&unescape(mb)).to_owned();
            match v {
                Value::Bool(true) => box_add.insert(mb),
                Value::Null => box_remove.insert(mb),
                _ => return Ok(Err(invalid(&[k]))),
            };
        } else {
            return Ok(Err(invalid(&[k])));
        }
    }
    if replace.is_some() && !(add.is_empty() && remove.is_empty()) {
        return Ok(Err(set_error("invalidPatch", None, &["keywords"])));
    }
    if boxes.is_some() && !(box_add.is_empty() && box_remove.is_empty()) {
        return Ok(Err(set_error("invalidPatch", None, &["mailboxIds"])));
    }
    let current = msg.mailbox.to_string();
    let mut target = boxes.unwrap_or_else(|| BTreeSet::from([current.clone()]));
    target.retain(|b| !box_remove.contains(b));
    target.extend(box_add);
    let to = match target.len() {
        0 => return Ok(Err(invalid(&["mailboxIds"]))),
        1 => target.pop_first().unwrap_or_default(),
        _ => return Ok(Err(set_error("tooManyMailboxes", None, &["mailboxIds"]))),
    };
    let to = if to == current {
        None
    } else {
        match mailbox(ctx, &to).await? {
            Some(id) => Some(id),
            None => return Ok(Err(invalid(&["mailboxIds"]))),
        }
    };

    let ids = [msg.id];
    if let Some(mut flags) = replace {
        // `\Deleted` в JMAP не виден и сохраняется.
        if msg.flags.contains(&Flag::Deleted) {
            flags.insert(Flag::Deleted);
        }
        d.store
            .set_flags(ctx.scope, &ids, FlagOp::Replace(flags), None)
            .await?;
    }
    if !add.is_empty() {
        d.store
            .set_flags(ctx.scope, &ids, FlagOp::Add(add), None)
            .await?;
    }
    if !remove.is_empty() {
        d.store
            .set_flags(ctx.scope, &ids, FlagOp::Remove(remove), None)
            .await?;
    }
    match to {
        None => Ok(Ok(None)),
        Some(to) => {
            let moved = d.store.move_to(ctx.scope, &ids, to).await?;
            Ok(Ok(moved.first().map(|i| i.id)))
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetArgs {
    account_id: String,
    if_in_state: Option<String>,
    create: Option<Map<String, Value>>,
    update: Option<Map<String, Value>>,
    destroy: Option<Vec<String>>,
}

/// `Email/set`: создание черновиков, изменение `keywords` и `mailboxIds`, удаление в корзину.
pub(crate) async fn set(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: SetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let n = a.create.as_ref().map_or(0, Map::len)
        + a.update.as_ref().map_or(0, Map::len)
        + a.destroy.as_ref().map_or(0, Vec::len);
    if n > ctx.jmap.cfg.max_objects_in_set as usize {
        return Err(MethodError::new("requestTooLarge"));
    }
    let old = state_string(ctx.state().await?);
    if a.if_in_state.as_ref().is_some_and(|s| *s != old) {
        return Err(MethodError::new("stateMismatch"));
    }

    let (mut created, mut not_created) = (Map::new(), Map::new());
    for (cid, obj) in a.create.unwrap_or_default() {
        match create_one(ctx, &obj).await? {
            Ok(id) => {
                created.insert(cid.clone(), summary(ctx, id).await?);
                ctx.created.insert(cid, id.to_string());
            }
            Err(e) => {
                not_created.insert(cid, e);
            }
        }
    }

    let (mut updated, mut not_updated) = (Map::new(), Map::new());
    for (id, patch) in a.update.unwrap_or_default() {
        match update_one(ctx, &id, &patch).await? {
            Ok(None) => {
                updated.insert(id, Value::Null);
            }
            Ok(Some(new)) => {
                updated.insert(id, json!({ "id": new.to_string() }));
            }
            Err(e) => {
                not_updated.insert(id, e);
            }
        }
    }

    let d = &ctx.jmap.d;
    let trash = d
        .mailboxes
        .list(ctx.scope)
        .await?
        .into_iter()
        .find(|m| m.role == Some(SpecialUse::Trash))
        .map(|m| m.id);
    let (mut destroyed, mut not_destroyed) = (Vec::new(), Map::new());
    let mut to_trash = Vec::new();
    for id in a.destroy.unwrap_or_default() {
        let msg = match MessageId::from_str(ctx.resolve_id(&id)) {
            Ok(mid) => d.messages.get(ctx.scope, mid).await?,
            Err(_) => None,
        };
        match msg {
            None => {
                not_destroyed.insert(id, set_error("notFound", None, &[]));
            }
            Some(m) if Some(m.mailbox) == trash => {
                not_destroyed.insert(
                    id,
                    set_error(
                        "forbidden",
                        Some("email is in Trash: it is deleted when the retention period expires"),
                        &[],
                    ),
                );
            }
            Some(m) => {
                to_trash.push(m.id);
                destroyed.push(id);
            }
        }
    }
    if !to_trash.is_empty() {
        d.store.delete_to_trash(ctx.scope, &to_trash).await?;
    }

    let new = state_string(ctx.state().await?);
    Ok(object(json!({
        "accountId": a.account_id,
        "oldState": old,
        "newState": new,
        "created": or_null(created),
        "updated": or_null(updated),
        "destroyed": if destroyed.is_empty() { Value::Null } else { json!(destroyed) },
        "notCreated": or_null(not_created),
        "notUpdated": or_null(not_updated),
        "notDestroyed": or_null(not_destroyed),
    })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testenv::{Env, mail};
    use axum::body::Bytes;

    async fn get(env: &Env, id: &str) -> Value {
        let args = json!({
            "accountId": env.account(),
            "ids": [id],
            "properties": ["mailboxIds", "keywords", "subject", "from", "to", "hasAttachment",
                           "textBody", "attachments", "bodyValues"],
            "fetchTextBodyValues": true,
        });
        env.call("Email/get", args, "Email/get").await["list"][0].clone()
    }

    /// keywords (патч и целиком), перенос в другую папку — новый id в `updated`, ошибки mailboxIds.
    #[tokio::test]
    async fn update_keywords_and_mailbox() {
        let env = Env::new().await;
        let inbox = env.mailbox(SpecialUse::Inbox);
        let sent = env.mailbox(SpecialUse::Sent);
        let id = env
            .deliver(inbox, &mail("Hi", "a@x.test", "", "body"), Flags::new())
            .await
            .to_string();
        let acc = env.account();
        let r = env
            .call(
                "Email/set",
                json!({"accountId": acc, "update": {&id: {"keywords/$seen": true, "keywords/Work": true}}}),
                "Email/set",
            )
            .await;
        assert_eq!(r["updated"], json!({&id: null}), "{r}");
        assert_eq!(
            get(&env, &id).await["keywords"],
            json!({"$seen": true, "work": true})
        );
        env.call(
            "Email/set",
            json!({"accountId": acc, "update": {&id: {"keywords": {"$draft": true}}}}),
            "Email/set",
        )
        .await;
        assert_eq!(get(&env, &id).await["keywords"], json!({"$draft": true}));

        let r = env
            .call(
                "Email/set",
                json!({"accountId": acc, "update": {&id: {"mailboxIds/x": true}}}),
                "Email/set",
            )
            .await;
        assert_eq!(r["notUpdated"][&id]["type"], "tooManyMailboxes");
        let r = env
            .call(
                "Email/set",
                json!({"accountId": acc, "update": {&id: {"keywords": {"$seen": true}, "keywords/$draft": true}}}),
                "Email/set",
            )
            .await;
        assert_eq!(r["notUpdated"][&id]["type"], "invalidPatch");

        let r = env
            .call(
                "Email/set",
                json!({"accountId": acc, "update": {&id: {
                    format!("mailboxIds/{inbox}"): null, format!("mailboxIds/{sent}"): true
                }}}),
                "Email/set",
            )
            .await;
        let new = r["updated"][&id]["id"].as_str().expect("new id").to_owned();
        assert_ne!(new, id);
        assert_eq!(
            get(&env, &new).await["mailboxIds"],
            json!({sent.to_string(): true})
        );
        assert_ne!(r["oldState"], r["newState"]);
    }

    /// Удаление — в корзину; письмо в корзине не удаляется досрочно.
    #[tokio::test]
    async fn destroy_to_trash() {
        let env = Env::new().await;
        let inbox = env.mailbox(SpecialUse::Inbox);
        let id = env
            .deliver(inbox, &mail("Hi", "a@x.test", "", "body"), Flags::new())
            .await
            .to_string();
        let acc = env.account();
        let r = env
            .call(
                "Email/set",
                json!({"accountId": acc, "destroy": [&id, "nope"]}),
                "Email/set",
            )
            .await;
        assert_eq!(r["destroyed"], json!([&id]));
        assert_eq!(r["notDestroyed"]["nope"]["type"], "notFound");
        let trash = env.mailbox(SpecialUse::Trash).to_string();
        assert_eq!(get(&env, &id).await["mailboxIds"], json!({&trash: true}));
        let r = env
            .call(
                "Email/set",
                json!({"accountId": acc, "destroy": [&id]}),
                "Email/set",
            )
            .await;
        assert_eq!(r["notDestroyed"][&id]["type"], "forbidden", "{r}");
        let r = env
            .call(
                "Email/set",
                json!({"accountId": acc, "ifInState": "x", "destroy": [&id]}),
                "error",
            )
            .await;
        assert_eq!(r["type"], "stateMismatch");
    }

    /// Черновик: адреса, тема, текст и HTML из bodyValues, вложение из загруженного blob, ссылка `#cid` на папку.
    #[tokio::test]
    async fn create_draft() {
        let env = Env::new().await;
        let acc = env.account();
        let key = env
            .jmap
            .put_upload(env.scope.tenant, Bytes::from_static(b"%PDF-1.4 test"))
            .await
            .expect("upload");
        let blob = crate::blob::upload_id(&key);
        let r = env
            .calls(json!([
                ["Mailbox/set", {"accountId": acc, "create": {"mb": {"name": "Черновики 2"}}}, "m"],
                ["Email/set", {"accountId": acc, "create": {"d1": {
                    "mailboxIds": {"#mb": true},
                    "keywords": {"$draft": true, "$seen": true},
                    "from": [{"name": "Пользователь", "email": "user@example.org"}],
                    "to": [{"email": "rcpt@external.test"}],
                    "subject": "Отчёт",
                    "header:X-Mailer": "test",
                    "textBody": [{"partId": "t", "type": "text/plain"}],
                    "htmlBody": [{"partId": "h", "type": "text/html"}],
                    "bodyValues": {"t": {"value": "Привет"}, "h": {"value": "<p>Привет</p>"}},
                    "attachments": [{"blobId": blob, "type": "application/pdf", "name": "report.pdf"}]
                }, "bad": {"mailboxIds": {"#mb": true}, "bodyStructure": {}}}}, "e"]
            ]))
            .await;
        let set = &r[1][1];
        let id = set["created"]["d1"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("not created: {set}"))
            .to_owned();
        assert!(set["created"]["d1"]["threadId"].is_string());
        assert_eq!(
            set["notCreated"]["bad"]["properties"],
            json!(["bodyStructure"])
        );
        let e = get(&env, &id).await;
        assert_eq!(e["subject"], "Отчёт");
        assert_eq!(
            e["from"],
            json!([{"name": "Пользователь", "email": "user@example.org"}])
        );
        assert_eq!(e["keywords"], json!({"$draft": true, "$seen": true}));
        assert_eq!(e["hasAttachment"], true);
        assert_eq!(e["attachments"][0]["name"], "report.pdf");
        let part = e["textBody"][0]["partId"].as_str().expect("part");
        assert_eq!(e["bodyValues"][part]["value"], "Привет");
        let mid: MessageId = id.parse().expect("id");
        let raw = env
            .jmap
            .read_blob(env.scope, &blob_id(mid, None), u64::MAX)
            .await
            .expect("raw")
            .expect("found");
        let text = String::from_utf8_lossy(&raw);
        assert!(text.contains("X-Mailer: test\r\n"), "{text}");
        assert!(text.contains("@example.org>"), "{text}");

        let r = env
            .call(
                "Email/set",
                json!({"accountId": acc, "create": {"x": {
                    "mailboxIds": {env.mailbox(SpecialUse::Drafts).to_string(): true},
                    "attachments": [{"blobId": "Unope"}]
                }}}),
                "Email/set",
            )
            .await;
        assert_eq!(r["notCreated"]["x"]["type"], "blobNotFound");
    }

    /// Email/import из загруженного blob; Email/copy — только между разными учётными записями.
    #[tokio::test]
    async fn import_and_copy() {
        let env = Env::new().await;
        let acc = env.account();
        let inbox = env.mailbox(SpecialUse::Inbox).to_string();
        let raw = mail("Imported", "a@x.test", "", "text");
        let key = env
            .jmap
            .put_upload(env.scope.tenant, Bytes::from(raw.clone()))
            .await
            .expect("upload");
        let r = env
            .call(
                "Email/import",
                json!({"accountId": acc, "emails": {
                    "i1": {"blobId": crate::blob::upload_id(&key), "mailboxIds": {&inbox: true},
                           "keywords": {"$seen": true}, "receivedAt": "2026-10-30T07:00:00Z"},
                    "i2": {"blobId": "Mnope", "mailboxIds": {&inbox: true}}
                }}),
                "Email/import",
            )
            .await;
        let id = r["created"]["i1"]["id"]
            .as_str()
            .unwrap_or_else(|| panic!("not imported: {r}"))
            .to_owned();
        assert_eq!(r["created"]["i1"]["size"], raw.len());
        assert_eq!(r["notCreated"]["i2"]["type"], "blobNotFound");
        let e = get(&env, &id).await;
        assert_eq!(e["subject"], "Imported");
        assert_eq!(e["keywords"], json!({"$seen": true}));

        let r = env
            .call(
                "Email/copy",
                json!({"fromAccountId": acc, "accountId": acc, "create": {}}),
                "error",
            )
            .await;
        assert_eq!(r["type"], "invalidArguments");
        let r = env
            .call(
                "Email/copy",
                json!({"fromAccountId": "other", "accountId": acc, "create": {}}),
                "error",
            )
            .await;
        assert_eq!(r["type"], "fromAccountNotFound");
    }

    #[test]
    fn keyword_flags() {
        assert_eq!(flag_of("$Seen"), Some(Flag::Seen));
        assert_eq!(flag_of("Work"), Some(Flag::Keyword("work".into())));
        assert_eq!(flag_of("a b"), None);
        assert_eq!(flag_of("a]"), None);
        assert_eq!(flag_of(""), None);
    }
}
