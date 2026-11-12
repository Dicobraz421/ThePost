//! Admin REST API: ящики, политики, экспорт/импорт, корзина, Legal Hold, аудит (SRS 5.2, 5.3).
//!
//! - [`server`] — каркас: HTTP (`axum`), OpenAPI (`utoipa`), вход Bearer OIDC, роли администраторов с областью,
//!   маршруты модулей, ошибки;
//! - [`audit`] — журнал аудита изменений и его чтение;
//! - [`export`] — выгрузка журнала аудита из базы в SIEM: syslog (RFC 5424 поверх TLS) и OTLP;
//! - [`delivery`] — поиск в журнале доставки;
//! - [`groups`] — группы рассылки: ограничения, скрытие из GAL, очередь модерации;
//! - [`accounts`] — организация, домены, учётные записи, квоты и политики уровней;
//! - [`retention`] — корзина ящика: список, восстановление, срок по политике, `trash.purge` и скрытые письма;
//! - `content` — тема и отправитель писем корзины и очереди модерации по `include=subject` с записью в аудит;
//! - [`app_passwords`] — пароли приложений: выпуск (значение один раз), список, отзыв;
//! - [`transport`] — настройки транспорта: принятые домены, доверенные источники, политики доменов назначения;
//! - [`directory`] — адресная книга: почтовые контакты, дерево подразделений, фильтры динамических групп;
//! - [`mailboxes`] — списки отправителей ящика, отключённые ящики, остановка синхронизации, аудит доступа к ящикам;
//! - [`sieve`] — правила Sieve и автоответ учётной записи, скрипт организации: проверка текста до записи;
//! - `cors` — CORS `/api/v1/*` для консоли на отдельном веб-сервере (`admin.console.cors_origins`);
//! - [`console`] — веб-консоль: вшитая статика `console/dist` (фича `console`), маршруты SPA, заголовки безопасности;
//! - [`init`] — первый администратор без OIDC (`thepost admin init`): локальный сокет, одноразовый токен, сессия.

pub mod accounts;
pub mod app_passwords;
pub mod audit;
pub mod console;
mod content;
mod cors;
pub mod delivery;
pub mod directory;
pub mod export;
pub mod groups;
pub mod init;
pub mod mailboxes;
pub mod retention;
pub mod server;
pub mod sieve;
#[cfg(test)]
mod testenv;
pub mod transport;

pub use audit::{AuditEntry, AuditPage, AuditRecord};
pub use console::ConsoleConfig;
pub use export::{
    AuditExportConfig, ExportSink, OtlpAudit, OtlpAuditConfig, SyslogAudit, SyslogConfig,
    export_pass,
};
pub use groups::Moderation;
pub use init::{
    InitConfig, InitGranted, InitReply, InitRequest, OidcClient, OidcLogin, SessionInfo,
};
pub use server::{Admin, AdminConfig, AdminRole, Deps, RoleBinding};
pub use sieve::{ScriptCheck, ScriptError};
