//! Тесты tp-sieve: разбор (SIEVE-01), проверка (SIEVE-02), выполнение (SIEVE-03), действия (SIEVE-04), тесты расширений (SIEVE-05), автоответ (SIEVE-06), правила доставки (SIEVE-07).

mod actions;
mod checking;
mod execution;
mod extensions;
mod parsing;
mod rules;
mod vacation;

use crate::{Diagnostic, Limits};

/// Первая ошибка проверки скрипта.
fn first_error(text: &str) -> Diagnostic {
    match crate::compile(text.as_bytes(), &Limits::default()) {
        Ok(_) => panic!("скрипт прошёл проверку: {text}"),
        Err(d) => d.into_iter().next().expect("ошибка"),
    }
}

/// Проверяет первую ошибку: строка, столбец и часть сообщения.
#[track_caller]
fn assert_error(text: &str, line: u32, column: u32, message: &str) {
    let d = first_error(text);
    assert_eq!((d.line, d.column), (line, column), "{text:?}: {d}");
    assert!(d.message.contains(message), "{text:?}: {d}");
}

/// Папки учётной записи для тестов: `INBOX`, `Work`, `Work/Reports`, `Архив`.
fn mailboxes() -> Vec<tp_contracts::meta::Mailbox> {
    use tp_contracts::meta::{Mailbox, SpecialUse};
    let mb = |parent, name: &str, role| Mailbox {
        id: tp_contracts::MailboxId::new(),
        parent,
        name: name.to_owned(),
        role,
        uid_validity: tp_contracts::UidValidity(1),
        subscribed: true,
    };
    let work = mb(None, "Work", None);
    vec![
        mb(None, "INBOX", Some(SpecialUse::Inbox)),
        mb(Some(work.id), "Reports", None),
        work,
        mb(None, "Архив", Some(SpecialUse::Archive)),
    ]
}
