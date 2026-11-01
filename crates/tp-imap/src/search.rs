//! `SEARCH` и `UID SEARCH` (RFC 9051, 6.4.4), ESEARCH (`RETURN (MIN MAX ALL COUNT)`, RFC 4731), критерий
//! `MODSEQ` (CONDSTORE, RFC 7162, 3.1.5).
//!
//! Критерии по метаданным — флаги, размер, даты, номера, UID, `MODSEQ`, поля конверта (`SUBJECT`, `FROM`,
//! `TO`, `CC`, `BCC`, `HEADER` этих полей, `Message-ID` и `In-Reply-To`) — проверяются по метаданным хранилища:
//! подстрока без учёта регистра в декодированном тексте. `BODY`, `TEXT` и прочие `HEADER` — запросом
//! к полнотекстовому индексу ([`SearchIndex`]): по словам с морфологией, а не по подстроке. Индекс может
//! отставать от приёма: только что пришедшее письмо по тексту может не найтись.
//!
//! Ответ — `* SEARCH` для клиентов IMAP4rev1 без `RETURN`, иначе `* ESEARCH` (в IMAP4rev2 — всегда).
//! `RETURN (SAVE)` (SEARCHRES) не поддерживается: imap-codec не разбирает `$` в наборах номеров.

use std::collections::{HashMap, HashSet};
use std::num::{NonZeroU32, NonZeroU64};

use tp_contracts::meta::{Address, Flag, Message, MessageQuery};
use tp_contracts::search::{SearchSort, TextField, TextQuery};
use tp_contracts::{Error, MessageId, Result};

use crate::proto::types::core::{AString, Charset, Tag, Vec1};
use crate::proto::types::response::{Code, Data};
use crate::proto::types::search::SearchKey;
use crate::proto::types::sequence::{Sequence, SequenceSet};
use crate::proto::{self, SearchReturn};
use crate::session::{Session, Transport};

/// Команда поиска: критерии imap-codec и параметры `RETURN`.
pub(crate) struct SearchCmd {
    pub charset: Option<Charset<'static>>,
    pub criteria: Vec1<SearchKey<'static>>,
    pub uid: bool,
    /// `None` — без `RETURN`.
    pub ret: Option<Vec<SearchReturn>>,
}

/// Данные для проверки критериев одного письма.
struct Ctx {
    /// Наибольший номер последовательности (`*`).
    last_seq: NonZeroU32,
    /// Наибольший UID папки (`*` в `UID`).
    last_uid: NonZeroU32,
    /// Письма, найденные индексом, по текстовому условию.
    text: Vec<(TextQuery, HashSet<MessageId>)>,
}

fn string(s: &AString<'_>) -> String {
    String::from_utf8_lossy(s.as_ref()).to_lowercase()
}

/// Поле конверта для `HEADER` (имя в нижнем регистре); иначе — запрос к индексу.
fn envelope_header(name: &str) -> bool {
    matches!(
        name,
        "subject" | "from" | "to" | "cc" | "bcc" | "message-id" | "in-reply-to"
    )
}

/// Текстовое условие для индекса; `None` — критерий проверяется по метаданным.
fn text_query(key: &SearchKey<'_>) -> Option<TextQuery> {
    Some(match key {
        SearchKey::Body(s) => TextQuery::term(TextField::Body, string(s)),
        SearchKey::Text(s) => TextQuery::term(TextField::Any, string(s)),
        SearchKey::Header(name, value) => {
            let name = string(name);
            if envelope_header(&name) {
                return None;
            }
            TextQuery::term(TextField::Header(name), string(value))
        }
        _ => return None,
    })
}

fn collect_text(key: &SearchKey<'_>, out: &mut Vec<TextQuery>) {
    match key {
        SearchKey::And(keys) => keys.as_ref().iter().for_each(|k| collect_text(k, out)),
        SearchKey::Or(a, b) => {
            collect_text(a, out);
            collect_text(b, out);
        }
        SearchKey::Not(k) => collect_text(k, out),
        k => {
            if let Some(q) = text_query(k)
                && !out.contains(&q)
            {
                out.push(q);
            }
        }
    }
}

fn uses_modseq(key: &SearchKey<'_>) -> bool {
    match key {
        SearchKey::And(keys) => keys.as_ref().iter().any(uses_modseq),
        SearchKey::Or(a, b) => uses_modseq(a) || uses_modseq(b),
        SearchKey::Not(k) => uses_modseq(k),
        SearchKey::ModSequence { .. } => true,
        _ => false,
    }
}

fn in_set(set: &SequenceSet, n: u32, largest: NonZeroU32) -> bool {
    set.0.as_ref().iter().any(|s| match s {
        Sequence::Single(v) => v.expand(largest).get() == n,
        Sequence::Range(a, b) => {
            let (a, b) = (a.expand(largest).get(), b.expand(largest).get());
            (a.min(b)..=a.max(b)).contains(&n)
        }
    })
}

fn date(d: time::OffsetDateTime) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::from_ymd_opt(d.year(), u32::from(u8::from(d.month())), u32::from(d.day()))
}

fn addresses(list: &[Address]) -> String {
    list.iter()
        .map(|a| match &a.name {
            Some(n) => format!("{n} <{}>", a.email),
            None => a.email.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
        .to_lowercase()
}

fn contains(field: Option<&str>, needle: &str) -> bool {
    field.is_some_and(|f| f.to_lowercase().contains(needle))
}

fn has_flag(m: &Message, f: &Flag) -> bool {
    m.flags.contains(f)
}

fn has_keyword(m: &Message, k: &str) -> bool {
    m.flags
        .iter()
        .any(|f| matches!(f, Flag::Keyword(x) if x.eq_ignore_ascii_case(k)))
}

/// Поле конверта по имени заголовка (имя в нижнем регистре).
fn envelope_field(m: &Message, name: &str) -> Option<String> {
    let e = &m.meta.envelope;
    Some(match name {
        "subject" => e.subject.clone()?,
        "from" => addresses(&e.from),
        "to" => addresses(&e.to),
        "cc" => addresses(&e.cc),
        "bcc" => addresses(&e.bcc),
        "message-id" => e.message_id.clone()?,
        "in-reply-to" => e.in_reply_to.clone()?,
        _ => return None,
    })
}

/// Письмо удовлетворяет критерию.
fn matches(key: &SearchKey<'_>, m: &Message, seq: u32, ctx: &Ctx) -> bool {
    let received = date(m.received_at);
    // Дата заголовка Date в его часовом поясе; нет заголовка — дата приёма.
    let sent = m.meta.envelope.date.and_then(date).or(received);
    let on = |d: Option<chrono::NaiveDate>, f: &dyn Fn(chrono::NaiveDate) -> bool| d.is_some_and(f);
    match key {
        SearchKey::And(keys) => keys.as_ref().iter().all(|k| matches(k, m, seq, ctx)),
        SearchKey::Or(a, b) => matches(a, m, seq, ctx) || matches(b, m, seq, ctx),
        SearchKey::Not(k) => !matches(k, m, seq, ctx),
        SearchKey::SequenceSet(set) => in_set(set, seq, ctx.last_seq),
        SearchKey::Uid(set) => in_set(set, m.uid.0, ctx.last_uid),
        SearchKey::All | SearchKey::Old => true,
        // `\Recent` не поддерживается (RFC 9051 его удалил): `RECENT` и `NEW` не находят ничего.
        SearchKey::Recent | SearchKey::New => false,
        SearchKey::Answered => has_flag(m, &Flag::Answered),
        SearchKey::Deleted => has_flag(m, &Flag::Deleted),
        SearchKey::Draft => has_flag(m, &Flag::Draft),
        SearchKey::Flagged => has_flag(m, &Flag::Flagged),
        SearchKey::Seen => has_flag(m, &Flag::Seen),
        SearchKey::Unanswered => !has_flag(m, &Flag::Answered),
        SearchKey::Undeleted => !has_flag(m, &Flag::Deleted),
        SearchKey::Undraft => !has_flag(m, &Flag::Draft),
        SearchKey::Unflagged => !has_flag(m, &Flag::Flagged),
        SearchKey::Unseen => !has_flag(m, &Flag::Seen),
        SearchKey::Keyword(k) => has_keyword(m, k.inner()),
        SearchKey::Unkeyword(k) => !has_keyword(m, k.inner()),
        SearchKey::Larger(n) => m.size > u64::from(*n),
        SearchKey::Smaller(n) => m.size < u64::from(*n),
        SearchKey::Before(d) => on(received, &|r| r < *d.as_ref()),
        SearchKey::On(d) => on(received, &|r| r == *d.as_ref()),
        SearchKey::Since(d) => on(received, &|r| r >= *d.as_ref()),
        SearchKey::SentBefore(d) => on(sent, &|r| r < *d.as_ref()),
        SearchKey::SentOn(d) => on(sent, &|r| r == *d.as_ref()),
        SearchKey::SentSince(d) => on(sent, &|r| r >= *d.as_ref()),
        SearchKey::ModSequence { modseq, .. } => m.modseq.0 >= *modseq,
        SearchKey::Subject(s) => contains(envelope_field(m, "subject").as_deref(), &string(s)),
        SearchKey::From(s) => contains(envelope_field(m, "from").as_deref(), &string(s)),
        SearchKey::To(s) => contains(envelope_field(m, "to").as_deref(), &string(s)),
        SearchKey::Cc(s) => contains(envelope_field(m, "cc").as_deref(), &string(s)),
        SearchKey::Bcc(s) => contains(envelope_field(m, "bcc").as_deref(), &string(s)),
        SearchKey::Header(name, value) if envelope_header(&string(name)) => {
            contains(envelope_field(m, &string(name)).as_deref(), &string(value))
        }
        key @ (SearchKey::Header(..) | SearchKey::Body(_) | SearchKey::Text(_)) => text_query(key)
            .and_then(|q| ctx.text.iter().find(|(t, _)| *t == q))
            .map(|(_, hits)| hits)
            .is_some_and(|hits| hits.contains(&m.id)),
    }
}

/// Ответ `* ESEARCH` (RFC 4731, 3.1): пустой результат — без `MIN`, `MAX`, `ALL`.
fn esearch_line(
    tag: &Tag<'_>,
    uid: bool,
    ret: &[SearchReturn],
    nums: &[u32],
    modseq: Option<u64>,
) -> Vec<u8> {
    let mut line = format!("* ESEARCH (TAG \"{}\")", tag.inner());
    if uid {
        line.push_str(" UID");
    }
    let all = [SearchReturn::All];
    let ret = if ret.is_empty() { &all[..] } else { ret };
    let (min, max) = (nums.iter().min(), nums.iter().max());
    for r in [
        SearchReturn::Min,
        SearchReturn::Max,
        SearchReturn::All,
        SearchReturn::Count,
    ] {
        if !ret.contains(&r) {
            continue;
        }
        match (r, min, max) {
            (SearchReturn::Min, Some(v), _) => line.push_str(&format!(" MIN {v}")),
            (SearchReturn::Max, _, Some(v)) => line.push_str(&format!(" MAX {v}")),
            (SearchReturn::All, Some(_), _) => {
                line.push_str(&format!(" ALL {}", proto::sequence_text(nums)));
            }
            (SearchReturn::Count, ..) => line.push_str(&format!(" COUNT {}", nums.len())),
            _ => {}
        }
    }
    if let Some(m) = modseq.filter(|_| !nums.is_empty()) {
        line.push_str(&format!(" MODSEQ {m}"));
    }
    line.push_str("\r\n");
    line.into_bytes()
}

impl<T: Transport> Session<T> {
    /// `SEARCH`/`UID SEARCH`, в том числе с `RETURN`.
    pub(crate) async fn search(&mut self, tag: &Tag<'static>, cmd: SearchCmd) -> Result<()> {
        if let Some(cs) = &cmd.charset {
            let name = match cs {
                Charset::Atom(a) => a.inner(),
                Charset::Quoted(q) => q.inner(),
            }
            .to_ascii_uppercase();
            if name != "UTF-8" && name != "US-ASCII" {
                self.no(
                    tag,
                    Some(Code::BadCharset {
                        allowed: vec![
                            Charset::try_from("US-ASCII").map_err(|_| Error::invalid("charset"))?,
                            Charset::try_from("UTF-8").map_err(|_| Error::invalid("charset"))?,
                        ],
                    }),
                    "Unsupported charset",
                );
                return Ok(());
            }
        }
        if cmd
            .ret
            .as_ref()
            .is_some_and(|r| r.contains(&SearchReturn::Save))
        {
            self.bad(Some(tag), "RETURN (SAVE) is not supported");
            return Ok(());
        }
        let Some(selected) = self.selected.as_ref() else {
            return Ok(());
        };
        let mailbox = selected.mailbox;
        let snapshot = selected.messages.clone();
        let scope = self.scope().ok_or(Error::Forbidden)?;
        let key = SearchKey::And(cmd.criteria);
        let modseq_used = uses_modseq(&key);
        // Критерий MODSEQ включает CONDSTORE (RFC 7162, 3.1).
        if modseq_used {
            self.condstore = true;
        }
        let limit = self.ctx.config.limits.max_mailbox_messages;
        let mut queries = Vec::new();
        collect_text(&key, &mut queries);
        let mut text = Vec::new();
        for q in queries {
            let ids = self
                .ctx
                .deps
                .search
                .query(scope, &[mailbox], &q, SearchSort::OldestFirst, limit)
                .await?;
            text.push((q, ids.into_iter().collect::<HashSet<_>>()));
        }
        let list = self
            .ctx
            .deps
            .messages
            .list(scope, mailbox, MessageQuery::All, limit)
            .await?;
        let by_id: HashMap<MessageId, Message> = list.into_iter().map(|m| (m.id, m)).collect();
        let largest = |n: Option<u32>| NonZeroU32::new(n.unwrap_or(1)).unwrap_or(NonZeroU32::MIN);
        let ctx = Ctx {
            last_seq: largest(u32::try_from(snapshot.len()).ok()),
            last_uid: largest(snapshot.last().map(|(u, _)| u.0)),
            text,
        };
        let mut nums = Vec::new();
        let mut highest = 0u64;
        for (i, (_, id)) in snapshot.iter().enumerate() {
            let seq = u32::try_from(i + 1).unwrap_or(u32::MAX);
            // Письмо удалено другой сессией — не найдено.
            let Some(m) = by_id.get(id) else { continue };
            if matches(&key, m, seq, &ctx) {
                nums.push(if cmd.uid { m.uid.0 } else { seq });
                highest = highest.max(m.modseq.0);
            }
        }
        let modseq = modseq_used.then_some(highest);
        match cmd.ret {
            Some(ret) => self
                .out
                .extend(esearch_line(tag, cmd.uid, &ret, &nums, modseq)),
            None if self.rev2 => self
                .out
                .extend(esearch_line(tag, cmd.uid, &[], &nums, modseq)),
            None => {
                let nums: Vec<NonZeroU32> = nums.iter().map(|&n| proto::nz(n)).collect();
                let modseq = modseq.and_then(|m| (!nums.is_empty()).then_some(m));
                self.data(Data::Search(nums, modseq.and_then(NonZeroU64::new)));
            }
        }
        let verb = if cmd.uid { "UID SEARCH" } else { "SEARCH" };
        self.ok(tag, None, &format!("{verb} completed"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn esearch_format() {
        let tag = Tag::try_from("A1").expect("tag");
        let line = |ret: &[SearchReturn], nums: &[u32]| {
            String::from_utf8(esearch_line(&tag, false, ret, nums, None)).expect("utf8")
        };
        // RFC 4731, 3.1.
        assert_eq!(
            line(&[SearchReturn::Min, SearchReturn::Count], &[2, 10, 11]),
            "* ESEARCH (TAG \"A1\") MIN 2 COUNT 3\r\n"
        );
        assert_eq!(
            line(&[], &[2, 10, 11]),
            "* ESEARCH (TAG \"A1\") ALL 2,10:11\r\n"
        );
        assert_eq!(
            line(
                &[SearchReturn::Min, SearchReturn::Max, SearchReturn::Count],
                &[]
            ),
            "* ESEARCH (TAG \"A1\") COUNT 0\r\n"
        );
        let uid = String::from_utf8(esearch_line(
            &tag,
            true,
            &[SearchReturn::Max],
            &[7, 3800],
            Some(9),
        ))
        .expect("utf8");
        assert_eq!(uid, "* ESEARCH (TAG \"A1\") UID MAX 3800 MODSEQ 9\r\n");
    }
}
