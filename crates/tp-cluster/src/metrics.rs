//! Счётчики шины уведомлений: платформа снимает их в общий реестр метрик (PLT-10, решение #39).
//!
//! Счётчики — атомики без зависимостей: шина отдаёт [`Arc<BusCounters>`], платформа по запросу
//! `/metrics` берёт [`BusCounters::snapshot`]. Значения только растут (кроме `connected`).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Снимок счётчиков шины.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BusMetrics {
    /// Есть связь с шиной (в процессе — всегда).
    pub connected: bool,
    /// Опубликовано уведомлений.
    pub published: u64,
    /// Неудачных публикаций (нет связи, таймаут).
    pub publish_errors: u64,
    /// Получено уведомлений узлом из шины (в процессе — каждое опубликованное).
    pub received: u64,
    /// Выдано уведомлений подписчикам узла (по одному на подписчика).
    pub delivered: u64,
    /// Получено нераспознанных сообщений.
    pub decode_errors: u64,
    /// Потерь связи с шиной (каждая ведёт к переподключению).
    pub reconnects: u64,
    /// Подписчиков, отключённых за отставание.
    pub lagged: u64,
}

/// Счётчики шины; общие для шины и платформы.
#[derive(Debug, Default)]
pub struct BusCounters {
    pub(crate) connected: AtomicBool,
    pub(crate) published: AtomicU64,
    pub(crate) publish_errors: AtomicU64,
    pub(crate) received: AtomicU64,
    pub(crate) delivered: AtomicU64,
    pub(crate) decode_errors: AtomicU64,
    pub(crate) reconnects: AtomicU64,
    pub(crate) lagged: AtomicU64,
}

impl BusCounters {
    /// Снимок текущих значений.
    pub fn snapshot(&self) -> BusMetrics {
        BusMetrics {
            connected: self.connected.load(Ordering::Relaxed),
            published: self.published.load(Ordering::Relaxed),
            publish_errors: self.publish_errors.load(Ordering::Relaxed),
            received: self.received.load(Ordering::Relaxed),
            delivered: self.delivered.load(Ordering::Relaxed),
            decode_errors: self.decode_errors.load(Ordering::Relaxed),
            reconnects: self.reconnects.load(Ordering::Relaxed),
            lagged: self.lagged.load(Ordering::Relaxed),
        }
    }

    pub(crate) fn set_connected(&self, connected: bool) {
        self.connected.store(connected, Ordering::Relaxed);
    }
}

pub(crate) fn inc(counter: &AtomicU64) {
    counter.fetch_add(1, Ordering::Relaxed);
}
