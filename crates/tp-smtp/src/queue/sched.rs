//! Планировщик исходящей очереди по доменам назначения (SRS 4.1, модель qmgr Postfix; решение
//! 2026-11-01-borrowings-security). Состояние — в памяти узла smtp-out: узлы без общего состояния, каждый
//! ограничивает свои отправки.
//!
//! - Параллельность на домен: окно медленного старта — с `initial_concurrency`, +1 после каждой отправки с
//!   ответом сервера, до `domain_concurrency`; недоступность домена возвращает окно к начальному.
//! - Скорость на домен: не больше `domain_rate` отправок за `rate_window` (`0` — без лимита).
//! - Недоступный домен (все получатели — временная неудача без ответа SMTP: соединение, DNS) откладывается:
//!   пауза `backoff_min`, удваивается с каждой неудачей подряд до `backoff_max`. Получатели отложенного
//!   домена переносятся на конец паузы без соединения ([`super::RcptResult::Deferred`]), другие домены той же
//!   записи и других записей доставляются как обычно.
//! - Раздельные пулы внутренней (домены кластера и `internal_domains`) и внешней доставки:
//!   `internal_concurrency` и `external_concurrency` одновременных отправок — внешняя очередь не занимает
//!   места внутренней.
//! - Нет места в домене или пуле дольше `wait` — получатели переносятся на `busy_delay`.
//!
//! Метки времени (паузы, окно скорости) — из часов очереди ([`super::Now`]), ожидание места — таймер tokio.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use time::OffsetDateTime;
use tokio::sync::Notify;

use super::{Now, RcptResult};
use crate::config::SchedulerConfig;

/// Пул доставки.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Домены кластера (принятые домены других организаций) и `internal_domains`.
    Internal,
    /// Остальные домены.
    External,
}

/// Решение планировщика для получателей домена.
#[derive(Debug)]
pub enum Admit {
    /// Отправлять; итог — [`Permit::done`].
    Go(Permit),
    /// Отложить без соединения до `until`.
    Defer {
        /// Причина (журнал, `last_error` получателя).
        reason: String,
        /// Срок следующей попытки.
        until: OffsetDateTime,
    },
}

#[derive(Debug)]
struct Domain {
    active: usize,
    window: usize,
    failures: u32,
    dead_until: Option<OffsetDateTime>,
    rate_start: OffsetDateTime,
    rate_count: u32,
}

#[derive(Debug, Default)]
struct State {
    domains: HashMap<String, Domain>,
    internal: usize,
    external: usize,
}

impl State {
    fn class(&mut self, class: Class) -> &mut usize {
        match class {
            Class::Internal => &mut self.internal,
            Class::External => &mut self.external,
        }
    }
}

/// Планировщик узла: окна, паузы и счётчики по доменам, пулы внутренней и внешней доставки.
#[derive(Default)]
pub struct Scheduler {
    state: Mutex<State>,
    freed: Notify,
}

impl fmt::Debug for Scheduler {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Scheduler").finish_non_exhaustive()
    }
}

/// Место отправки в домене и пуле; освобождается при удалении.
pub struct Permit {
    sched: Arc<Scheduler>,
    domain: Option<String>,
    class: Class,
    backoff: (std::time::Duration, std::time::Duration),
    max: usize,
    initial: usize,
    now: Now,
}

impl fmt::Debug for Permit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Permit")
            .field("domain", &self.domain)
            .field("class", &self.class)
            .finish_non_exhaustive()
    }
}

/// Все получатели — временная неудача без ответа сервера (соединение, DNS): домен недоступен.
pub fn unreachable(results: &[RcptResult]) -> bool {
    !results.is_empty()
        && results.iter().all(|r| match r {
            RcptResult::Temporary(reason) => !is_reply(reason),
            _ => false,
        })
}

/// Текст начинается с кода ответа SMTP (`451 …`).
fn is_reply(s: &str) -> bool {
    s.len() >= 3 && s.as_bytes()[..3].iter().all(u8::is_ascii_digit)
}

impl Permit {
    /// Итог отправки: ответ сервера расширяет окно домена, недоступность — пауза и начальное окно.
    pub fn done(&self, results: &[RcptResult]) {
        let Some(name) = &self.domain else {
            return;
        };
        let now = (self.now)();
        let mut st = self.sched.lock();
        let Some(d) = st.domains.get_mut(name) else {
            return;
        };
        if unreachable(results) {
            d.failures = d.failures.saturating_add(1);
            d.window = self.initial;
            let (min, max) = self.backoff;
            let pause = min
                .saturating_mul(1u32 << (d.failures - 1).min(20))
                .min(max.max(min));
            d.dead_until = Some(now + pause);
            tracing::info!(domain = %name, failures = d.failures, pause = ?pause, "smtp: домен недоступен, доставка отложена");
        } else {
            d.failures = 0;
            d.dead_until = None;
            d.window = (d.window + 1).min(self.max);
        }
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        {
            let mut st = self.sched.lock();
            let c = st.class(self.class);
            *c = c.saturating_sub(1);
            if let Some(d) = self.domain.as_ref().and_then(|n| st.domains.get_mut(n)) {
                d.active = d.active.saturating_sub(1);
            }
        }
        self.sched.freed.notify_waiters();
    }
}

impl Scheduler {
    /// Пустой планировщик.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Пул домена `domain` (нижний регистр): `internal_domains` секции или `cluster` (принятые домены).
    pub fn class(cfg: &SchedulerConfig, domain: &str, cluster: impl Fn(&str) -> bool) -> Class {
        if cfg
            .internal_domains
            .iter()
            .any(|d| d.eq_ignore_ascii_case(domain))
            || cluster(domain)
        {
            Class::Internal
        } else {
            Class::External
        }
    }

    /// Место для отправки в домен `domain` пула `class`: сразу, после ожидания до `wait` или отказ с переносом.
    pub async fn acquire(
        self: &Arc<Self>,
        cfg: &SchedulerConfig,
        domain: &str,
        class: Class,
        now: &Now,
    ) -> Admit {
        let deadline = tokio::time::Instant::now() + cfg.wait;
        loop {
            let freed = self.freed.notified();
            tokio::pin!(freed);
            freed.as_mut().enable();
            if let Ok(admit) = self.try_acquire(cfg, domain, class, now) {
                return admit;
            }
            if tokio::time::timeout_at(deadline, freed).await.is_err() {
                return Admit::Defer {
                    reason: format!("domain {domain} busy, deferred by scheduler"),
                    until: now() + cfg.busy_delay,
                };
            }
        }
    }

    /// `Err` — нет места сейчас (ждать).
    fn try_acquire(
        self: &Arc<Self>,
        cfg: &SchedulerConfig,
        domain: &str,
        class: Class,
        now: &Now,
    ) -> Result<Admit, ()> {
        let t = now();
        let initial = cfg
            .initial_concurrency
            .clamp(1, cfg.domain_concurrency.max(1));
        let max = cfg.domain_concurrency.max(1);
        let limit = match class {
            Class::Internal => cfg.internal_concurrency,
            Class::External => cfg.external_concurrency,
        }
        .max(1);
        let mut st = self.lock();
        if *st.class(class) >= limit {
            return Err(());
        }
        if !st.domains.contains_key(domain) && st.domains.len() >= cfg.max_domains.max(1) {
            // Забыть простаивающие домены без паузы.
            st.domains
                .retain(|_, d| d.active > 0 || d.dead_until.is_some_and(|u| u > t));
        }
        let tracked = st.domains.len() < cfg.max_domains.max(1) || st.domains.contains_key(domain);
        if tracked {
            let d = st.domains.entry(domain.to_owned()).or_insert(Domain {
                active: 0,
                window: initial,
                failures: 0,
                dead_until: None,
                rate_start: t,
                rate_count: 0,
            });
            if let Some(until) = d.dead_until.filter(|u| *u > t) {
                return Ok(Admit::Defer {
                    reason: format!("domain {domain} unreachable, delivery deferred"),
                    until,
                });
            }
            if cfg.domain_rate > 0 {
                if t - d.rate_start >= cfg.rate_window {
                    d.rate_start = t;
                    d.rate_count = 0;
                }
                if d.rate_count >= cfg.domain_rate {
                    return Ok(Admit::Defer {
                        reason: format!("domain {domain} rate limit, deferred by scheduler"),
                        until: d.rate_start + cfg.rate_window,
                    });
                }
            }
            if d.active >= d.window.min(max) {
                return Err(());
            }
            d.active += 1;
            d.rate_count += 1;
        }
        *st.class(class) += 1;
        Ok(Admit::Go(Permit {
            sched: self.clone(),
            domain: tracked.then(|| domain.to_owned()),
            class,
            backoff: (cfg.backoff_min, cfg.backoff_max),
            max,
            initial,
            now: now.clone(),
        }))
    }
}
