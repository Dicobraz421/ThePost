//! Лимиты отправки учётной записи (SRS 6.3) — политики организации (цепочка «организация → домен → учётная
//! запись», SMTP-40): писем в час — `Policy::send_per_hour`, адресатов в час — `Policy::rcpts_per_hour` (сумма
//! получателей писем, как RecipientRateLimit в Exchange; SMTP-43, решение 113). Защита от рассылок со
//! скомпрометированных ящиков.
//!
//! Окно скользящее: оценка — счёт текущего часа плюс доля счёта предыдущего часа, пропорциональная
//! непрошедшей части окна. Учитываются принятые письма (submission — после ответа `2xx` на данные, JMAP — после
//! отправки) с числом их получателей; писем — проверка на `MAIL`, адресатов — до приёма письма с известными
//! получателями. Счётчики — в памяти узла, как и [`tp_platform::RateLimiter`], общие для submission и JMAP;
//! ключ — идентификатор учётной записи; число учётных записей ограничено `smtp.limits.max_tracked_accounts`.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Окно лимита.
const HOUR: Duration = Duration::from_secs(3600);

/// Текст ответа `451 4.7.1` при превышении лимита писем в час.
pub(crate) const MESSAGES_EXCEEDED: &str = "Hourly message limit exceeded, try again later";
/// Текст ответа `451 4.7.1` при превышении лимита адресатов в час.
pub(crate) const RCPTS_EXCEEDED: &str = "Hourly recipient limit exceeded, try again later";

#[derive(Debug, Clone, Copy)]
struct Window {
    start: Instant,
    /// Счёт текущего часа: писем, адресатов.
    cur: [u64; 2],
    /// Счёт предыдущего часа.
    prev: [u64; 2],
}

/// Счётчик окна: письма.
const MESSAGES: usize = 0;
/// Счётчик окна: адресаты.
const RCPTS: usize = 1;

impl Window {
    fn new(now: Instant) -> Self {
        Self {
            start: now,
            cur: [0; 2],
            prev: [0; 2],
        }
    }

    /// Сдвигает окно к `now`.
    fn roll(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.start);
        if elapsed >= 2 * HOUR {
            *self = Self::new(now);
        } else if elapsed >= HOUR {
            self.prev = self.cur;
            self.cur = [0; 2];
            self.start += HOUR;
        }
    }

    /// Оценка счётчика `which` за последний час.
    fn estimate(&self, which: usize, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.start).as_secs_f64();
        let rest = (1.0 - elapsed / HOUR.as_secs_f64()).clamp(0.0, 1.0);
        self.cur[which] as f64 + self.prev[which] as f64 * rest
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

    /// Оценка счётчика `which` учётной записи `account` за последний час и добавка `more` в пределах `per_hour`.
    fn allow(&self, which: usize, per_hour: u32, account: &str, more: u64, now: Instant) -> bool {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let sent = match state.get_mut(account) {
            Some(w) => {
                w.roll(now);
                w.estimate(which, now)
            }
            None => 0.0,
        };
        sent + more as f64 <= f64::from(per_hour)
    }

    /// Можно начать ещё одно письмо учётной записи `account` при лимите `per_hour` писем в час.
    pub fn allow_message_at(&self, per_hour: u32, account: &str, now: Instant) -> bool {
        self.allow(MESSAGES, per_hour, account, 1, now)
    }

    /// Можно принять письмо учётной записи `account` с `rcpts` получателями при лимите `per_hour` адресатов в час.
    pub fn allow_rcpts_at(&self, per_hour: u32, account: &str, rcpts: usize, now: Instant) -> bool {
        self.allow(RCPTS, per_hour, account, rcpts as u64, now)
    }

    /// Учитывает принятое письмо учётной записи `account` с `rcpts` получателями; под наблюдением не больше
    /// `max_tracked` учётных записей.
    pub fn record_at(&self, max_tracked: usize, account: &str, rcpts: usize, now: Instant) {
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
        w.cur[MESSAGES] += 1;
        w.cur[RCPTS] += rcpts as u64;
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
