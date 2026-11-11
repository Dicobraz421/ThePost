//! Правила Sieve при локальной доставке (SRS 4.3, CONTRACTS.md#Sieve; SMTP-31) и автоответ (SMTP-32).
//!
//! [`crate::Delivery::with_rules`] подключает точку правил [`tp_contracts::sieve::DeliveryRules`] (`tp-sieve`): для
//! каждого локального получателя — после правил потока и до `MailStore::ingest` — правила выбирают папки и флаги
//! (`keep`, `fileinto`, `:create`, `:flags`; несколько папок — несколько доставок одного `ingest`), отбрасывают
//! письмо (`discard`), перенаправляют его (`redirect`: локальные адреса — в ящики, как пересылки модулей; внешние —
//! в очередь с SRS и ARC, [`crate::forward`]) и решают автоответ. Сбой правил — доставка во «Входящие» и запись в
//! журналы. Решение правил (папки, перенаправление, отброс, ошибки) пишется в журнал доставки без содержимого письма.
//! Получатели перенаправления писем правилам не передаются (защита от петель).
//!
//! Автоответ ([`build_reply`], RFC 3834) — конверт `MAIL FROM:<>`, `Auto-Submitted: auto-replied`, `In-Reply-To`,
//! `References`: внешнему адресату — в очередь исходящей почты, локальному — сразу во «Входящие» (без правил).
//! Ответ не отправляется ([`suppressed`]) на уведомления (пустой отправитель), автоматические письма
//! (`Auto-Submitted` не `no`), письма списков рассылки (`List-*`, `Precedence: bulk|list|junk`), письма роботов
//! (`MAILER-DAEMON`, `owner-*`, `*-request`, `noreply` …), при `X-Auto-Response-Suppress: All|OOF`, на письма в
//! «Спам» и сверх `delivery.vacation_rate` ответов учётной записи в час. Период ответа одному отправителю
//! (`:days`, `VacationLog`) проверяют правила.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mail_builder::MessageBuilder;
use mail_builder::headers::address::Address;
use mail_builder::headers::date::Date;
use mail_builder::headers::raw::Raw;
use mail_builder::mime::MimePart;
use time::OffsetDateTime;
use tp_contracts::meta::{AccountScope, Mailbox, MailboxStore, NewMailbox, SpecialUse};
use tp_contracts::sieve::{AutoReply, ReplyBody};
use tp_contracts::{AccountId, Error, MailboxId, Result};

use crate::tracking::Step;

/// Итог правил всех получателей письма, кроме доставок в ящики.
#[derive(Debug, Default)]
pub(crate) struct Sieved {
    /// Адреса перенаправления (`redirect`) без повторов.
    pub redirect: Vec<String>,
    /// Автоответы: учётная запись получателя и ответ.
    pub replies: Vec<(AccountScope, AutoReply)>,
    /// События журнала доставки: решения правил.
    pub steps: Vec<Step>,
}

/// Окно ограничения частоты автоответов.
const HOUR: Duration = Duration::from_secs(3600);

/// Учётных записей в счётчике автоответов, после которых устаревшие записи удаляются.
const MAX_TRACKED: usize = 100_000;

/// Ограничение частоты автоответов учётной записи (`delivery.vacation_rate` в час, окно фиксированное; счётчики — в
/// памяти узла).
#[derive(Debug)]
pub(crate) struct VacationRate {
    per_hour: u32,
    sent: Mutex<HashMap<AccountId, (Instant, u32)>>,
}

impl VacationRate {
    /// Не больше `per_hour` ответов в час; 0 — ни одного.
    pub(crate) fn new(per_hour: u32) -> Self {
        Self {
            per_hour,
            sent: Mutex::new(HashMap::new()),
        }
    }

    /// Можно ли ответить от учётной записи `account` сейчас (разрешённый ответ учитывается).
    pub(crate) fn allow(&self, account: AccountId) -> bool {
        self.allow_at(account, Instant::now())
    }

    fn allow_at(&self, account: AccountId, now: Instant) -> bool {
        if self.per_hour == 0 {
            return false;
        }
        let mut sent = self
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if sent.len() >= MAX_TRACKED && !sent.contains_key(&account) {
            sent.retain(|_, (start, _)| now.saturating_duration_since(*start) < HOUR);
        }
        let entry = sent.entry(account).or_insert((now, 0));
        if now.saturating_duration_since(entry.0) >= HOUR {
            *entry = (now, 0);
        }
        if entry.1 >= self.per_hour {
            return false;
        }
        entry.1 += 1;
        true
    }
}

/// Части пути папки (`a/b/c`), без пустых.
fn segments(path: &str) -> impl Iterator<Item = &str> {
    path.split('/').filter(|s| !s.is_empty())
}

/// Вложенная папка `name` папки `parent` (`None` — верхний уровень; `INBOX` в любом регистре — «Входящие»).
fn child(list: &[Mailbox], parent: Option<MailboxId>, name: &str) -> Option<MailboxId> {
    list.iter()
        .find(|m| {
            m.parent == parent
                && (m.name == name
                    || (parent.is_none()
                        && m.role == Some(SpecialUse::Inbox)
                        && name.eq_ignore_ascii_case("INBOX")))
        })
        .map(|m| m.id)
}

/// Папка по пути `path` (имена от корня через `/`) среди папок `list`.
pub(crate) fn find_path(list: &[Mailbox], path: &str) -> Option<MailboxId> {
    let mut parent = None;
    for seg in segments(path) {
        parent = Some(child(list, parent, seg)?);
    }
    parent
}

/// Создаёт недостающие папки пути `path` (`fileinto :create`, RFC 5490); пустой путь — `None`. Папку, созданную
/// параллельной доставкой (`Conflict`), находит заново.
pub(crate) async fn create_path(
    store: &dyn MailboxStore,
    scope: AccountScope,
    list: &[Mailbox],
    path: &str,
) -> Result<Option<MailboxId>> {
    let mut list = list.to_vec();
    let mut parent = None;
    for seg in segments(path) {
        if let Some(id) = child(&list, parent, seg) {
            parent = Some(id);
            continue;
        }
        let new = NewMailbox {
            parent,
            name: seg.to_owned(),
            role: None,
        };
        let id = match store.create(scope, new).await {
            Ok(m) => m.id,
            Err(Error::Conflict) => {
                list = store.list(scope).await?;
                child(&list, parent, seg).ok_or(Error::Conflict)?
            }
            Err(e) => return Err(e),
        };
        parent = Some(id);
    }
    Ok(parent)
}

/// Заголовки списков рассылки (RFC 2369, RFC 2919).
const LIST_HEADERS: [&str; 7] = [
    "List-Id",
    "List-Help",
    "List-Unsubscribe",
    "List-Subscribe",
    "List-Post",
    "List-Owner",
    "List-Archive",
];

/// Адрес робота (RFC 3834 §2, RFC 5230 §4.6): `MAILER-DAEMON`, `LISTSERV`, `majordomo`, `owner-*`, `*-request`,
/// `*-owner`, `noreply` и подобные.
fn robot(address: &str) -> bool {
    let local = address
        .rsplit_once('@')
        .map_or(address, |(l, _)| l)
        .to_ascii_lowercase();
    matches!(
        local.as_str(),
        "mailer-daemon"
            | "listserv"
            | "majordomo"
            | "noreply"
            | "no-reply"
            | "donotreply"
            | "do-not-reply"
            | "do_not_reply"
            | "no_reply"
    ) || local.starts_with("owner-")
        || local.ends_with("-request")
        || local.ends_with("-owner")
        || local.starts_with("bounce")
}

/// Причина не отвечать на письмо `raw` от отправителя конверта `mail_from` автоответом `reply` (RFC 3834 §2);
/// `None` — отвечать можно.
pub fn suppressed(mail_from: &str, raw: &[u8], reply: &AutoReply) -> Option<&'static str> {
    if mail_from.is_empty() || reply.to.is_empty() {
        return Some("empty envelope sender");
    }
    if robot(mail_from) || robot(&reply.to) {
        return Some("automated sender");
    }
    if reply.to.eq_ignore_ascii_case(&reply.from) {
        return Some("own address");
    }
    let msg = mail_parser::MessageParser::new().parse_headers(raw)?;
    for h in msg.headers() {
        let name = h.name.as_str();
        let value = raw
            .get(h.offset_start as usize..h.offset_end as usize)
            .map(|v| String::from_utf8_lossy(v).trim().to_ascii_lowercase())
            .unwrap_or_default();
        if name.eq_ignore_ascii_case("Auto-Submitted") {
            if value.split(';').next().map(str::trim) != Some("no") {
                return Some("auto-submitted message");
            }
        } else if LIST_HEADERS.iter().any(|l| name.eq_ignore_ascii_case(l)) {
            return Some("mailing list");
        } else if name.eq_ignore_ascii_case("Precedence") {
            if matches!(value.as_str(), "bulk" | "list" | "junk") {
                return Some("bulk message");
            }
        } else if name.eq_ignore_ascii_case("X-Auto-Response-Suppress")
            && value
                .split(',')
                .map(str::trim)
                .any(|v| v == "all" || v == "oof")
        {
            return Some("auto-responses suppressed by sender");
        }
    }
    None
}

/// Значение поля: без управляющих символов.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

/// `Message-ID` без угловых скобок и пробелов.
fn msg_id(s: &str) -> String {
    clean(
        s.trim()
            .trim_start_matches('<')
            .trim_end_matches('>')
            .trim(),
    )
}

/// Переводы строк — CRLF.
fn crlf(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 16);
    let mut prev = '\0';
    for c in s.chars() {
        if c == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(c);
        prev = c;
    }
    out
}

/// Письмо автоответа (RFC 3834 §3) на `mail-builder`: `From`, `To`, тема, `Auto-Submitted: auto-replied`,
/// `X-Auto-Response-Suppress: All` (автоответы Exchange на него не отвечают), `In-Reply-To`, `References`; текст —
/// `text/plain`, `multipart/alternative` или готовая MIME-сущность правил. `id` — уникальная часть `Message-ID`
/// (`vacation-<id>@<hostname>`).
pub fn build_reply(reply: &AutoReply, hostname: &str, id: &str, now: OffsetDateTime) -> Vec<u8> {
    let mut b = MessageBuilder::new()
        .from(Address::new_address(None::<&str>, clean(&reply.from)))
        .to(Address::new_address(None::<&str>, clean(&reply.to)))
        .subject(clean(&reply.subject))
        .date(Date::new(now.unix_timestamp()))
        .message_id(format!("vacation-{id}@{hostname}"))
        .header("Auto-Submitted", Raw::new("auto-replied"))
        .header("X-Auto-Response-Suppress", Raw::new("All"));
    if let Some(m) = reply
        .in_reply_to
        .as_deref()
        .map(msg_id)
        .filter(|m| !m.is_empty())
    {
        b = b.in_reply_to(m);
    }
    let refs: Vec<String> = reply
        .references
        .iter()
        .map(|r| msg_id(r))
        .filter(|r| !r.is_empty())
        .collect();
    if !refs.is_empty() {
        b = b.references(refs);
    }
    b = match &reply.body {
        ReplyBody::Text(text) => b.text_body(text.clone()),
        ReplyBody::Html { text, html } => {
            if let Some(text) = text {
                b = b.text_body(text.clone());
            }
            b.html_body(html.clone())
        }
        ReplyBody::Mime(entity) => b.body(MimePart::raw(crlf(entity))),
    };
    b.write_to_vec().unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_contracts::UidValidity;

    fn reply(body: ReplyBody) -> AutoReply {
        AutoReply {
            to: "sender@example.net".into(),
            from: "bob@example.org".into(),
            subject: "Автоответ: hello".into(),
            body,
            in_reply_to: Some("<orig-1@example.net>".into()),
            references: vec!["<root@example.net>".into(), "orig-1@example.net".into()],
        }
    }

    #[test]
    fn reply_headers_and_bodies() {
        let now = OffsetDateTime::UNIX_EPOCH;
        let raw = build_reply(
            &reply(ReplyBody::Text("В отпуске".into())),
            "mx.example.org",
            "t1",
            now,
        );
        let msg = mail_parser::MessageParser::new()
            .parse(&raw)
            .expect("parse");
        assert_eq!(
            msg.header_raw("Auto-Submitted").map(str::trim),
            Some("auto-replied")
        );
        assert_eq!(msg.in_reply_to().as_text(), Some("orig-1@example.net"));
        assert_eq!(
            msg.references().as_text_list().map(|l| l.to_vec()),
            Some(vec!["root@example.net".into(), "orig-1@example.net".into()])
        );
        assert_eq!(msg.message_id(), Some("vacation-t1@mx.example.org"));
        assert_eq!(msg.subject(), Some("Автоответ: hello"));
        assert_eq!(
            msg.body_text(0).as_deref().map(str::trim),
            Some("В отпуске")
        );

        let raw = build_reply(
            &reply(ReplyBody::Html {
                text: Some("text".into()),
                html: "<p>html</p>".into(),
            }),
            "mx.example.org",
            "t2",
            now,
        );
        let msg = mail_parser::MessageParser::new()
            .parse(&raw)
            .expect("parse");
        assert_eq!(msg.body_text(0).as_deref().map(str::trim), Some("text"));
        assert!(msg.body_html(0).is_some_and(|h| h.contains("<p>html</p>")));

        let raw = build_reply(
            &reply(ReplyBody::Mime(
                "Content-Type: text/plain; charset=utf-8\n\nготово\n".into(),
            )),
            "mx.example.org",
            "t3",
            now,
        );
        let text = String::from_utf8(raw.clone()).expect("utf8");
        assert!(
            text.contains("Content-Type: text/plain; charset=utf-8\r\n\r\nготово\r\n"),
            "{text}"
        );
        let msg = mail_parser::MessageParser::new()
            .parse(&raw)
            .expect("parse");
        assert_eq!(msg.body_text(0).as_deref().map(str::trim), Some("готово"));
    }

    #[test]
    fn loop_protection() {
        let r = reply(ReplyBody::Text("x".into()));
        let plain = b"From: sender@example.net\r\nSubject: hi\r\n\r\nbody\r\n";
        assert_eq!(suppressed("sender@example.net", plain, &r), None);
        let no = b"Auto-Submitted: no\r\nSubject: hi\r\n\r\nbody\r\n";
        assert_eq!(suppressed("sender@example.net", no, &r), None);
        assert!(suppressed("", plain, &r).is_some());
        for robot in [
            "MAILER-DAEMON@example.net",
            "owner-list@example.net",
            "list-request@example.net",
            "noreply@example.net",
        ] {
            assert!(suppressed(robot, plain, &r).is_some(), "{robot}");
        }
        for headers in [
            &b"Auto-Submitted: auto-replied\r\n\r\nx"[..],
            b"auto-submitted: Auto-Generated; owner-email=\"a@b\"\r\n\r\nx",
            b"List-Id: <team.example.net>\r\n\r\nx",
            b"List-Unsubscribe: <mailto:u@example.net>\r\n\r\nx",
            b"Precedence: bulk\r\n\r\nx",
            b"X-Auto-Response-Suppress: DR, OOF\r\n\r\nx",
        ] {
            assert!(
                suppressed("sender@example.net", headers, &r).is_some(),
                "{}",
                String::from_utf8_lossy(headers)
            );
        }
        let own = AutoReply {
            to: "bob@example.org".into(),
            ..r
        };
        assert!(suppressed("bob@example.org", plain, &own).is_some());
    }

    #[test]
    fn rate_per_account() {
        let rate = VacationRate::new(2);
        let (a, b) = (AccountId::new(), AccountId::new());
        let t = Instant::now();
        assert!(rate.allow_at(a, t));
        assert!(rate.allow_at(a, t));
        assert!(!rate.allow_at(a, t + Duration::from_secs(60)));
        assert!(rate.allow_at(b, t));
        assert!(rate.allow_at(a, t + HOUR));
        assert!(!VacationRate::new(0).allow_at(a, t));
    }

    #[test]
    fn mailbox_paths() {
        let mb = |id: MailboxId, parent, name: &str, role| Mailbox {
            id,
            parent,
            name: name.into(),
            role,
            uid_validity: UidValidity(1),
            subscribed: true,
        };
        let (inbox, work, sub) = (MailboxId::new(), MailboxId::new(), MailboxId::new());
        let list = vec![
            mb(inbox, None, "Входящие", Some(SpecialUse::Inbox)),
            mb(work, None, "Work", None),
            mb(sub, Some(work), "2026", None),
        ];
        assert_eq!(find_path(&list, "Work"), Some(work));
        assert_eq!(find_path(&list, "Work/2026"), Some(sub));
        assert_eq!(find_path(&list, "/Work//2026/"), Some(sub));
        assert_eq!(find_path(&list, "inbox"), Some(inbox));
        assert_eq!(find_path(&list, "Work/2025"), None);
        assert_eq!(find_path(&list, "work"), None);
        assert_eq!(find_path(&list, ""), None);
    }
}
