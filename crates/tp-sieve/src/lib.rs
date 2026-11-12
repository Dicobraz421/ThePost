//! Интерпретатор Sieve (RFC 5228 и расширения), ManageSieve (SRS 4.3).
//!
//! Своя реализация: sieve-rs распространяется под AGPL-3.0 (SRS 4.3, 7.1). Путь скрипта:
//! [`parse`] (лексер и грамматика RFC 5228 §8, SIEVE-01) → [`check`] / [`compile`] (require, аргументы, расширения —
//! программа [`Program`], SIEVE-02) → [`run`] (выполнение над письмом, SIEVE-03) → слияние действий в
//! `RulesOutcome` (SIEVE-04). Точка правил доставки — [`Rules`] (SIEVE-07): скрипт организации, автоответ
//! (SIEVE-06), активный скрипт пользователя; секция конфигурации — [`SieveConfig`]. [`fuzz`] — проверки для целей
//! cargo-fuzz `crates/tp-sieve/fuzz` (не API).
//!
//! Все лимиты ([`Limits`]) дают ошибку разбора или выполнения, а не панику: скрипты пишут пользователи.

pub mod ast;
mod compile;
mod config;
pub mod fuzz;
mod lexer;
mod outcome;
mod rules;
mod run;
mod vacation;

use std::fmt;

use serde::{Deserialize, Serialize};

pub use ast::{Arg, Command, Script, Test, parse};
pub use compile::{EXTENSIONS, Program, check, compile};
pub use config::{CacheConfig, ManageSieveConfig, SieveConfig, VacationConfig};
pub use rules::Rules;
pub use run::{Action, Context, Outcome, VacationAction, run};

/// Лимиты разбора и выполнения; в приложении — из секции `sieve` ([`SieveConfig::limits`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Размер текста скрипта, байт.
    pub max_script_size: usize,
    /// Глубина вложенности блоков и тестов.
    pub max_nesting: usize,
    /// Элементов в списке строк или тестов.
    pub max_list_len: usize,
    /// Команд и тестов в скрипте.
    pub max_commands: usize,
    /// Действий за одно выполнение (`keep`, `fileinto`, `redirect`, `vacation`, `discard`).
    pub max_actions: usize,
    /// Шагов выполнения (команды, тесты, сравнения строк).
    pub max_steps: u64,
    /// Перенаправлений (`redirect`) за одно выполнение; больше — ошибка выполнения.
    pub max_redirects: usize,
    /// Объём текста письма, который читает тест `body`, байт (остальное не проверяется).
    pub max_body_scan: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_script_size: 128 * 1024,
            max_nesting: 32,
            max_list_len: 1024,
            max_commands: 4096,
            max_actions: 256,
            max_steps: 10_000_000,
            max_redirects: 4,
            max_body_scan: 1024 * 1024,
        }
    }
}

/// Позиция в тексте скрипта: строка и столбец (с 1, столбец — в символах).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Pos {
    /// Строка.
    pub line: u32,
    /// Столбец.
    pub column: u32,
}

impl Pos {
    /// Начало текста.
    pub const START: Pos = Pos { line: 1, column: 1 };
}

/// Ошибка в скрипте с позицией — для ManageSieve `CHECKSCRIPT`/`PUTSCRIPT`, JMAP `SieveScript/validate`, Admin API.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// Строка (с 1).
    pub line: u32,
    /// Столбец (с 1, в символах).
    pub column: u32,
    /// Описание.
    pub message: String,
}

impl Diagnostic {
    pub(crate) fn new(pos: Pos, message: impl Into<String>) -> Self {
        Self {
            line: pos.line,
            column: pos.column,
            message: message.into(),
        }
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "строка {}, столбец {}: {}",
            self.line, self.column, self.message
        )
    }
}

impl std::error::Error for Diagnostic {}

#[cfg(test)]
mod tests;
