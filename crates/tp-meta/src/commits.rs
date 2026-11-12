//! Время фиксации транзакций узла на primary (META-53; решение 2026-11-11 п. 57, SRS Д.7).
//!
//! Каждая фиксация крейта идёт через [`commit`] и попадает в гистограмму процесса ([`commit_metrics`]);
//! отправки NOTIFY публикатора сигналов — ещё и в гистограмму шины ([`crate::signals::SignalMetrics`]).
//! Доля времени фиксации, занятая NOTIFY, — отношение сумм двух гистограмм
//! ([`crate::signals::SignalMetrics::notify_commit_share`]); по кластеру — отношение сумм по узлам.
//! Время измеряет узел: от отправки COMMIT до ответа СУБД (с сетью).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use sqlx::{Postgres, Transaction};
use tp_contracts::Result;

use crate::schema::db_err;

/// Число корзин гистограммы: границы — 64 мкс × 2^i, последняя корзина — без верхней границы.
pub const HISTOGRAM_BUCKETS: usize = 20;

/// Снимок гистограммы длительностей.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Histogram {
    /// Наблюдений в корзинах (не накопительно); границы — [`Histogram::upper_bound`].
    pub buckets: [u64; HISTOGRAM_BUCKETS],
    /// Всего наблюдений.
    pub count: u64,
    /// Сумма длительностей.
    pub sum: Duration,
}

impl Histogram {
    /// Верхняя граница корзины `i` (включительно); `None` — последняя корзина (+Inf).
    pub fn upper_bound(i: usize) -> Option<Duration> {
        (i + 1 < HISTOGRAM_BUCKETS).then(|| Duration::from_micros(64 << i))
    }

    /// Корзины накопительно, как в формате Prometheus (`le`): граница и число наблюдений не больше неё.
    pub fn cumulative(&self) -> impl Iterator<Item = (Option<Duration>, u64)> + '_ {
        let mut total = 0u64;
        self.buckets.iter().enumerate().map(move |(i, n)| {
            total = total.saturating_add(*n);
            (Self::upper_bound(i), total)
        })
    }

    /// Сумма двух снимков (шины шардов).
    #[must_use]
    pub fn merged(mut self, other: &Self) -> Self {
        for (a, b) in self.buckets.iter_mut().zip(other.buckets) {
            *a = a.saturating_add(b);
        }
        self.count = self.count.saturating_add(other.count);
        self.sum = self.sum.saturating_add(other.sum);
        self
    }
}

/// Накопитель гистограммы.
#[derive(Debug)]
pub(crate) struct Recorder {
    buckets: [AtomicU64; HISTOGRAM_BUCKETS],
    count: AtomicU64,
    sum_us: AtomicU64,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    pub(crate) const fn new() -> Self {
        Self {
            buckets: [const { AtomicU64::new(0) }; HISTOGRAM_BUCKETS],
            count: AtomicU64::new(0),
            sum_us: AtomicU64::new(0),
        }
    }

    pub(crate) fn observe(&self, took: Duration) {
        let us = u64::try_from(took.as_micros()).unwrap_or(u64::MAX);
        let i = (0..HISTOGRAM_BUCKETS - 1)
            .find(|i| us <= 64 << i)
            .unwrap_or(HISTOGRAM_BUCKETS - 1);
        self.buckets[i].fetch_add(1, Ordering::Relaxed);
        self.count.fetch_add(1, Ordering::Relaxed);
        self.sum_us.fetch_add(us, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> Histogram {
        let mut buckets = [0; HISTOGRAM_BUCKETS];
        for (b, a) in buckets.iter_mut().zip(&self.buckets) {
            *b = a.load(Ordering::Relaxed);
        }
        Histogram {
            buckets,
            count: self.count.load(Ordering::Relaxed),
            sum: Duration::from_micros(self.sum_us.load(Ordering::Relaxed)),
        }
    }
}

/// Все фиксации процесса (включая отправки NOTIFY).
static COMMITS: Recorder = Recorder::new();

/// Учитывает фиксацию, выполненную не через [`commit`] (NOTIFY публикатора — одна команда с неявной фиксацией).
pub(crate) fn observe_commit(took: Duration) {
    COMMITS.observe(took);
}

/// Фиксирует транзакцию и учитывает время фиксации (успешной).
pub(crate) async fn commit(tx: Transaction<'_, Postgres>) -> Result<()> {
    let started = Instant::now();
    tx.commit().await.map_err(db_err)?;
    COMMITS.observe(started.elapsed());
    Ok(())
}

/// Гистограмма времени всех фиксаций процесса на primary (с запуска), включая отправки NOTIFY.
pub fn commit_metrics() -> Histogram {
    COMMITS.snapshot()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn histogram_buckets() {
        let r = Recorder::new();
        r.observe(Duration::from_micros(64));
        r.observe(Duration::from_micros(65));
        r.observe(Duration::from_secs(3600));
        let h = r.snapshot();
        assert_eq!(h.count, 3);
        assert_eq!((h.buckets[0], h.buckets[1]), (1, 1));
        assert_eq!(h.buckets[HISTOGRAM_BUCKETS - 1], 1);
        assert_eq!(
            h.sum,
            Duration::from_micros(129) + Duration::from_secs(3600)
        );
        let cum: Vec<_> = h.cumulative().collect();
        assert_eq!(cum[0], (Some(Duration::from_micros(64)), 1));
        assert_eq!(cum[1], (Some(Duration::from_micros(128)), 2));
        assert_eq!(cum[HISTOGRAM_BUCKETS - 1], (None, 3));
        let twice = h.merged(&h);
        assert_eq!((twice.count, twice.sum), (6, h.sum * 2));
    }
}
