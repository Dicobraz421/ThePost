//! Секция `imap` config.yaml (SRS 4.2, 5.5). Поля необязательны, неизвестное поле — ошибка.
//!
//! ```yaml
//! imap:
//!   listeners:
//!     - listen: "[::]:143"
//!     - listen: "[::]:993"
//!       implicit_tls: true
//!   limits:
//!     max_message_size: 52428800
//!   timeouts:
//!     login: 1m
//!     autologout: 30m
//!     drain: 30s
//! ```
//!
//! Слушатели применяются при запуске роли; лимиты и таймауты — к новым сессиям без перезапуска.

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tp_contracts::config::duration;
use tp_platform::EndpointConfig;

/// Секция `imap`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ImapConfig {
    /// Слушатели IMAP: 143 со STARTTLS и 993 с неявным TLS. Требует перезапуска.
    pub listeners: Vec<EndpointConfig>,
    /// Лимиты сессии.
    pub limits: Limits,
    /// Таймауты сессии.
    pub timeouts: Timeouts,
}

/// Лимиты сессии IMAP.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Наибольший литерал (письмо в `APPEND`), байт.
    pub max_message_size: u32,
    /// Наибольшая команда без литералов, байт.
    pub max_command_size: u32,
    /// Ошибочных команд (`BAD`) за сессию; затем `BYE` и закрытие.
    pub max_errors: u32,
    /// Неудачных попыток входа за сессию; затем `BYE` и закрытие.
    pub max_auth_failures: u32,
    /// Одновременных соединений на слушатель; сверх лимита — `BYE` и закрытие.
    pub max_connections: usize,
    /// Наибольшее число писем выбранной папки.
    pub max_mailbox_messages: usize,
    /// Писем в одной выборке хранилища при `FETCH`.
    pub fetch_batch: usize,
    /// Накопленный ответ отправляется клиенту, когда превышает этот размер, байт.
    pub write_buffer: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_message_size: 50 * 1024 * 1024,
            max_command_size: 64 * 1024,
            max_errors: 20,
            max_auth_failures: 3,
            max_connections: 5000,
            max_mailbox_messages: 1_000_000,
            fetch_batch: 256,
            write_buffer: 64 * 1024,
        }
    }
}

/// Таймауты сессии IMAP (RFC 9051, 5.4).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    /// Бездействие до входа.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub login: Duration,
    /// Бездействие после входа (не меньше 30 минут по RFC 9051).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub autologout: Duration,
    /// Ожидание завершения сессий при остановке узла после `BYE`.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub drain: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            login: Duration::from_secs(60),
            autologout: Duration::from_secs(30 * 60),
            drain: Duration::from_secs(30),
        }
    }
}
