//! Ошибка границ крейтов (CONTRACTS.md#Errors).

use std::io;
use std::time::Duration;

/// Ошибка на границе крейтов. Внутри крейта — собственные ошибки `thiserror`, преобразуемые в эту.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// Объект не найден (в контексте организации вызова).
    #[error("not found")]
    NotFound,
    /// Конфликт состояния: объект уже существует, изменён параллельно, нарушено условие.
    #[error("conflict")]
    Conflict,
    /// Некорректный запрос или данные.
    #[error("invalid: {0}")]
    Invalid(String),
    /// Нет прав на операцию.
    #[error("forbidden")]
    Forbidden,
    /// Превышена квота.
    #[error("quota exceeded")]
    QuotaExceeded,
    /// Временная недоступность (сеть, БД, S3); операцию можно повторить.
    #[error("unavailable")]
    Unavailable {
        /// Рекомендуемая пауза перед повтором, если известна.
        retry_after: Option<Duration>,
    },
    /// Внутренняя ошибка; повтор не поможет.
    #[error("internal: {0}")]
    Internal(String),
}

/// Результат операций на границе крейтов.
pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// `Unavailable` без рекомендации паузы.
    pub const UNAVAILABLE: Error = Error::Unavailable { retry_after: None };

    /// Ошибку можно повторить (только `Unavailable`).
    pub fn is_retryable(&self) -> bool {
        matches!(self, Error::Unavailable { .. })
    }

    /// `Invalid` из любого сообщения.
    pub fn invalid(msg: impl Into<String>) -> Self {
        Error::Invalid(msg.into())
    }

    /// `Internal` из любого сообщения.
    pub fn internal(msg: impl Into<String>) -> Self {
        Error::Internal(msg.into())
    }
}

impl From<io::Error> for Error {
    /// `NotFound` → `NotFound`; сетевые и временные сбои → `Unavailable`; остальное → `Internal`.
    fn from(e: io::Error) -> Self {
        use io::ErrorKind as K;
        match e.kind() {
            K::NotFound => Error::NotFound,
            K::TimedOut
            | K::Interrupted
            | K::WouldBlock
            | K::ConnectionRefused
            | K::ConnectionReset
            | K::ConnectionAborted
            | K::NotConnected
            | K::BrokenPipe
            | K::HostUnreachable
            | K::NetworkUnreachable
            | K::NetworkDown
            | K::ResourceBusy => Error::UNAVAILABLE,
            _ => Error::Internal(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn io_mapping() {
        let e = |k| Error::from(io::Error::from(k));
        assert_eq!(e(io::ErrorKind::NotFound), Error::NotFound);
        assert!(e(io::ErrorKind::TimedOut).is_retryable());
        assert!(e(io::ErrorKind::ConnectionReset).is_retryable());
        assert!(matches!(
            e(io::ErrorKind::PermissionDenied),
            Error::Internal(_)
        ));
        assert!(!Error::Conflict.is_retryable());
    }
}
