//! Сигналы процесса (SRS 2.2): SIGHUP — перечитать конфигурацию (hot reload, SRS 5.5),
//! SIGTERM/SIGINT — штатная остановка.

use std::io;

use tokio::signal::unix::{SignalKind, signal};

/// Сигнал, полученный процессом.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Signal {
    /// SIGHUP: перечитать конфигурацию.
    Hup,
    /// SIGTERM: штатная остановка (systemd, Kubernetes).
    Term,
    /// SIGINT: остановка из терминала.
    Int,
}

impl Signal {
    /// Сигнал требует остановки процесса.
    pub fn is_shutdown(self) -> bool {
        matches!(self, Self::Term | Self::Int)
    }
}

/// Поток сигналов SIGHUP/SIGTERM/SIGINT. Обработчики устанавливаются при создании и действуют,
/// пока жив процесс; повторные сигналы между вызовами `recv` объединяются ядром tokio.
#[derive(Debug)]
pub struct Signals {
    hup: tokio::signal::unix::Signal,
    term: tokio::signal::unix::Signal,
    int: tokio::signal::unix::Signal,
}

impl Signals {
    /// Устанавливает обработчики. Требует контекста tokio-рантайма.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            hup: signal(SignalKind::hangup())?,
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
        })
    }

    /// Следующий сигнал. `None` — источник сигналов закрыт рантаймом.
    pub async fn recv(&mut self) -> Option<Signal> {
        tokio::select! {
            r = self.hup.recv() => r.map(|()| Signal::Hup),
            r = self.term.recv() => r.map(|()| Signal::Term),
            r = self.int.recv() => r.map(|()| Signal::Int),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use nix::sys::signal::{Signal as NixSignal, kill};
    use nix::unistd::Pid;

    use super::*;

    #[tokio::test]
    async fn sighup_to_self_is_received() {
        let mut signals = Signals::new().expect("install handlers");
        kill(Pid::this(), NixSignal::SIGHUP).expect("kill");
        let got = tokio::time::timeout(Duration::from_secs(5), signals.recv())
            .await
            .expect("signal within timeout");
        assert_eq!(got, Some(Signal::Hup));
        assert!(!Signal::Hup.is_shutdown());
        assert!(Signal::Term.is_shutdown());
    }
}
