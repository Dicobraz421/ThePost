//! Общее для фоновых работ BLOB (GC, поиск «сирот»): часы, ограничение скорости,
//! периодический проход под лизой лидера.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tokio::time::Instant;
use tp_contracts::Result;
use tp_contracts::lease::{Lease, LeaseStore};

/// Источник текущего времени (UTC). В тестах — часы testkit.
pub type Now = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// Системные часы UTC.
pub fn system_now() -> Now {
    Arc::new(OffsetDateTime::now_utc)
}

/// `t - d` с насыщением.
pub(crate) fn before(t: OffsetDateTime, d: Duration) -> OffsetDateTime {
    time::Duration::try_from(d)
        .ok()
        .and_then(|d| t.checked_sub(d))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// Ограничение скорости: не чаще `per_sec` операций в секунду; 0 — без ограничения.
#[derive(Debug)]
pub(crate) struct Pacer {
    interval: Option<Duration>,
    next: Option<Instant>,
}

impl Pacer {
    pub(crate) fn new(per_sec: u32) -> Self {
        Self {
            interval: (per_sec > 0).then(|| Duration::from_secs(1) / per_sec),
            next: None,
        }
    }

    /// Ждёт очереди следующей операции.
    pub(crate) async fn wait(&mut self) {
        let Some(interval) = self.interval else {
            return;
        };
        let now = Instant::now();
        let at = self.next.map_or(now, |n| n.max(now));
        if at > now {
            tokio::time::sleep_until(at).await;
        }
        self.next = Some(at + interval);
    }
}

/// Лиза лидера на время прохода.
pub(crate) struct Leader<'a> {
    leases: &'a dyn LeaseStore,
    lease: Lease,
    ttl: Duration,
    lost: bool,
}

impl Leader<'_> {
    /// Продлевает лизу между пакетами; `false` — лиза потеряна, проход нужно прекратить.
    pub(crate) async fn renew(&mut self) -> Result<bool> {
        match self.leases.renew(&self.lease, self.ttl).await? {
            Some(lease) => {
                self.lease = lease;
                Ok(true)
            }
            None => {
                self.lost = true;
                Ok(false)
            }
        }
    }
}

/// Продлевает лизу, если проход идёт под лизой; без лизы (разовый проход) — всегда `true`.
pub(crate) async fn keep(leader: &mut Option<&mut Leader<'_>>) -> Result<bool> {
    match leader {
        Some(l) => l.renew().await,
        None => Ok(true),
    }
}

/// Параметры периодического прохода под лизой.
#[derive(Clone, Copy)]
pub(crate) struct Schedule<'a> {
    pub leases: &'a dyn LeaseStore,
    pub name: &'a str,
    pub holder: &'a str,
    pub ttl: Duration,
    pub interval: Duration,
    /// Имя работы для логов.
    pub what: &'static str,
}

/// Проход фоновой работы под лизой. Футура — `Send`: `run` воркеров запускается через `tokio::spawn`
/// (async-замыкание здесь не годится — `Send` его футуры компилятор не выводит).
pub(crate) trait LeasedPass: Sync {
    fn leased_pass(&self, leader: &mut Leader<'_>) -> impl Future<Output = Result<()>> + Send;
}

/// Каждые `interval` захватывает (или продлевает) лизу `name` и выполняет проход `pass`;
/// узел без лизы пропускает проход. Завершается по `shutdown` и освобождает лизу.
/// Прерывание прохода на точке ожидания безопасно: каждый шаг протокола идемпотентен.
pub(crate) async fn run_leased(
    s: Schedule<'_>,
    shutdown: impl Future<Output = ()>,
    pass: &impl LeasedPass,
) {
    tokio::pin!(shutdown);
    let mut held: Option<Lease> = None;
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            () = tick(s, &mut held, pass) => {}
        }
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            () = tokio::time::sleep(s.interval) => {}
        }
    }
    if let Some(lease) = held
        && let Err(e) = s.leases.release(&lease).await
    {
        tracing::warn!(what = s.what, error = %e, "не удалось освободить лизу");
    }
}

async fn tick(s: Schedule<'_>, held: &mut Option<Lease>, pass: &impl LeasedPass) {
    let lease = match held.take() {
        Some(lease) => s.leases.renew(&lease, s.ttl).await,
        None => s.leases.try_acquire(s.name, s.holder, s.ttl).await,
    };
    let lease = match lease {
        Ok(Some(lease)) => lease,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!(what = s.what, error = %e, "лиза лидера недоступна");
            return;
        }
    };
    let mut leader = Leader {
        leases: s.leases,
        lease,
        ttl: s.ttl,
        lost: false,
    };
    if let Err(e) = pass.leased_pass(&mut leader).await {
        tracing::warn!(what = s.what, error = %e, "проход прерван ошибкой");
    }
    if leader.lost {
        tracing::info!(what = s.what, "лиза лидера потеряна");
    } else {
        *held = Some(leader.lease);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn before_saturates() {
        let t = OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(10);
        assert_eq!(
            before(t, Duration::from_secs(4)),
            OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(6)
        );
        assert_eq!(before(t, Duration::MAX), OffsetDateTime::UNIX_EPOCH);
    }

    #[tokio::test]
    async fn pacer_limits_rate() {
        let mut free = Pacer::new(0);
        let start = Instant::now();
        for _ in 0..100 {
            free.wait().await;
        }
        assert!(start.elapsed() < Duration::from_millis(50));

        let mut paced = Pacer::new(100);
        let start = Instant::now();
        for _ in 0..6 {
            paced.wait().await;
        }
        // Первая операция сразу, ещё пять — через 10 мс каждая.
        assert!(start.elapsed() >= Duration::from_millis(50));
    }
}
