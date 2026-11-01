//! Лимиты отправки учётной записи (SRS 6.3): писем и получателей в час (`smtp.limits.messages_per_hour`,
//! `smtp.limits.recipients_per_hour`) — защита от рассылок со скомпрометированных ящиков.
//!
//! Окно скользящее: оценка — счёт текущего часа плюс доля счёта предыдущего часа, пропорциональная
//! непрошедшей части окна. Учитываются принятые письма (после ответа `2xx` на данные); проверка — на `MAIL`
//! (письма) и `RCPT` (получатели вместе с уже принятыми в транзакции). Счётчики — в памяти узла, как и
//! [`tp_platform::RateLimiter`]; число учётных записей ограничено `smtp.limits.max_tracked_accounts`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::config::Limits;

/// Окно лимита.
const HOUR: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Copy, Default)]
struct Counts {
    messages: u64,
    rcpts: u64,
}

#[derive(Debug, Clone, Copy)]
struct Window {
    start: Instant,
    cur: Counts,
    prev: Counts,
}

impl Window {
    fn new(now: Instant) -> Self {
        Self {
            start: now,
            cur: Counts::default(),
            prev: Counts::default(),
        }
    }

    /// Сдвигает окно к `now`.
    fn roll(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.start);
        if elapsed >= 2 * HOUR {
            *self = Self::new(now);
        } else if elapsed >= HOUR {
            self.prev = self.cur;
            self.cur = Counts::default();
            self.start += HOUR;
        }
    }

    /// Оценка за последний час: (письма, получатели).
    fn estimate(&self, now: Instant) -> (f64, f64) {
        let elapsed = now.saturating_duration_since(self.start).as_secs_f64();
        let rest = (1.0 - elapsed / HOUR.as_secs_f64()).clamp(0.0, 1.0);
        (
            self.cur.messages as f64 + self.prev.messages as f64 * rest,
            self.cur.rcpts as f64 + self.prev.rcpts as f64 * rest,
        )
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

    fn estimate(&self, account: &str, now: Instant) -> (f64, f64) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        match state.get_mut(account) {
            Some(w) => {
                w.roll(now);
                w.estimate(now)
            }
            None => (0.0, 0.0),
        }
    }

    /// Можно начать ещё одно письмо учётной записи `account`.
    pub fn allow_message_at(&self, cfg: &Limits, account: &str, now: Instant) -> bool {
        cfg.messages_per_hour == 0
            || self.estimate(account, now).0 + 1.0 <= f64::from(cfg.messages_per_hour)
    }

    /// Можно отправить письмо `rcpts` получателям (с уже принятыми в транзакции).
    pub fn allow_rcpts_at(&self, cfg: &Limits, account: &str, rcpts: usize, now: Instant) -> bool {
        cfg.recipients_per_hour == 0
            || self.estimate(account, now).1 + rcpts as f64 <= f64::from(cfg.recipients_per_hour)
    }

    /// Учитывает принятое письмо `rcpts` получателям.
    pub fn record_at(&self, cfg: &Limits, account: &str, rcpts: usize, now: Instant) {
        if cfg.messages_per_hour == 0 && cfg.recipients_per_hour == 0 {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.len() >= cfg.max_tracked_accounts.max(1) && !state.contains_key(account) {
            // Учётные записи без отправок за два часа больше не влияют на оценку.
            state.retain(|_, w| now.saturating_duration_since(w.start) < 2 * HOUR);
            if state.len() >= cfg.max_tracked_accounts.max(1) {
                tracing::warn!("smtp: превышен предел учётных записей в лимитах отправки");
                return;
            }
        }
        let w = state
            .entry(account.to_owned())
            .or_insert_with(|| Window::new(now));
        w.roll(now);
        w.cur.messages += 1;
        w.cur.rcpts += rcpts as u64;
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
