//! JMAP Core, Mail, Submission, Push, Sieve, Calendars, Contacts (SRS 4.2–4.4).
//!
//! - [`core`] — сессия, запрос API, ссылки на результаты, ошибки, лимиты, аутентификация, capability модулей (RFC 8620);
//! - `mailbox` — Mailbox/get, changes, query, queryChanges, set (RFC 8621 §2);
//! - `email_get` — Email/get, Email/changes, Thread/get, Thread/changes (RFC 8621 §3, §4.2–4.3);
//! - `email_query` — Email/query, Email/queryChanges: фильтры, сортировка, collapseThreads, текст через `SearchIndex`;
//!   SearchSnippet/get (RFC 8621 §5);
//! - `email_set` — Email/set (черновики, keywords, mailboxIds, удаление в корзину), Email/import, Email/copy (§4.6–4.8);
//! - `blob` — загрузка и скачивание blob, Blob/copy (RFC 8620 §6);
//! - [`submission`] — Identity/get, set и EmailSubmission/get, changes, query, queryChanges, set: отправка сразу и
//!   отложенная (`sendAt`, отмена) через [`Outbound`](tp_contracts::queue::Outbound), записи — `SubmissionStore` (RFC 8621 §6–7);
//! - `push` — EventSource и WebSocket (RFC 8620 §7, RFC 8887) по шине уведомлений;
//! - `fuzz` — проверки разбора для целей cargo-fuzz `crates/tp-jmap/fuzz` (JMAP-09; не API);
//! - `audit` — запись действий с ящиком в `MailboxAudit` (JMAP-18): отправка от чужого адреса, по политике — удаление,
//!   перенос, отправка и изменение правил владельцем;
//! - `autoconfig` — автонастройка клиентов: `/.well-known/caldav`, `/.well-known/carddav`, Thunderbird, `.mobileconfig`
//!   (подпись профиля ключом TLS — [`ProfileSigner`], [`Jmap::with_profile_signer`]);
//! - `vacation` — VacationResponse/get, set: автоответ (RFC 8621 §8) поверх `SieveStore`, [`Jmap::with_sieve`];
//! - `calendar_event` — CalendarEvent/get, changes, set, copy: iCalendar в `DavStore`, JSCalendar — calcard,
//!   индекс — `tp_dav::index`;
//! - `calendar_query` — CalendarEvent/query, queryChanges: интервал по индексу и развёртке повторов calcard,
//!   `expandRecurrences`;
//! - `calendars` — Calendar/get, changes, set (draft-ietf-jmap-calendars) поверх `DavStore`, [`Jmap::with_dav`];
//! - `contact_card` — ContactCard/get, changes, query, queryChanges, set, copy: vCard в `DavStore`, JSContact —
//!   calcard, индекс — `tp_dav::index`;
//! - `contacts` — AddressBook/get, changes, set (RFC 9610) поверх `DavStore`, [`Jmap::with_dav`];
//! - `principal` — Principal/get, query (RFC 9670) и Principal/getAvailability поверх `AccountStore` и `FreeBusy`,
//!   [`Jmap::with_freebusy`];
//! - `scheduling` — приглашения (JMAP-31): рассылка iTIP после `CalendarEvent/set` с `sendSchedulingMessages`
//!   (`tp_dav::itip`, [`Jmap::with_schedule`]), ParticipantIdentity/get, CalendarEventNotification/get, changes, set
//!   по входящим планирования;
//! - `sieve` — SieveScript/get, set, query, validate (RFC 9661) поверх `SieveStore` и проверки `tp_sieve`;
//! - `sender_lists` — SenderList/get, set: личные списки надёжных и заблокированных отправителей, capability
//!   `urn:thepost:senderlists`, [`Jmap::with_sender_lists`];
//! - `orgchart` — OrgUnit/get: дерево подразделений с членами по старшинству, capability `urn:thepost:orgchart`,
//!   [`Jmap::with_org`];
//! - `settings` — UserSettings/get, set: настройки пользователя (`trash.purge`), capability `urn:thepost:params:jmap:settings`.
//!
//! Протокол — JSON поверх HTTP: типы запросов и ответов — `serde_json`, HTTP — `axum`.

mod audit;
mod autoconfig;
mod blob;
mod calendar_event;
mod calendar_query;
mod calendars;
mod contact_card;
mod contacts;
pub mod core;
mod email_get;
mod email_query;
mod email_set;
#[doc(hidden)]
pub mod fuzz;
mod mailbox;
mod orgchart;
mod principal;
mod push;
mod scheduling;
mod sender_lists;
mod settings;
mod sieve;
pub mod submission;
#[cfg(test)]
mod testenv;
mod vacation;

use serde_json::{Map, Value};

pub use crate::autoconfig::sign::{ProfileSigner, SignatureAlgorithm, SigningKey};
pub use crate::core::{Deps, Jmap, JmapConfig, MethodError, Problem};

use crate::core::error::MethodResult;
use crate::core::{
    AVAILABILITY, CALENDARS, CONTACTS, CORE, Ctx, MAIL, ORG_CHART, PRINCIPALS, SENDER_LISTS,
    SETTINGS, SIEVE, SUBMISSION, VACATION,
};

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
    ("EmailSubmission/changes", SUBMISSION),
    ("EmailSubmission/set", SUBMISSION),
    ("EmailSubmission/query", SUBMISSION),
    ("EmailSubmission/queryChanges", SUBMISSION),
    ("UserSettings/get", SETTINGS),
    ("UserSettings/set", SETTINGS),
    ("SenderList/get", SENDER_LISTS),
    ("SenderList/set", SENDER_LISTS),
    ("OrgUnit/get", ORG_CHART),
    ("VacationResponse/get", VACATION),
    ("VacationResponse/set", VACATION),
    ("SieveScript/get", SIEVE),
    ("SieveScript/set", SIEVE),
    ("SieveScript/query", SIEVE),
    ("SieveScript/validate", SIEVE),
    ("AddressBook/get", CONTACTS),
    ("AddressBook/changes", CONTACTS),
    ("AddressBook/set", CONTACTS),
    ("ContactCard/get", CONTACTS),
    ("ContactCard/changes", CONTACTS),
    ("ContactCard/query", CONTACTS),
    ("ContactCard/queryChanges", CONTACTS),
    ("ContactCard/set", CONTACTS),
    ("ContactCard/copy", CONTACTS),
    ("Principal/get", PRINCIPALS),
    ("Principal/query", PRINCIPALS),
    ("Principal/getAvailability", AVAILABILITY),
    ("Calendar/get", CALENDARS),
    ("Calendar/changes", CALENDARS),
    ("Calendar/set", CALENDARS),
    ("CalendarEvent/get", CALENDARS),
    ("CalendarEvent/changes", CALENDARS),
    ("CalendarEvent/query", CALENDARS),
    ("CalendarEvent/queryChanges", CALENDARS),
    ("CalendarEvent/set", CALENDARS),
    ("CalendarEvent/copy", CALENDARS),
    ("ParticipantIdentity/get", CALENDARS),
    ("CalendarEventNotification/get", CALENDARS),
    ("CalendarEventNotification/changes", CALENDARS),
    ("CalendarEventNotification/set", CALENDARS),
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
        "EmailSubmission/changes" => submission::submission_changes(ctx, args).await,
        "EmailSubmission/set" => submission::submission_set(ctx, args).await,
        "EmailSubmission/query" => submission::submission_query(ctx, args).await,
        "EmailSubmission/queryChanges" => submission::submission_query_changes(ctx, args).await,
        "UserSettings/get" => settings::get(ctx, args).await,
        "UserSettings/set" => settings::set(ctx, args).await,
        "SenderList/get" => sender_lists::get(ctx, args).await,
        "SenderList/set" => sender_lists::set(ctx, args).await,
        "OrgUnit/get" => orgchart::get(ctx, args).await,
        "VacationResponse/get" => vacation::get(ctx, args).await,
        "VacationResponse/set" => vacation::set(ctx, args).await,
        "SieveScript/get" => sieve::get(ctx, args).await,
        "SieveScript/set" => sieve::set(ctx, args).await,
        "SieveScript/query" => sieve::query(ctx, args).await,
        "SieveScript/validate" => sieve::validate(ctx, args).await,
        "AddressBook/get" => contacts::get(ctx, args).await,
        "AddressBook/changes" => contacts::changes(ctx, args).await,
        "AddressBook/set" => contacts::set(ctx, args).await,
        "ContactCard/get" => contact_card::get(ctx, args).await,
        "ContactCard/changes" => contact_card::changes(ctx, args).await,
        "ContactCard/query" => contact_card::query(ctx, args).await,
        "ContactCard/queryChanges" => contact_card::query_changes(ctx, args).await,
        "ContactCard/set" => contact_card::set(ctx, args).await,
        "ContactCard/copy" => contact_card::copy(ctx, args).await,
        "Principal/get" => principal::get(ctx, args).await,
        "Principal/query" => principal::query(ctx, args).await,
        "Principal/getAvailability" => principal::get_availability(ctx, args).await,
        "Calendar/get" => calendars::get(ctx, args).await,
        "Calendar/changes" => calendars::changes(ctx, args).await,
        "Calendar/set" => calendars::set(ctx, args).await,
        "CalendarEvent/get" => calendar_event::get(ctx, args).await,
        "CalendarEvent/changes" => calendar_event::changes(ctx, args).await,
        "CalendarEvent/query" => calendar_query::query(ctx, args).await,
        "CalendarEvent/queryChanges" => calendar_query::query_changes(ctx, args).await,
        "CalendarEvent/set" => calendar_event::set(ctx, args).await,
        "CalendarEvent/copy" => calendar_event::copy(ctx, args).await,
        "ParticipantIdentity/get" => scheduling::identity_get(ctx, args).await,
        "CalendarEventNotification/get" => scheduling::notification_get(ctx, args).await,
        "CalendarEventNotification/changes" => scheduling::notification_changes(ctx, args).await,
        "CalendarEventNotification/set" => scheduling::notification_set(ctx, args).await,
        _ => Err(MethodError::new("unknownMethod")),
    }
}
