//! Лимит отправки учётной записи (SRS 6.3): писем в час — `Policy::send_per_hour` политики организации
//! (цепочка «организация → домен → учётная запись», SMTP-40), защита от рассылок со скомпрометированных ящиков.
//!
//! Окно скользящее: оценка — счёт текущего часа плюс доля счёта предыдущего часа, пропорциональная
//! непрошедшей части окна. Учитываются принятые письма (после ответа `2xx` на данные); проверка — на `MAIL`.
//! Счётчики — в памяти узла, как и [`tp_platform::RateLimiter`]; число учётных записей ограничено
//! `smtp.limits.max_tracked_accounts`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Окно лимита.
const HOUR: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy)]
struct Window {
    start: Instant,
    cur: u64,
    prev: u64,
}

impl Window {
    fn new(now: Instant) -> Self {
        Self {
            start: now,
            cur: 0,
            prev: 0,
        }
    }

    /// Сдвигает окно к `now`.
    fn roll(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.start);
        if elapsed >= 2 * HOUR {
            *self = Self::new(now);
        } else if elapsed >= HOUR {
            self.prev = self.cur;
            self.cur = 0;
            self.start += HOUR;
        }
    }

    /// Оценка писем за последний час.
    fn estimate(&self, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.start).as_secs_f64();
        let rest = (1.0 - elapsed / HOUR.as_secs_f64()).clamp(0.0, 1.0);
        self.cur as f64 + self.prev as f64 * rest
    }
}

/// Счётчики отправки по учётным записям узла.
#[derive(Default)]
pub struct SendLimits {
    state: Mutex<HashMap<String, Window>>,
}

impl fmt::Debug for SendLimits {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SendLimits").finish_non_exhaustive()
    }
}

impl SendLimits {
    /// Пустые счётчики.
    pub fn new() -> Self {
        Self::default()
    }

    /// Можно начать ещё одно письмо учётной записи `account` при лимите `per_hour` писем в час.
    pub fn allow_message_at(&self, per_hour: u32, account: &str, now: Instant) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let sent = match state.get_mut(account) {
            Some(w) => {
                w.roll(now);
                w.estimate(now)
            }
            None => 0.0,
        };
        sent + 1.0 <= f64::from(per_hour)
    }

    /// Учитывает принятое письмо учётной записи `account`; под наблюдением не больше `max_tracked` учётных записей.
    pub fn record_at(&self, max_tracked: usize, account: &str, now: Instant) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.len() >= max_tracked.max(1) && !state.contains_key(account) {
            // Учётные записи без отправок за два часа больше не влияют на оценку.
            state.retain(|_, w| now.saturating_duration_since(w.start) < 2 * HOUR);
            if state.len() >= max_tracked.max(1) {
                tracing::warn!("smtp: превышен предел учётных записей в лимитах отправки");
                return;
            }
        }
        let w = state
            .entry(account.to_owned())
            .or_insert_with(|| Window::new(now));
        w.roll(now);
        w.cur += 1;
    }

    /// Учётных записей под наблюдением.
    pub fn len(&self) -> usize {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// Счётчиков нет.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests;
