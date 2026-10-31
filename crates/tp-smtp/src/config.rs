//! Секция `smtp` config.yaml (SRS 4.1, 5.5). Поля необязательны, неизвестное поле — ошибка.
//!
//! ```yaml
//! smtp:
//!   hostname: mx.example.org
//!   inbound:
//!     - listen: "[::]:25"
//!   submission:
//!     - listen: "[::]:587"
//!     - listen: "[::]:465"
//!       implicit_tls: true
//!   limits:
//!     max_message_size: 52428800
//!     max_recipients: 100
//!   timeouts:
//!     command: 5m
//!     data: 10m
//! ```
//!
//! Слушатели (`inbound`, `submission`) применяются при запуске роли; лимиты и таймауты — к новым сессиям
//! без перезапуска.

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tp_contracts::config::duration;
use tp_platform::EndpointConfig;

/// Секция `smtp`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SmtpConfig {
    /// Имя узла в приветствии, ответе `EHLO` и заголовке `Received`.
    pub hostname: String,
    /// Слушатели роли smtp-in (приём почты, 25 с STARTTLS). Требует перезапуска.
    pub inbound: Vec<EndpointConfig>,
    /// Слушатели роли submission: 587 (STARTTLS) и 465 (`implicit_tls: true`). Требует перезапуска.
    pub submission: Vec<EndpointConfig>,
    /// Лимиты сессии.
    pub limits: Limits,
    /// Таймауты сессии.
    pub timeouts: Timeouts,
}

impl Default for SmtpConfig {
    fn default() -> Self {
        Self {
            hostname: "localhost".into(),
            inbound: Vec::new(),
            submission: Vec::new(),
            limits: Limits::default(),
            timeouts: Timeouts::default(),
        }
    }
}

/// Лимиты сессии SMTP.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Наибольший размер письма, байт (объявляется в `SIZE`).
    pub max_message_size: usize,
    /// Получателей в одной транзакции.
    pub max_recipients: usize,
    /// Ошибочных команд за сессию; затем соединение закрывается (`421`).
    pub max_errors: u32,
    /// Одновременных соединений на слушатель; сверх лимита — `421` и закрытие.
    pub max_connections: usize,
    /// Неудачных `AUTH` подряд; затем адрес клиента блокируется (`ratelimit.block_duration`) и соединение
    /// закрывается.
    pub max_auth_failures: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_message_size: 50 * 1024 * 1024,
            max_recipients: 100,
            max_errors: 10,
            max_connections: 1000,
            max_auth_failures: 3,
        }
    }
}

/// Таймауты сессии SMTP (RFC 5321, 4.5.3.2).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    /// Ожидание команды.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub command: Duration,
    /// Ожидание очередного блока данных письма (`DATA`, `BDAT`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub data: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            command: Duration::from_secs(5 * 60),
            data: Duration::from_secs(10 * 60),
        }
    }
}
