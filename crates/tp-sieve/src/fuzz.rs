//! Проверки разбора и выполнения Sieve на произвольных данных (SIEVE-07; SRS 6.8): цели cargo-fuzz
//! `crates/tp-sieve/fuzz` и тесты крейта на примерах. Функция паникует, только если нарушен инвариант.
//!
//! - [`parse`] — текст скрипта: разбор и проверка заканчиваются программой или ошибками с позициями.
//! - [`run`] — скрипт и письмо (через байт NUL): выполнение укладывается в лимиты, итог согласован.

use crate::outcome::merge;
use crate::{Context, EXTENSIONS, Limits, compile};

/// Лимиты в целях (меньше конфигурации по умолчанию: важна граница, не значение).
const LIMITS: Limits = Limits {
    max_script_size: 16 * 1024,
    max_nesting: 16,
    max_list_len: 64,
    max_commands: 512,
    max_actions: 16,
    max_steps: 50_000,
    max_redirects: 2,
    max_body_scan: 4 * 1024,
};

/// Текст скрипта.
pub fn parse(data: &[u8]) {
    let ast = crate::parse(data, &LIMITS);
    match compile(data, &LIMITS) {
        Ok(program) => {
            assert!(ast.is_ok(), "программа без разбора");
            assert!(data.len() <= LIMITS.max_script_size, "скрипт больше лимита");
            for e in program.extensions() {
                assert!(
                    EXTENSIONS.contains(&e.as_str()),
                    "неизвестное расширение {e}"
                );
            }
        }
        Err(diags) => {
            assert!(!diags.is_empty(), "ошибка без диагностики");
            for d in &diags {
                assert!(d.line >= 1 && d.column >= 1, "позиция с нуля: {d}");
                assert!(!d.message.is_empty(), "пустая диагностика");
            }
        }
    }
}

/// Скрипт и письмо: до первого байта NUL — скрипт, после — письмо.
pub fn run(data: &[u8]) {
    let mut parts = data.splitn(2, |&b| b == 0);
    let script = parts.next().unwrap_or_default();
    let message = parts.next().unwrap_or_default();
    let Ok(program) = compile(script, &LIMITS) else {
        return;
    };
    let mailboxes = ["INBOX".to_owned(), "Work".to_owned()];
    let ctx = Context {
        message,
        envelope_from: "alice@example.com",
        envelope_to: "bob+lists@example.org",
        mailboxes: &mailboxes,
    };
    let o = crate::run(&program, &ctx, &LIMITS);
    assert!(
        o.actions.len() <= LIMITS.max_actions,
        "действий больше лимита"
    );
    if o.error.is_some() {
        assert!(
            o.actions.is_empty() && o.implicit_keep,
            "ошибка без implicit keep"
        );
    }
    let implicit = o.implicit_keep;
    let rules = merge(o, &[]);
    assert!(
        rules.redirect.len() <= LIMITS.max_redirects,
        "перенаправлений больше лимита"
    );
    assert!(
        !implicit || !rules.keep.is_empty(),
        "implicit keep без доставки"
    );
    assert!(
        !rules.discard || rules.keep.is_empty(),
        "discard с доставкой"
    );
    assert!(rules.vacation.is_none());
}
