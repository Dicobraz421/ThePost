//! Слияние действий скриптов в [`RulesOutcome`] (SIEVE-04; RFC 5228 §2.10).
//!
//! Скрипты одной доставки (организации, затем пользователя) складываются так, будто записаны подряд: действия
//! копятся, implicit keep остаётся, только если его не отменил ни один скрипт. Повтор папки — одна доставка
//! (флаги объединяются), повтор адреса `redirect` — одно перенаправление. Ошибка скрипта отменяет его действия и
//! оставляет письмо во «Входящих». `fileinto` в папку, которой нет, без `:create` — «Входящие» и запись в `errors`.
//!
//! Имя папки — как в IMAP: путь от корня через `/`, `INBOX` в любом регистре — «Входящие», остальные имена
//! сравниваются побайтно (UTF-8).

use tp_contracts::MailboxId;
use tp_contracts::meta::{Flag, Flags, Mailbox, SpecialUse};
use tp_contracts::sieve::{KeepTo, MailboxRef, RulesOutcome};

use crate::run::{Action, Outcome, VacationAction};

/// Глубина иерархии папок при построении пути (защита от цикла в данных).
const MAX_PATH_DEPTH: usize = 64;

/// Накопитель действий скриптов одной доставки.
#[derive(Debug)]
pub(crate) struct Merge<'a> {
    mailboxes: &'a [Mailbox],
    keep: Vec<KeepTo>,
    redirect: Vec<String>,
    discarded: bool,
    /// Ни один скрипт не отменил implicit keep.
    implicit: bool,
    /// Письмо обязательно остаётся во «Входящих» (ошибка скрипта, отменённое перенаправление).
    forced: bool,
    /// Флаги implicit keep — переменная `imap4flags` последнего выполненного скрипта.
    flags: Vec<String>,
    errors: Vec<String>,
    vacation: Option<VacationAction>,
}

impl<'a> Merge<'a> {
    pub(crate) fn new(mailboxes: &'a [Mailbox]) -> Self {
        Self {
            mailboxes,
            keep: Vec::new(),
            redirect: Vec::new(),
            discarded: false,
            implicit: true,
            forced: false,
            flags: Vec::new(),
            errors: Vec::new(),
            vacation: None,
        }
    }

    /// Ошибка скрипта `what` (не скомпилирован, сбой выполнения): письмо остаётся во «Входящих».
    pub(crate) fn fail(&mut self, what: &str, error: &str) {
        self.errors.push(format!("{what}: {error}"));
        self.forced = true;
    }

    /// Добавляет итог выполнения скрипта `what` («скрипт организации», «скрипт пользователя»).
    pub(crate) fn add(&mut self, what: &str, outcome: Outcome) {
        if let Some(e) = &outcome.error {
            self.fail(what, e);
            return;
        }
        for action in outcome.actions {
            match action {
                Action::Keep { flags } => self.keep_to(MailboxRef::Inbox, &flags),
                Action::Discard => self.discarded = true,
                Action::FileInto {
                    mailbox,
                    create,
                    flags,
                    ..
                } => match resolve(self.mailboxes, &mailbox) {
                    Resolved::Found(target) => self.keep_to(target, &flags),
                    Resolved::Missing(name) if create => {
                        self.keep_to(MailboxRef::Name { name, create: true }, &flags);
                    }
                    Resolved::Missing(_) => {
                        self.errors.push(format!(
                            "{what}: fileinto: папки {mailbox:?} нет, письмо оставлено во «Входящих»"
                        ));
                        self.keep_to(MailboxRef::Inbox, &flags);
                    }
                    Resolved::Invalid => {
                        self.errors.push(format!(
                            "{what}: fileinto: недопустимое имя папки {mailbox:?}, письмо оставлено во «Входящих»"
                        ));
                        self.keep_to(MailboxRef::Inbox, &flags);
                    }
                },
                Action::Redirect { address, .. } => {
                    if !self
                        .redirect
                        .iter()
                        .any(|a| a.eq_ignore_ascii_case(&address))
                    {
                        self.redirect.push(address);
                    }
                }
                Action::Vacation(v) => {
                    if self.vacation.is_none() {
                        self.vacation = Some(v);
                    }
                }
            }
        }
        self.implicit &= outcome.implicit_keep;
        self.flags = outcome.flags;
    }

    /// Есть перенаправления.
    pub(crate) fn redirects(&self) -> bool {
        !self.redirect.is_empty()
    }

    /// Отменяет перенаправления (защита от петли): письмо остаётся во «Входящих», причина — в `errors`.
    pub(crate) fn cancel_redirects(&mut self, reason: &str) {
        if self.redirects() {
            self.redirect.clear();
            self.errors.push(reason.to_owned());
            self.forced = true;
        }
    }

    /// Итог правил (без автоответа) и параметры `vacation` из скрипта.
    pub(crate) fn finish(mut self) -> (RulesOutcome, Option<VacationAction>) {
        if self.implicit {
            let flags = std::mem::take(&mut self.flags);
            self.keep_to(MailboxRef::Inbox, &flags);
        } else if self.forced {
            self.keep_to(MailboxRef::Inbox, &[]);
        }
        let discard = self.discarded && self.keep.is_empty();
        (
            RulesOutcome {
                keep: self.keep,
                redirect: self.redirect,
                discard,
                vacation: None,
                errors: self.errors,
            },
            self.vacation,
        )
    }

    fn keep_to(&mut self, mailbox: MailboxRef, flags: &[String]) {
        let flags: Flags = flags.iter().filter_map(|f| flag(f)).collect();
        match self.keep.iter_mut().find(|k| k.mailbox == mailbox) {
            Some(k) => k.flags.extend(flags),
            None => self.keep.push(KeepTo { mailbox, flags }),
        }
    }
}

/// Итог одного скрипта как [`RulesOutcome`] (без автоответа).
pub(crate) fn merge(outcome: Outcome, mailboxes: &[Mailbox]) -> RulesOutcome {
    let mut m = Merge::new(mailboxes);
    m.add("скрипт", outcome);
    m.finish().0
}

enum Resolved {
    Found(MailboxRef),
    /// Папки нет; путь без пустых частей.
    Missing(String),
    Invalid,
}

fn is_inbox(name: &str) -> bool {
    name.eq_ignore_ascii_case("INBOX")
}

fn resolve(list: &[Mailbox], path: &str) -> Resolved {
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    if segments.is_empty() || path.chars().any(char::is_control) {
        return Resolved::Invalid;
    }
    if let [one] = segments.as_slice()
        && is_inbox(one)
    {
        return Resolved::Found(MailboxRef::Inbox);
    }
    let mut parent: Option<&Mailbox> = None;
    for seg in &segments {
        let pid = parent.map(|m| m.id);
        let next = list.iter().find(|m| {
            m.parent == pid
                && (m.name == *seg
                    || (pid.is_none() && m.role == Some(SpecialUse::Inbox) && is_inbox(seg)))
        });
        match next {
            Some(m) => parent = Some(m),
            None => return Resolved::Missing(segments.join("/")),
        }
    }
    match parent {
        Some(m) if m.role == Some(SpecialUse::Inbox) => Resolved::Found(MailboxRef::Inbox),
        Some(m) => Resolved::Found(MailboxRef::Id(m.id)),
        None => Resolved::Invalid,
    }
}

/// Пути папок учётной записи (имена от корня через `/`) для `mailboxexists`.
pub(crate) fn mailbox_paths(list: &[Mailbox]) -> Vec<String> {
    list.iter()
        .filter_map(|m| {
            let mut names = vec![m.name.as_str()];
            let mut parent: Option<MailboxId> = m.parent;
            while let Some(pid) = parent {
                if names.len() >= MAX_PATH_DEPTH {
                    return None;
                }
                let p = list.iter().find(|x| x.id == pid)?;
                names.push(p.name.as_str());
                parent = p.parent;
            }
            names.reverse();
            Some(names.join("/"))
        })
        .collect()
}

/// Флаг IMAP из скрипта: системные — без учёта регистра, неизвестные системные (`\Recent`) и недопустимые ключевые
/// слова отбрасываются.
fn flag(s: &str) -> Option<Flag> {
    let lower = s.to_ascii_lowercase();
    Some(match lower.as_str() {
        "\\seen" | "$seen" => Flag::Seen,
        "\\answered" | "$answered" => Flag::Answered,
        "\\flagged" | "$flagged" => Flag::Flagged,
        "\\deleted" => Flag::Deleted,
        "\\draft" | "$draft" => Flag::Draft,
        _ => {
            let atom = !s.is_empty()
                && s.bytes().all(|b| {
                    (33..=126).contains(&b)
                        && !matches!(b, b'(' | b')' | b'{' | b'%' | b'*' | b'"' | b'\\' | b']')
                });
            if !atom {
                return None;
            }
            Flag::Keyword(s.to_owned())
        }
    })
}
