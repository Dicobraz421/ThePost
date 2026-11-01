//! JMAP Core, Mail, Submission, Push, Sieve, Calendars, Contacts (SRS 4.2–4.4).
//!
//! - [`core`] — сессия, запрос API, ссылки на результаты, ошибки, лимиты, аутентификация, capability модулей (RFC 8620).
//!
//! Протокол — JSON поверх HTTP: типы запросов и ответов — `serde_json`, HTTP — `axum`.

pub mod core;
#[cfg(test)]
mod testenv;

use serde_json::{Map, Value};

pub use crate::core::{Deps, Jmap, JmapConfig, MethodError, Problem};

use crate::core::error::MethodResult;
use crate::core::{CORE, Ctx};

/// Встроенные методы и их capability.
pub(crate) const METHODS: &[(&str, &str)] = &[("Core/echo", CORE)];

/// Варианты сортировки `Email/query` (сессия, `emailQuerySortOptions`).
pub(crate) const EMAIL_SORT_OPTIONS: &[&str] = &[];

/// Выполняет встроенный метод из [`METHODS`].
pub(crate) async fn call_builtin(
    _ctx: &mut Ctx<'_>,
    name: &str,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    match name {
        "Core/echo" => Ok(args),
        _ => Err(MethodError::new("unknownMethod")),
    }
}
