//! JMAP Core, Mail, Submission, Push, Sieve, Calendars, Contacts (SRS 4.2–4.4).
//!
//! - [`core`] — сессия, запрос API, ссылки на результаты, ошибки, лимиты, аутентификация, capability модулей (RFC 8620);
//! - `mailbox` — Mailbox/get, changes, query, queryChanges, set (RFC 8621 §2);
//! - `email_get` — Email/get, Email/changes, Thread/get, Thread/changes (RFC 8621 §3, §4.2–4.3);
//! - `email_query` — Email/query, Email/queryChanges: фильтры, сортировка, collapseThreads, текст через `SearchIndex`.
//!
//! Протокол — JSON поверх HTTP: типы запросов и ответов — `serde_json`, HTTP — `axum`.

pub mod core;
mod email_get;
mod email_query;
mod mailbox;
#[cfg(test)]
mod testenv;

use serde_json::{Map, Value};

pub use crate::core::{Deps, Jmap, JmapConfig, MethodError, Problem};

use crate::core::error::MethodResult;
use crate::core::{CORE, Ctx, MAIL};

/// Встроенные методы и их capability.
pub(crate) const METHODS: &[(&str, &str)] = &[
    ("Core/echo", CORE),
    ("Mailbox/get", MAIL),
    ("Mailbox/changes", MAIL),
    ("Mailbox/query", MAIL),
    ("Mailbox/queryChanges", MAIL),
    ("Mailbox/set", MAIL),
    ("Email/get", MAIL),
    ("Email/changes", MAIL),
    ("Thread/get", MAIL),
    ("Thread/changes", MAIL),
    ("Email/query", MAIL),
    ("Email/queryChanges", MAIL),
];

/// Варианты сортировки `Email/query` (сессия, `emailQuerySortOptions`).
pub(crate) const EMAIL_SORT_OPTIONS: &[&str] = email_query::SORT_OPTIONS;

/// Выполняет встроенный метод из [`METHODS`].
pub(crate) async fn call_builtin(
    ctx: &mut Ctx<'_>,
    name: &str,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    match name {
        "Core/echo" => Ok(args),
        "Mailbox/get" => mailbox::get(ctx, args).await,
        "Mailbox/changes" => mailbox::changes(ctx, args).await,
        "Mailbox/query" => mailbox::query(ctx, args).await,
        "Mailbox/queryChanges" => mailbox::query_changes(ctx, args).await,
        "Mailbox/set" => mailbox::set(ctx, args).await,
        "Email/get" => email_get::get(ctx, args).await,
        "Email/changes" => email_get::changes(ctx, args).await,
        "Thread/get" => email_get::thread_get(ctx, args).await,
        "Thread/changes" => email_get::thread_changes(ctx, args).await,
        "Email/query" => email_query::query(ctx, args).await,
        "Email/queryChanges" => email_query::query_changes(ctx, args).await,
        _ => Err(MethodError::new("unknownMethod")),
    }
}
