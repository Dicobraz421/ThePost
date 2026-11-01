//! JMAP Core, Mail, Submission, Push, Sieve, Calendars, Contacts (SRS 4.2–4.4).
//!
//! - [`core`] — сессия, запрос API, ссылки на результаты, ошибки, лимиты, аутентификация, capability модулей (RFC 8620);
//! - `mailbox` — Mailbox/get, changes, query, queryChanges, set (RFC 8621 §2);
//! - `email_get` — Email/get, Email/changes, Thread/get, Thread/changes (RFC 8621 §3, §4.2–4.3);
//! - `email_query` — Email/query, Email/queryChanges: фильтры, сортировка, collapseThreads, текст через `SearchIndex`;
//!   SearchSnippet/get (RFC 8621 §5);
//! - `email_set` — Email/set (черновики, keywords, mailboxIds, удаление в корзину), Email/import, Email/copy (§4.6–4.8);
//! - `blob` — загрузка и скачивание blob, Blob/copy (RFC 8620 §6);
//! - [`submission`] — Identity/get, set и EmailSubmission/get, set → очередь [`Outbound`] (RFC 8621 §6–7);
//! - `push` — EventSource и WebSocket (RFC 8620 §7, RFC 8887) по шине уведомлений.
//!
//! Протокол — JSON поверх HTTP: типы запросов и ответов — `serde_json`, HTTP — `axum`.

mod blob;
pub mod core;
mod email_get;
mod email_query;
mod email_set;
mod mailbox;
mod push;
pub mod submission;
#[cfg(test)]
mod testenv;

use serde_json::{Map, Value};

pub use crate::core::{Deps, Jmap, JmapConfig, MethodError, Problem};
pub use crate::submission::Outbound;

use crate::core::error::MethodResult;
use crate::core::{CORE, Ctx, MAIL, SUBMISSION};

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
    ("Email/set", MAIL),
    ("Email/import", MAIL),
    ("Email/copy", MAIL),
    ("SearchSnippet/get", MAIL),
    ("Blob/copy", CORE),
    ("Identity/get", SUBMISSION),
    ("Identity/set", SUBMISSION),
    ("EmailSubmission/get", SUBMISSION),
    ("EmailSubmission/set", SUBMISSION),
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
        "Email/set" => email_set::set(ctx, args).await,
        "Email/import" => email_set::import(ctx, args).await,
        "Email/copy" => email_set::copy(ctx, args).await,
        "SearchSnippet/get" => email_query::snippet_get(ctx, args).await,
        "Blob/copy" => blob::copy(ctx, args).await,
        "Identity/get" => submission::identity_get(ctx, args).await,
        "Identity/set" => submission::identity_set(ctx, args).await,
        "EmailSubmission/get" => submission::submission_get(ctx, args).await,
        "EmailSubmission/set" => submission::submission_set(ctx, args).await,
        _ => Err(MethodError::new("unknownMethod")),
    }
}
