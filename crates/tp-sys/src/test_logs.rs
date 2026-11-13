//! Журнал тестового подписчика `tracing` для проверок записей в журнал.

use std::io::{self, Write};
use std::sync::{Arc, Mutex, PoisonError};

use tracing::subscriber::DefaultGuard;

/// Накопленный текст журнала.
#[derive(Debug, Clone, Default)]
pub(crate) struct Logs(Arc<Mutex<Vec<u8>>>);

impl Logs {
    /// Подписчик потока, пишущий в новый журнал, пока жив страж. В однопоточном рантайме теста он видит и
    /// события фоновых задач.
    pub(crate) fn capture() -> (Self, DefaultGuard) {
        let logs = Self::default();
        let writer = logs.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        (logs, tracing::subscriber::set_default(subscriber))
    }

    pub(crate) fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap_or_else(PoisonError::into_inner)).into_owned()
    }
}

impl Write for Logs {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
