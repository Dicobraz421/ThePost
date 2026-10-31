//! SMTP-сервер (SRS 4.1): сессия по RFC 5321 — разбор команд, данных письма и ответов на `smtp-proto`.
//!
//! [`Session`] — конечный автомат сессии над любым потоком (`AsyncRead + AsyncWrite`): `EHLO`/`HELO`, `MAIL`,
//! `RCPT`, `DATA`, `BDAT` (CHUNKING), `RSET`, `NOOP`, `QUIT`; расширения PIPELINING, SIZE, 8BITMIME, SMTPUTF8,
//! ENHANCEDSTATUSCODES; лимиты и таймауты — секция [`SmtpConfig`]. Что делать с получателем и письмом,
//! решает [`Handler`] (локальная доставка — [`crate::deliver`]).

use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use bytes::Bytes;
use time::OffsetDateTime;
use tp_contracts::auth::Principal;
use tp_platform::ConnInfo;

use crate::config::SmtpConfig;

mod session;
#[cfg(test)]
mod tests;

pub use session::{Exit, Session};

/// Ответ SMTP: код, расширенный код (RFC 3463) и текст.
pub type Reply = smtp_proto::Response<String>;

/// Ответ с кодом `code`, расширенным кодом `esc` и текстом `text`.
pub fn reply(code: u16, esc: [u8; 3], text: impl Into<String>) -> Reply {
    smtp_proto::Response {
        code,
        esc,
        message: text.into(),
    }
}

/// Ответ положительный (`2xx`).
pub fn is_positive(r: &Reply) -> bool {
    (200..300).contains(&r.code)
}

/// Режим слушателя.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Роль smtp-in: приём почты от других MTA (25), только для локальных получателей.
    Inbound,
    /// Роль submission: отправка клиентами (587, 465), только после аутентификации.
    Submission,
}

impl Mode {
    /// Имя роли (`Role::as_str`).
    pub fn name(self) -> &'static str {
        match self {
            Mode::Inbound => "smtp-in",
            Mode::Submission => "submission",
        }
    }
}

/// Конверт транзакции SMTP.
#[derive(Clone)]
pub struct Envelope {
    /// Идентификатор транзакции (журналы, заголовок `Received`).
    pub id: String,
    /// Режим слушателя.
    pub mode: Mode,
    /// Соединение: адрес клиента, TLS.
    pub conn: ConnInfo,
    /// Имя из `EHLO`/`HELO`.
    pub helo: String,
    /// Клиент представился `EHLO` (ESMTP).
    pub esmtp: bool,
    /// Аутентифицированный отправитель (submission).
    pub principal: Option<Principal>,
    /// Отправитель конверта (`MAIL FROM`); пустой — уведомление о недоставке (`<>`).
    pub mail_from: String,
    /// Принятые получатели (`RCPT TO`) в порядке команд.
    pub rcpts: Vec<String>,
    /// Транзакция объявила `SMTPUTF8` (RFC 6531).
    pub smtputf8: bool,
    /// Начало транзакции (`MAIL`).
    pub started_at: OffsetDateTime,
}

impl fmt::Debug for Envelope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Адреса — персональные данные: в отладочный вывод только счётчики.
        f.debug_struct("Envelope")
            .field("id", &self.id)
            .field("mode", &self.mode)
            .field("client", &self.conn.client)
            .field("tls", &self.conn.tls)
            .field("authenticated", &self.principal.is_some())
            .field("rcpts", &self.rcpts.len())
            .finish_non_exhaustive()
    }
}

/// Обработчик транзакций сессии: проверка получателя и приём письма.
#[async_trait]
pub trait Handler: Send + Sync + 'static {
    /// Проверяет получателя `rcpt` для транзакции `env` (принятые ранее — в `env.rcpts`).
    /// Положительный ответ — получатель принят.
    async fn rcpt(&self, env: &Envelope, rcpt: &str) -> Reply;

    /// Принимает письмо `raw` (после снятия точек `DATA` или склейки `BDAT`) для всех получателей `env`.
    async fn data(&self, env: &Envelope, raw: Bytes) -> Reply;
}

/// Общее для сессий слушателя: режим, конфигурация, обработчик.
#[derive(Clone)]
pub struct SessionCtx {
    /// Режим слушателя.
    pub mode: Mode,
    /// Секция `smtp` на момент соединения.
    pub config: Arc<SmtpConfig>,
    /// Обработчик транзакций.
    pub handler: Arc<dyn Handler>,
}

impl fmt::Debug for SessionCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionCtx")
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

/// Идентификатор транзакции: время запуска процесса и счётчик, уникален в пределах узла.
pub(crate) fn next_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    static START: std::sync::OnceLock<i64> = std::sync::OnceLock::new();
    let start = *START.get_or_init(|| OffsetDateTime::now_utc().unix_timestamp());
    format!("{start:x}{:06x}", SEQ.fetch_add(1, Ordering::Relaxed))
}
