//! Тесты tp-sieve: разбор (SIEVE-01), проверка (SIEVE-02), выполнение (SIEVE-03).

mod checking;
mod execution;
mod parsing;

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
