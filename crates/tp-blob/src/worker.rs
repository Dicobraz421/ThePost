//! Общее для фоновых работ BLOB (GC, поиск «сирот»): часы и таймер, ограничение скорости,
//! проход под лизой лидера (по расписанию из конфигурации или по сроку).

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use time::{OffsetDateTime, PrimitiveDateTime};
use tokio::time::Instant;
use tp_contracts::Result;
use tp_contracts::lease::{Lease, LeaseStore};

/// Источник текущего времени (UTC). В тестах — часы testkit.
pub type Now = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// Системные часы UTC.
pub fn system_now() -> Now {
    Arc::new(OffsetDateTime::now_utc)
}

/// Таймер: футура завершается не раньше момента `at` (UTC). В работе — tokio по системным часам,
/// в тестах — по часам testkit.
pub(crate) type Sleep = Arc<dyn Fn(OffsetDateTime) -> BoxFuture<'static, ()> + Send + Sync>;

/// Таймер tokio до момента `at` по часам `now`.
pub(crate) fn system_sleep(now: Now) -> Sleep {
    Arc::new(move |at| {
        let wait = Duration::try_from(at - now()).unwrap_or(Duration::ZERO);
        Box::pin(tokio::time::sleep(wait))
    })
}

/// Момент позже любого срока.
const FAR: OffsetDateTime = PrimitiveDateTime::MAX.assume_utc();

/// `t + d`; вне диапазона — [`FAR`].
pub(crate) fn after(t: OffsetDateTime, d: Duration) -> OffsetDateTime {
    time::Duration::try_from(d)
        .ok()
        .and_then(|d| t.checked_add(d))
        .unwrap_or(FAR)
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

/// Параметры прохода под лизой.
#[derive(Clone, Copy)]
pub(crate) struct Schedule<'a> {
    pub leases: &'a dyn LeaseStore,
    pub name: &'a str,
    pub holder: &'a str,
    pub ttl: Duration,
    /// Имя работы для логов.
    pub what: &'static str,
}

/// Итог попытки прохода под лизой.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Tick {
    /// Проход выполнен.
    Done,
    /// Лиза у другого узла или потеряна во время прохода.
    NotLeader,
    /// Лиза недоступна или проход прерван ошибкой.
    Failed,
}

/// Проход фоновой работы под лизой. Футура — `Send`: `run` воркеров запускается через `tokio::spawn`
/// (async-замыкание здесь не годится — `Send` его футуры компилятор не выводит).
pub(crate) trait LeasedPass: Sync {
    fn leased_pass(&self, leader: &mut Leader<'_>) -> impl Future<Output = Result<()>> + Send;
}

/// По расписанию из конфигурации (SRS 2.8, таймер 4) каждые `interval` захватывает (или продлевает)
/// лизу `name` и выполняет проход `pass`; узел без лизы пропускает проход. Завершается по `shutdown`
/// и освобождает лизу ([`release`]). Прерывание прохода на точке ожидания безопасно: каждый шаг
/// протокола идемпотентен.
pub(crate) async fn run_leased(
    s: Schedule<'_>,
    interval: Duration,
    shutdown: impl Future<Output = ()>,
    pass: &impl LeasedPass,
) {
    tokio::pin!(shutdown);
    let mut held: Option<Lease> = None;
    loop {
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            _ = tick(s, &mut held, pass) => {}
        }
        tokio::select! {
            biased;
            () = &mut shutdown => break,
            () = tokio::time::sleep(interval) => {}
        }
    }
    release(s, held).await;
}

/// Освобождает лизу, удержанную между проходами.
pub(crate) async fn release(s: Schedule<'_>, held: Option<Lease>) {
    if let Some(lease) = held
        && let Err(e) = s.leases.release(&lease).await
    {
        tracing::warn!(what = s.what, error = %e, "не удалось освободить лизу");
    }
}

/// Продлевает удержанную лизу (истёкшую и занятую другим — захватывает заново) и выполняет проход.
pub(crate) async fn tick(
    s: Schedule<'_>,
    held: &mut Option<Lease>,
    pass: &impl LeasedPass,
) -> Tick {
    let renewed = match held.take() {
        Some(lease) => s.leases.renew(&lease, s.ttl).await,
        None => Ok(None),
    };
    let lease = match renewed {
        Ok(Some(lease)) => Ok(Some(lease)),
        Ok(None) => s.leases.try_acquire(s.name, s.holder, s.ttl).await,
        Err(e) => Err(e),
    };
    let lease = match lease {
        Ok(Some(lease)) => lease,
        Ok(None) => return Tick::NotLeader,
        Err(e) => {
            tracing::warn!(what = s.what, error = %e, "лиза лидера недоступна");
            return Tick::Failed;
        }
    };
    let mut leader = Leader {
        leases: s.leases,
        lease,
        ttl: s.ttl,
        lost: false,
    };
    let res = pass.leased_pass(&mut leader).await;
    if let Err(e) = &res {
        tracing::warn!(what = s.what, error = %e, "проход прерван ошибкой");
    }
    if leader.lost {
        tracing::info!(what = s.what, "лиза лидера потеряна");
        return Tick::NotLeader;
    }
    *held = Some(leader.lease);
    if res.is_ok() {
        Tick::Done
    } else {
        Tick::Failed
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
