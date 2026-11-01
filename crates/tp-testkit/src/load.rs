//! Подменный монитор нагрузки (CONTRACTS.md#Load; SRS 6.3).
//!
//! [`MemLoadMonitor`] — уровень задаёт тест ([`MemLoadMonitor::set`]): проверка поведения ролей на `Elevated`
//! и `High` без настоящих показателей узла.

use std::sync::Arc;

use tokio::sync::watch;
use tp_contracts::load::{LoadLevel, LoadMonitor, LoadRx};

/// Монитор нагрузки с уровнем из теста; клоны разделяют уровень.
#[derive(Debug, Clone)]
pub struct MemLoadMonitor(Arc<watch::Sender<LoadLevel>>);

impl MemLoadMonitor {
    /// Монитор с начальным уровнем `level`.
    pub fn new(level: LoadLevel) -> Self {
        Self(Arc::new(watch::channel(level).0))
    }

    /// Переход на уровень `level`; подписчики получают уведомление, только если уровень изменился.
    pub fn set(&self, level: LoadLevel) {
        self.0.send_if_modified(|cur| {
            let changed = *cur != level;
            *cur = level;
            changed
        });
    }

    /// Монитор как `Arc<dyn LoadMonitor>` для `RoleCtx::load`.
    pub fn shared(&self) -> Arc<dyn LoadMonitor> {
        Arc::new(self.clone())
    }
}

impl Default for MemLoadMonitor {
    fn default() -> Self {
        Self::new(LoadLevel::Normal)
    }
}

impl LoadMonitor for MemLoadMonitor {
    fn subscribe(&self) -> LoadRx {
        self.0.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    #[tokio::test]
    async fn transitions_notify_subscribers() {
        let m = MemLoadMonitor::default();
        let shared = m.shared();
        let mut rx = shared.subscribe();
        assert_eq!(shared.level(), LoadLevel::Normal);

        m.set(LoadLevel::Normal);
        assert!(
            rx.changed().now_or_never().is_none(),
            "без перехода — без уведомления"
        );

        m.set(LoadLevel::High);
        rx.changed().await.expect("changed");
        assert_eq!(*rx.borrow_and_update(), LoadLevel::High);
        assert!(shared.level().limits_new_sessions());

        m.clone().set(LoadLevel::Elevated);
        rx.changed().await.expect("changed");
        assert_eq!(*rx.borrow_and_update(), LoadLevel::Elevated);
        assert!(m.level().pauses_background() && !m.level().limits_new_sessions());
    }
}
