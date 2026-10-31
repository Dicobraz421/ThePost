//! `FETCH` и `UID FETCH` (RFC 9051, 6.4.5; RFC 3516 BINARY).
//!
//! Метаданные (`FLAGS`, `UID`, `INTERNALDATE`, `RFC822.SIZE`, `ENVELOPE`, `BODYSTRUCTURE`, `MODSEQ`) — из
//! хранилища метаданных, вычисленные при приёме (MS-07). Содержимое — из почтового хранилища:
//! `BODY[]` с диапазоном — `fetch_raw` с диапазоном, тело листовой части — `fetch_part`, заголовки —
//! начало письма; прочие разделы (`HEADER`, `TEXT`, `MIME`, `HEADER.FIELDS`, разделы вложенного
//! `message/rfc822`) — разбор письма `mail-parser` с путями частей, как у MS-07. `BINARY` снимает
//! base64 и quoted-printable, иная кодировка — `NO [UNKNOWN-CTE]`. Выборка без `.PEEK` в папке,
//! открытой на запись, ставит `\Seen` и возвращает `FLAGS`.
//!
//! Тексты с не-ASCII в `ENVELOPE` и `BODYSTRUCTURE` для клиентов IMAP4rev1 — encoded-word (RFC 2047),
//! для IMAP4rev2 — UTF-8.

use std::collections::HashMap;
use std::num::{NonZeroU32, NonZeroU64};
use std::ops::Range;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use futures::StreamExt;
use mail_parser::{MessageParser, MimeHeaders, PartType};
use tp_contracts::blob::ByteStream;
use tp_contracts::meta::{Address, BodyPart, Envelope, Flag, FlagOp, Message, MessageQuery};
use tp_contracts::{Error, MessageId, Result};

use crate::proto::types::body::{
    BasicFields, Body, BodyStructure, Disposition, Language, Location, MultiPartExtensionData,
    SinglePartExtensionData, SpecificFields,
};
use crate::proto::types::core::{
    Atom, IString, Literal, LiteralMode, NString, NString8, Tag, Vec1,
};
use crate::proto::types::datetime::DateTime;
use crate::proto::types::envelope::{Address as ImapAddress, Envelope as ImapEnvelope};
use crate::proto::types::extensions::binary::Literal8;
use crate::proto::types::fetch::{
    Macro, MacroOrMessageDataItemNames, MessageDataItem, MessageDataItemName, Part, Section,
};
use crate::proto::types::flag::{Flag as ImapFlag, FlagFetch};
use crate::proto::types::response::{Code, Data};
use crate::proto::types::sequence::{Sequence, SequenceSet};
use crate::proto::{self};
use crate::session::{Session, Transport};

/// Начало письма, в котором ищется конец заголовка, байт.
const HEAD_PROBE: u64 = 64 * 1024;

/// Ошибка выборки одного письма, прерывающая команду.
enum Fail {
    /// `NO [UNKNOWN-CTE]`.
    UnknownCte,
    /// Ошибка хранилища.
    Store(Error),
}

impl From<Error> for Fail {
    fn from(e: Error) -> Self {
        Fail::Store(e)
    }
}

/// Элементы `FETCH` после раскрытия макросов.
fn expand(
    items: MacroOrMessageDataItemNames<'static>,
    uid: bool,
) -> Vec<MessageDataItemName<'static>> {
    let mut items = match items {
        MacroOrMessageDataItemNames::Macro(m) => {
            let mut v = vec![
                MessageDataItemName::Flags,
                MessageDataItemName::InternalDate,
                MessageDataItemName::Rfc822Size,
            ];
            if matches!(m, Macro::All | Macro::Full) {
                v.push(MessageDataItemName::Envelope);
            }
            if matches!(m, Macro::Full) {
                v.push(MessageDataItemName::Body);
            }
            v
        }
        MacroOrMessageDataItemNames::MessageDataItemNames(v) => v,
    };
    // UID FETCH всегда возвращает UID (RFC 9051, 6.4.9).
    if uid && !items.contains(&MessageDataItemName::Uid) {
        items.insert(0, MessageDataItemName::Uid);
    }
    items
}

/// Элемент меняет `\Seen` (выборка содержимого без `.PEEK`).
fn sets_seen(item: &MessageDataItemName<'_>) -> bool {
    match item {
        MessageDataItemName::BodyExt { peek, .. } | MessageDataItemName::Binary { peek, .. } => {
            !peek
        }
        MessageDataItemName::Rfc822 | MessageDataItemName::Rfc822Text => true,
        _ => false,
    }
}

// ---------- Строки ----------

/// Строка IMAP из байтов: литерал (NUL заменяется пробелом — в `BODY` он недопустим).
fn nstring_bytes(mut data: Vec<u8>) -> NString<'static> {
    if data.is_empty() {
        return NString::try_from("").unwrap_or(NString(None));
    }
    for b in &mut data {
        if *b == 0 {
            *b = b' ';
        }
    }
    match Literal::try_from(data) {
        Ok(l) => NString(Some(IString::Literal(l))),
        Err(_) => NString(None),
    }
}

/// Текст заголовка: ASCII — как есть; иначе UTF-8 (IMAP4rev2) или encoded-word (RFC 2047).
fn text(s: &str, rev2: bool) -> Vec<u8> {
    if s.is_ascii() || rev2 {
        s.as_bytes().to_vec()
    } else {
        format!("=?utf-8?B?{}?=", STANDARD.encode(s)).into_bytes()
    }
}

fn nstring_text(s: Option<&str>, rev2: bool) -> NString<'static> {
    match s {
        None => NString(None),
        Some(s) => {
            let bytes = text(s, rev2);
            match NString::try_from(bytes.clone()) {
                Ok(n) => n,
                Err(_) => nstring_bytes(bytes),
            }
        }
    }
}

fn istring(s: &str, rev2: bool) -> IString<'static> {
    let bytes = text(s, rev2);
    IString::try_from(bytes.clone())
        .or_else(|_| Literal::try_from(bytes).map(IString::Literal))
        .unwrap_or_else(|_| IString::Literal(Literal::unvalidated(Vec::<u8>::new())))
}

// ---------- ENVELOPE, BODYSTRUCTURE ----------

fn address(a: &Address, rev2: bool) -> ImapAddress<'static> {
    let (local, host) = match a.email.rsplit_once('@') {
        Some((l, h)) => (l, Some(h)),
        None => (a.email.as_str(), None),
    };
    ImapAddress {
        name: nstring_text(a.name.as_deref().filter(|n| !n.is_empty()), rev2),
        adl: NString(None),
        mailbox: nstring_text(Some(local), rev2),
        host: nstring_text(host, rev2),
    }
}

fn rfc2822(d: time::OffsetDateTime) -> Option<String> {
    let offset = chrono::FixedOffset::east_opt(d.offset().whole_seconds())?;
    let dt = chrono::DateTime::from_timestamp(d.unix_timestamp(), 0)?.with_timezone(&offset);
    Some(dt.to_rfc2822())
}

fn envelope(e: &Envelope, rev2: bool) -> ImapEnvelope<'static> {
    let list = |v: &[Address]| v.iter().map(|a| address(a, rev2)).collect::<Vec<_>>();
    let from = list(&e.from);
    // RFC 9051, 7.5.2: пустые Sender и Reply-To — как From.
    let or_from = |v: &[Address]| if v.is_empty() { from.clone() } else { list(v) };
    ImapEnvelope {
        date: nstring_text(e.date.and_then(rfc2822).as_deref(), rev2),
        subject: nstring_text(e.subject.as_deref(), rev2),
        sender: or_from(&e.sender),
        reply_to: or_from(&e.reply_to),
        from,
        to: list(&e.to),
        cc: list(&e.cc),
        bcc: list(&e.bcc),
        in_reply_to: nstring_text(e.in_reply_to.as_deref(), rev2),
        message_id: nstring_text(e.message_id.as_deref(), rev2),
    }
}

fn params(p: &[(String, String)], rev2: bool) -> Vec<(IString<'static>, IString<'static>)> {
    p.iter()
        .map(|(k, v)| (istring(k, rev2), istring(v, rev2)))
        .collect()
}

fn disposition(p: &BodyPart, rev2: bool) -> Disposition<'static> {
    Disposition {
        disposition: p
            .disposition
            .as_ref()
            .map(|(kind, ps)| (istring(kind, rev2), params(ps, rev2))),
        tail: Some(Language {
            language: p.language.iter().map(|l| istring(l, rev2)).collect(),
            tail: Some(Location {
                location: nstring_text(p.location.as_deref(), rev2),
                extensions: Vec::new(),
            }),
        }),
    }
}

/// `BODYSTRUCTURE` (`ext`) или `BODY`; `nested` — конверт вложенного `message/rfc822` по пути части.
fn body_structure(
    p: &BodyPart,
    ext: bool,
    rev2: bool,
    nested: &HashMap<String, Envelope>,
) -> BodyStructure<'static> {
    if p.media_type == "multipart" {
        let bodies: Vec<_> = p
            .children
            .iter()
            .map(|c| body_structure(c, ext, rev2, nested))
            .collect();
        if let Ok(bodies) = Vec1::try_from(bodies) {
            return BodyStructure::Multi {
                bodies,
                subtype: istring(&p.subtype, rev2),
                extension_data: ext.then(|| MultiPartExtensionData {
                    parameter_list: params(&p.params, rev2),
                    tail: Some(disposition(p, rev2)),
                }),
            };
        }
    }
    let basic = BasicFields {
        parameter_list: params(&p.params, rev2),
        id: nstring_text(p.content_id.as_deref(), rev2),
        description: nstring_text(p.description.as_deref(), rev2),
        content_transfer_encoding: istring(p.encoding.as_deref().unwrap_or("7bit"), rev2),
        size: u32::try_from(p.size).unwrap_or(u32::MAX),
    };
    let lines = p.lines.unwrap_or(0);
    let specific = match (
        p.media_type.as_str(),
        p.subtype.as_str(),
        p.children.first(),
    ) {
        // Составная без частей (битый MIME, нет boundary) — как текст.
        ("multipart", _, _) => SpecificFields::Text {
            subtype: istring("plain", rev2),
            number_of_lines: lines,
        },
        ("text", subtype, _) => SpecificFields::Text {
            subtype: istring(subtype, rev2),
            number_of_lines: lines,
        },
        ("message", "rfc822", Some(child)) => SpecificFields::Message {
            envelope: Box::new(envelope(
                nested.get(&p.part).unwrap_or(&Envelope::default()),
                rev2,
            )),
            body_structure: Box::new(body_structure(child, ext, rev2, nested)),
            number_of_lines: lines,
        },
        (media_type, subtype, _) => SpecificFields::Basic {
            r#type: istring(media_type, rev2),
            subtype: istring(subtype, rev2),
        },
    };
    BodyStructure::Single {
        body: Body { basic, specific },
        extension_data: ext.then(|| SinglePartExtensionData {
            md5: NString(None),
            tail: Some(disposition(p, rev2)),
        }),
    }
}

/// Пути вложенных `message/rfc822`.
fn rfc822_parts(p: &BodyPart, out: &mut Vec<String>) {
    if p.media_type == "message" && p.subtype == "rfc822" {
        out.push(p.part.clone());
    }
    for c in &p.children {
        rfc822_parts(c, out);
    }
}

/// Часть по пути в структуре метаданных.
fn find_part<'a>(p: &'a BodyPart, path: &str) -> Option<&'a BodyPart> {
    if p.part == path {
        return Some(p);
    }
    p.children.iter().find_map(|c| find_part(c, path))
}

fn part_path(nums: &[NonZeroU32]) -> String {
    nums.iter()
        .map(|n| n.to_string())
        .collect::<Vec<_>>()
        .join(".")
}

// ---------- Разделы письма ----------

/// Сущность MIME в разобранном письме с байтами, к которым относятся её смещения.
///
/// У вложенного `message/rfc822` без кодировки передачи `mail-parser` даёт смещения частей относительно
/// верхнего письма, у закодированного (base64) — относительно раскодированных байтов вложения.
#[derive(Clone, Copy)]
struct Ent<'m, 'x> {
    m: &'m mail_parser::Message<'x>,
    /// Часть; `None` — само письмо.
    part: Option<usize>,
    raw: &'m [u8],
    /// Письмо верхнего уровня (тело — до конца байтов).
    top: bool,
}

impl<'m, 'x> Ent<'m, 'x> {
    fn top(m: &'m mail_parser::Message<'x>) -> Self {
        Ent {
            m,
            part: None,
            raw: m.raw_message(),
            top: true,
        }
    }

    fn at(self, part: usize) -> Self {
        Ent {
            part: Some(part),
            ..self
        }
    }

    fn get(&self) -> Option<&'m mail_parser::MessagePart<'x>> {
        self.m.parts.get(self.part?)
    }

    /// Вложенное письмо части `message/rfc822`.
    fn nested(self) -> Option<Self> {
        let PartType::Message(nested) = &self.get()?.body else {
            return None;
        };
        let root = nested.parts.first()?;
        let absolute = root.offset_end as usize > nested.raw_message().len();
        Some(Ent {
            m: nested,
            part: None,
            raw: if absolute {
                self.raw
            } else {
                nested.raw_message()
            },
            top: false,
        })
    }

    fn bytes(&self, r: Range<u32>) -> Vec<u8> {
        let end = (r.end as usize).min(self.raw.len());
        let start = (r.start as usize).min(end);
        self.raw[start..end].to_vec()
    }

    /// Заголовок и тело письма (`part = None`).
    fn header_and_text(&self) -> (Vec<u8>, Vec<u8>) {
        match self.m.parts.first() {
            Some(root) => {
                let end = if self.top {
                    u32::try_from(self.raw.len()).unwrap_or(u32::MAX)
                } else {
                    root.offset_end
                };
                (
                    self.bytes(root.offset_header..root.offset_body),
                    self.bytes(root.offset_body..end),
                )
            }
            None => (Vec::new(), self.raw.to_vec()),
        }
    }
}

/// `n`-я часть (с 1) внутри сущности, нумерация — как у MS-07 и split.
fn step<'m, 'x>(e: Ent<'m, 'x>, n: usize) -> Option<Ent<'m, 'x>> {
    let (e, parent) = match e.part {
        None => (e, 0),
        Some(i) => match &e.get()?.body {
            PartType::Multipart(_) => (e, i),
            PartType::Message(_) => return step(e.nested()?, n),
            _ => return None,
        },
    };
    match &e.m.parts.get(parent)?.body {
        // Дети после родителя (защита от циклов битой структуры).
        PartType::Multipart(ids) => ids
            .iter()
            .map(|&c| c as usize)
            .filter(|&c| c > parent && c < e.m.parts.len())
            .nth(n.checked_sub(1)?)
            .map(|c| e.at(c)),
        _ if e.part.is_none() => (n == 1).then(|| e.at(0)),
        _ => None,
    }
}

fn walk<'m, 'x>(msg: &'m mail_parser::Message<'x>, path: &[NonZeroU32]) -> Option<Ent<'m, 'x>> {
    path.iter()
        .try_fold(Ent::top(msg), |e, n| step(e, n.get() as usize))
}

/// Письмо раздела: верхнее или вложенное `message/rfc822` части `part`.
fn message_at<'m, 'x>(
    msg: &'m mail_parser::Message<'x>,
    part: Option<&Part>,
) -> Option<Ent<'m, 'x>> {
    match part {
        None => Some(Ent::top(msg)),
        Some(part) => walk(msg, part.0.as_ref())?.nested(),
    }
}

/// Поля заголовка с именами из `names` (`not` — кроме них) и пустая строка в конце.
fn header_fields(header: &[u8], names: &[Vec<u8>], not: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let mut field: Vec<u8> = Vec::new();
    let flush = |field: &mut Vec<u8>, out: &mut Vec<u8>| {
        if field.is_empty() {
            return;
        }
        let name = field
            .iter()
            .position(|&b| b == b':')
            .map(|i| field[..i].trim_ascii())
            .unwrap_or_default();
        let hit = names.iter().any(|n| n.eq_ignore_ascii_case(name));
        if hit != not {
            out.extend_from_slice(field);
        }
        field.clear();
    };
    for line in header.split_inclusive(|&b| b == b'\n') {
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        if !matches!(line.first(), Some(b' ' | b'\t')) {
            flush(&mut field, &mut out);
        }
        field.extend_from_slice(line);
    }
    flush(&mut field, &mut out);
    out.extend_from_slice(b"\r\n");
    out
}

/// Байты раздела по разобранному письму; `None` — раздела нет (пустая строка в ответе).
fn section_bytes(msg: &mail_parser::Message<'_>, section: &Section<'_>) -> Option<Vec<u8>> {
    let fields = |names: &Vec1<crate::proto::types::core::AString<'_>>| -> Vec<Vec<u8>> {
        names.as_ref().iter().map(|n| n.as_ref().to_vec()).collect()
    };
    Some(match section {
        Section::Part(part) => {
            let e = walk(msg, part.0.as_ref())?;
            let p = e.get()?;
            e.bytes(p.offset_body..p.offset_end)
        }
        Section::Mime(part) => {
            let e = walk(msg, part.0.as_ref())?;
            let p = e.get()?;
            e.bytes(p.offset_header..p.offset_body)
        }
        Section::Header(part) => message_at(msg, part.as_ref())?.header_and_text().0,
        Section::Text(part) => message_at(msg, part.as_ref())?.header_and_text().1,
        Section::HeaderFields(part, names) => {
            let header = message_at(msg, part.as_ref())?.header_and_text().0;
            header_fields(&header, &fields(names), false)
        }
        Section::HeaderFieldsNot(part, names) => {
            let header = message_at(msg, part.as_ref())?.header_and_text().0;
            header_fields(&header, &fields(names), true)
        }
    })
}

/// Тело части по пути без кодировки передачи (BINARY); `Err` — неизвестная кодировка.
fn decode_part(
    msg: &mail_parser::Message<'_>,
    path: &[NonZeroU32],
) -> std::result::Result<Vec<u8>, ()> {
    let Some(e) = walk(msg, path) else {
        return Ok(Vec::new());
    };
    let Some(p) = e.get() else {
        return Ok(Vec::new());
    };
    let body = e.bytes(p.offset_body..p.offset_end);
    if matches!(p.body, PartType::Multipart(_) | PartType::Message(_)) {
        return Ok(body);
    }
    decode_cte(p.content_transfer_encoding(), &body)
}

fn decode_cte(cte: Option<&str>, body: &[u8]) -> std::result::Result<Vec<u8>, ()> {
    match cte.map(str::to_ascii_lowercase).as_deref() {
        None | Some("7bit" | "8bit" | "binary") => Ok(body.to_vec()),
        // Битые данные известной кодировки — как есть: UNKNOWN-CTE только для неизвестной (RFC 3516).
        Some("base64") => {
            Ok(mail_parser::decoders::base64::base64_decode(body).unwrap_or_else(|| body.to_vec()))
        }
        Some("quoted-printable") => Ok(
            mail_parser::decoders::quoted_printable::quoted_printable_decode(body)
                .unwrap_or_else(|| body.to_vec()),
        ),
        Some(_) => Err(()),
    }
}

fn partial(data: Vec<u8>, partial: Option<(u32, NonZeroU32)>) -> Vec<u8> {
    match partial {
        None => data,
        Some((origin, len)) => {
            let start = (origin as usize).min(data.len());
            let end = start.saturating_add(len.get() as usize).min(data.len());
            data[start..end].to_vec()
        }
    }
}

async fn collect(mut stream: ByteStream) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(chunk) = stream.next().await {
        out.extend_from_slice(&chunk?);
    }
    Ok(out)
}

fn imap_flags(flags: &tp_contracts::meta::Flags) -> Vec<FlagFetch<'static>> {
    flags
        .iter()
        .filter_map(|f| {
            Some(FlagFetch::Flag(match f {
                Flag::Seen => ImapFlag::Seen,
                Flag::Answered => ImapFlag::Answered,
                Flag::Flagged => ImapFlag::Flagged,
                Flag::Deleted => ImapFlag::Deleted,
                Flag::Draft => ImapFlag::Draft,
                Flag::Keyword(k) => ImapFlag::Keyword(Atom::try_from(k.clone()).ok()?),
            }))
        })
        .collect()
}

fn internal_date(d: time::OffsetDateTime) -> Option<DateTime> {
    let utc = chrono::FixedOffset::east_opt(0)?;
    let dt = chrono::DateTime::from_timestamp(d.unix_timestamp(), 0)?.with_timezone(&utc);
    DateTime::try_from(dt).ok()
}

impl<T: Transport> Session<T> {
    /// Номера писем выбранной папки (индексы снимка) по набору; `None` — номер вне диапазона (`BAD`).
    pub(crate) fn resolve_set(&self, set: &SequenceSet, uid: bool) -> Option<Vec<usize>> {
        let messages = &self.selected.as_ref()?.messages;
        let mut out = Vec::new();
        if uid {
            let Some(&(max, _)) = messages.last() else {
                return Some(out);
            };
            let max = NonZeroU32::new(max.0).unwrap_or(NonZeroU32::MIN);
            let ranges: Vec<(u32, u32)> = set
                .0
                .as_ref()
                .iter()
                .map(|s| match s {
                    Sequence::Single(v) => (v.expand(max).get(), v.expand(max).get()),
                    Sequence::Range(a, b) => {
                        let (a, b) = (a.expand(max).get(), b.expand(max).get());
                        (a.min(b), a.max(b))
                    }
                })
                .collect();
            for (i, (u, _)) in messages.iter().enumerate() {
                if ranges.iter().any(|(a, b)| (*a..=*b).contains(&u.0)) {
                    out.push(i);
                }
            }
            return Some(out);
        }
        let len = u32::try_from(messages.len()).ok()?;
        let largest = NonZeroU32::new(len)?;
        for s in set.0.as_ref() {
            let (a, b) = match s {
                Sequence::Single(v) => (v.expand(largest).get(), v.expand(largest).get()),
                Sequence::Range(a, b) => {
                    let (a, b) = (a.expand(largest).get(), b.expand(largest).get());
                    (a.min(b), a.max(b))
                }
            };
            if b > len {
                return None;
            }
            out.extend((a..=b).map(|n| n as usize - 1));
        }
        out.sort_unstable();
        out.dedup();
        Some(out)
    }

    pub(crate) async fn fetch(
        &mut self,
        tag: &Tag<'static>,
        set: &SequenceSet,
        items: MacroOrMessageDataItemNames<'static>,
        uid: bool,
    ) -> Result<()> {
        let Some(indexes) = self.resolve_set(set, uid) else {
            self.bad(Some(tag), "Invalid message sequence number");
            return Ok(());
        };
        let items = expand(items, uid);
        let Some(selected) = self.selected.clone() else {
            return Ok(());
        };
        let scope = self.scope().ok_or(Error::Forbidden)?;
        let mark_seen = !selected.read_only && items.iter().any(sets_seen);
        let batch = self.ctx.config.limits.fetch_batch.max(1);
        for chunk in indexes.chunks(batch) {
            let ids: Vec<MessageId> = chunk.iter().map(|&i| selected.messages[i].1).collect();
            let list = self
                .ctx
                .deps
                .messages
                .list(
                    scope,
                    selected.mailbox,
                    MessageQuery::Ids(ids.clone()),
                    ids.len(),
                )
                .await?;
            let mut by_id: HashMap<MessageId, Message> =
                list.into_iter().map(|m| (m.id, m)).collect();
            if mark_seen {
                let unseen: Vec<MessageId> = by_id
                    .values()
                    .filter(|m| !m.flags.contains(&Flag::Seen))
                    .map(|m| m.id)
                    .collect();
                if !unseen.is_empty() {
                    let op = FlagOp::Add([Flag::Seen].into());
                    self.ctx
                        .deps
                        .store
                        .set_flags(scope, &unseen, op, None)
                        .await?;
                    for id in unseen {
                        if let Some(m) = by_id.get_mut(&id) {
                            m.flags.insert(Flag::Seen);
                        }
                    }
                }
            }
            for &i in chunk {
                // Письмо удалено другой сессией — пропускается.
                let Some(msg) = by_id.get(&selected.messages[i].1) else {
                    continue;
                };
                let data = match self.fetch_one(msg, &items, mark_seen).await {
                    Ok(data) => data,
                    Err(Fail::UnknownCte) => {
                        self.no(
                            tag,
                            Some(Code::UnknownCte),
                            "Unknown Content-Transfer-Encoding",
                        );
                        return Ok(());
                    }
                    Err(Fail::Store(e)) => return Err(e),
                };
                let seq = proto::nz(u32::try_from(i + 1).unwrap_or(u32::MAX));
                if let Ok(items) = Vec1::try_from(data) {
                    self.data(Data::Fetch { seq, items });
                }
                self.maybe_flush().await?;
            }
        }
        let verb = if uid { "UID FETCH" } else { "FETCH" };
        self.ok(tag, None, &format!("{verb} completed"));
        Ok(())
    }

    /// Элементы ответа `FETCH` одного письма.
    async fn fetch_one(
        &self,
        msg: &Message,
        items: &[MessageDataItemName<'static>],
        seen_changed: bool,
    ) -> std::result::Result<Vec<MessageDataItem<'static>>, Fail> {
        let scope = self.scope().ok_or(Error::Forbidden)?;
        let store = &self.ctx.deps.store;
        let rev2 = self.rev2;
        let mut raw: Option<Vec<u8>> = None;
        let mut out = Vec::new();
        // Полное письмо загружается один раз, если нужно хотя бы одному элементу.
        macro_rules! raw {
            () => {{
                if raw.is_none() {
                    raw = Some(collect(store.fetch_raw(scope, msg.id, None).await?).await?);
                }
                raw.as_deref().unwrap_or_default()
            }};
        }
        let mut nested: HashMap<String, Envelope> = HashMap::new();
        let needs_nested = items.iter().any(|i| {
            matches!(
                i,
                MessageDataItemName::BodyStructure | MessageDataItemName::Body
            )
        });
        if needs_nested {
            let mut paths = Vec::new();
            rfc822_parts(&msg.meta.body, &mut paths);
            if !paths.is_empty() {
                let bytes = raw!().to_vec();
                if let Some(parsed) = MessageParser::default().parse(&bytes) {
                    for p in paths {
                        let nums: Option<Vec<NonZeroU32>> =
                            p.split('.').map(|n| n.parse().ok()).collect();
                        let section = nums.and_then(|n| Vec1::try_from(n).ok()).map(Part);
                        let body = section
                            .and_then(|s| section_bytes(&parsed, &Section::Part(s)))
                            .unwrap_or_default();
                        // Вложение в base64 или quoted-printable — конверт по раскодированному.
                        let cte = find_part(&msg.meta.body, &p).and_then(|b| b.encoding.clone());
                        let body = decode_cte(cte.as_deref(), &body).unwrap_or(body);
                        nested.insert(p, tp_mailstore::meta::message_meta(&body).envelope);
                    }
                }
            }
        }
        let mut flags_done = false;
        for item in items {
            match item {
                MessageDataItemName::Flags => {
                    flags_done = true;
                    out.push(MessageDataItem::Flags(imap_flags(&msg.flags)));
                }
                MessageDataItemName::Uid => {
                    out.push(MessageDataItem::Uid(proto::nz(msg.uid.0)));
                }
                MessageDataItemName::InternalDate => {
                    if let Some(d) = internal_date(msg.received_at) {
                        out.push(MessageDataItem::InternalDate(d));
                    }
                }
                MessageDataItemName::Rfc822Size => out.push(MessageDataItem::Rfc822Size(
                    u32::try_from(msg.size).unwrap_or(u32::MAX),
                )),
                MessageDataItemName::ModSeq => {
                    let m = NonZeroU64::new(msg.modseq.0).unwrap_or(NonZeroU64::MIN);
                    out.push(MessageDataItem::ModSeq(m));
                }
                MessageDataItemName::Envelope => {
                    out.push(MessageDataItem::Envelope(envelope(
                        &msg.meta.envelope,
                        rev2,
                    )));
                }
                MessageDataItemName::BodyStructure => out.push(MessageDataItem::BodyStructure(
                    body_structure(&msg.meta.body, true, rev2, &nested),
                )),
                MessageDataItemName::Body => out.push(MessageDataItem::Body(body_structure(
                    &msg.meta.body,
                    false,
                    rev2,
                    &nested,
                ))),
                MessageDataItemName::Rfc822 => {
                    out.push(MessageDataItem::Rfc822(nstring_bytes(raw!().to_vec())));
                }
                MessageDataItemName::Rfc822Header => {
                    let header = self.header(msg, &mut raw).await?;
                    out.push(MessageDataItem::Rfc822Header(nstring_bytes(header)));
                }
                MessageDataItemName::Rfc822Text => {
                    let parsed = MessageParser::default().parse(raw!());
                    let text = parsed
                        .map(|m| Ent::top(&m).header_and_text().1)
                        .unwrap_or_default();
                    out.push(MessageDataItem::Rfc822Text(nstring_bytes(text)));
                }
                MessageDataItemName::BodyExt {
                    section,
                    partial: p,
                    ..
                } => {
                    let data = self
                        .body_section(msg, section.as_ref(), *p, &mut raw)
                        .await?;
                    out.push(MessageDataItem::BodyExt {
                        section: section.clone(),
                        origin: p.map(|(o, _)| o),
                        data: nstring_bytes(data),
                    });
                }
                MessageDataItemName::Binary {
                    section,
                    partial: p,
                    ..
                } => {
                    let data = partial(self.binary(msg, section, &mut raw).await?, *p);
                    let value = if data.contains(&0) {
                        NString8::Literal8(Literal8 {
                            data: data.into(),
                            mode: LiteralMode::Sync,
                        })
                    } else {
                        NString8::NString(nstring_bytes(data))
                    };
                    out.push(MessageDataItem::Binary {
                        section: section.clone(),
                        value,
                    });
                }
                MessageDataItemName::BinarySize { section } => {
                    let size = self.binary(msg, section, &mut raw).await?.len();
                    out.push(MessageDataItem::BinarySize {
                        section: section.clone(),
                        size: u32::try_from(size).unwrap_or(u32::MAX),
                    });
                }
            }
        }
        // Выборка поставила \Seen: клиенту сообщаются новые флаги (RFC 9051, 6.4.5).
        if seen_changed && !flags_done {
            out.push(MessageDataItem::Flags(imap_flags(&msg.flags)));
        }
        Ok(out)
    }

    /// Заголовок письма: из начала письма, если конец заголовка в нём, иначе из полного письма.
    async fn header(&self, msg: &Message, raw: &mut Option<Vec<u8>>) -> Result<Vec<u8>> {
        let full = match raw {
            Some(r) => r.clone(),
            None => {
                let scope = self.scope().ok_or(Error::Forbidden)?;
                let probe = HEAD_PROBE.min(msg.size);
                let head = collect(
                    self.ctx
                        .deps
                        .store
                        .fetch_raw(scope, msg.id, Some(0..probe))
                        .await?,
                )
                .await?;
                let end = head
                    .windows(4)
                    .position(|w| w == b"\r\n\r\n")
                    .map(|i| i + 4)
                    .or_else(|| head.windows(2).position(|w| w == b"\n\n").map(|i| i + 2));
                match end {
                    Some(end) => return Ok(head[..end].to_vec()),
                    None if probe >= msg.size => return Ok(head),
                    None => {
                        let full =
                            collect(self.ctx.deps.store.fetch_raw(scope, msg.id, None).await?)
                                .await?;
                        *raw = Some(full.clone());
                        full
                    }
                }
            }
        };
        Ok(MessageParser::default()
            .parse(&full)
            .map(|m| Ent::top(&m).header_and_text().0)
            .unwrap_or(full))
    }

    async fn full(&self, msg: &Message, raw: &mut Option<Vec<u8>>) -> Result<Vec<u8>> {
        if let Some(r) = raw {
            return Ok(r.clone());
        }
        let scope = self.scope().ok_or(Error::Forbidden)?;
        let full = collect(self.ctx.deps.store.fetch_raw(scope, msg.id, None).await?).await?;
        *raw = Some(full.clone());
        Ok(full)
    }

    /// Тело листовой части в кодировке передачи через `fetch_part`; `None` — не лист или нет объекта.
    async fn leaf(&self, msg: &Message, path: &[NonZeroU32]) -> Result<Option<Vec<u8>>> {
        let path = part_path(path);
        let leaf = find_part(&msg.meta.body, &path).is_some_and(|p| {
            p.children.is_empty() && p.media_type != "multipart" && p.media_type != "message"
        });
        if !leaf || path.is_empty() {
            return Ok(None);
        }
        let scope = self.scope().ok_or(Error::Forbidden)?;
        match self.ctx.deps.store.fetch_part(scope, msg.id, &path).await {
            Ok(stream) => Ok(Some(collect(stream).await?)),
            Err(Error::NotFound | Error::Invalid(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn body_section(
        &self,
        msg: &Message,
        section: Option<&Section<'static>>,
        p: Option<(u32, NonZeroU32)>,
        raw: &mut Option<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        let scope = self.scope().ok_or(Error::Forbidden)?;
        match section {
            None => match (p, raw.as_ref()) {
                // Диапазон всего письма — без загрузки целиком.
                (Some((origin, len)), None) => {
                    let start = u64::from(origin).min(msg.size);
                    let end = (start + u64::from(len.get())).min(msg.size);
                    Ok(collect(
                        self.ctx
                            .deps
                            .store
                            .fetch_raw(scope, msg.id, Some(start..end))
                            .await?,
                    )
                    .await?)
                }
                _ => Ok(partial(self.full(msg, raw).await?, p)),
            },
            Some(Section::Header(None)) => Ok(partial(self.header(msg, raw).await?, p)),
            Some(Section::HeaderFields(None, _) | Section::HeaderFieldsNot(None, _)) => {
                let header = self.header(msg, raw).await?;
                let parsed = MessageParser::default().parse(&header);
                let data = parsed
                    .and_then(|m| section_bytes(&m, section?))
                    .unwrap_or_else(|| b"\r\n".to_vec());
                Ok(partial(data, p))
            }
            Some(Section::Part(part)) => {
                if raw.is_none()
                    && let Some(body) = self.leaf(msg, part.0.as_ref()).await?
                {
                    return Ok(partial(body, p));
                }
                let full = self.full(msg, raw).await?;
                let data = MessageParser::default()
                    .parse(&full)
                    .and_then(|m| section_bytes(&m, section?))
                    .unwrap_or_default();
                Ok(partial(data, p))
            }
            Some(s) => {
                let full = self.full(msg, raw).await?;
                let data = MessageParser::default()
                    .parse(&full)
                    .and_then(|m| section_bytes(&m, s))
                    .unwrap_or_default();
                Ok(partial(data, p))
            }
        }
    }

    /// `BINARY[section]` без диапазона.
    async fn binary(
        &self,
        msg: &Message,
        section: &[NonZeroU32],
        raw: &mut Option<Vec<u8>>,
    ) -> std::result::Result<Vec<u8>, Fail> {
        if section.is_empty() {
            return Ok(self.full(msg, raw).await?);
        }
        if raw.is_none()
            && let Some(body) = self.leaf(msg, section).await?
        {
            let path = part_path(section);
            let cte = find_part(&msg.meta.body, &path).and_then(|p| p.encoding.clone());
            return decode_cte(cte.as_deref(), &body).map_err(|()| Fail::UnknownCte);
        }
        let full = self.full(msg, raw).await?;
        let Some(parsed) = MessageParser::default().parse(&full) else {
            return Ok(Vec::new());
        };
        decode_part(&parsed, section).map_err(|()| Fail::UnknownCte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_fields_filter() {
        let h = b"From: a\r\nSubject: long\r\n  folded\r\nTo: b\r\n\r\n";
        let names = vec![b"subject".to_vec(), b"TO".to_vec()];
        assert_eq!(
            header_fields(h, &names, false),
            b"Subject: long\r\n  folded\r\nTo: b\r\n\r\n"
        );
        assert_eq!(header_fields(h, &names, true), b"From: a\r\n\r\n");
        assert_eq!(header_fields(b"", &names, false), b"\r\n");
    }

    #[test]
    fn partial_ranges() {
        assert_eq!(
            partial(b"0123456789".to_vec(), Some((2, proto::nz(3)))),
            b"234"
        );
        assert_eq!(partial(b"0123".to_vec(), Some((3, proto::nz(10)))), b"3");
        assert_eq!(partial(b"0123".to_vec(), Some((10, proto::nz(1)))), b"");
    }

    #[test]
    fn rfc2047_for_rev1() {
        assert_eq!(text("Привет", false), b"=?utf-8?B?0J/RgNC40LLQtdGC?=");
        assert_eq!(text("Привет", true), "Привет".as_bytes());
        assert_eq!(text("Hello", false), b"Hello");
    }
}
