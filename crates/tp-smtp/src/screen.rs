//! Экран соединений smtp-in (SRS 4.1, модель postscreen Postfix; решение 2026-11-01-borrowings-security).
//!
//! До сессии ([`Screen::admit`]): частота соединений с адреса (`screen.max_rate` за `screen.rate_window`) и
//! кэш результатов по IP-адресу — прошедший проверки адрес не проверяется `screen.pass_ttl`, нарушитель
//! отклоняется без проверки `screen.reject_ttl`. Адреса из `screen.exempt` не проверяются. Адрес клиента приходит
//! в канонической форме (слушатель — PLT-34, `XCLIENT`/`XFORWARD` — разбор [`crate::sources`]); к ней приводятся
//! только одиночные адреса `screen.exempt` из конфигурации.
//!
//! В начале сессии ([`Probe`], проверки ведёт [`crate::server::Session`]):
//! - задержка приветствия: сервер отправляет первую строку `220-` и ждёт `screen.greet_delay`; клиент,
//!   заговоривший до конца приветствия, нарушает протокол (RFC 5321, 4.3.1);
//! - конвейер команд без PIPELINING (RFC 2920): несколько команд одним пакетом до `EHLO`, после `HELO` или
//!   вслед за `EHLO` (он должен быть последним в группе, RFC 2920, 3.1).
//!
//! Нарушение в режиме `enforce` — отказ (`554 5.5.1`; частота — `421 4.7.0`) и закрытие соединения, адрес
//! запоминается как нарушитель; в режиме `log` — только журнал и метрика, сессия продолжается.
//! Метрика — `tp_smtp_screen_violations_total{reason,action}` ([`crate::SmtpMetrics::screen`]).
//! Репутационные списки DNS (DNSBL, DNSWL) — средствами Rspamd через Milter.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tp_contracts::transport::IpNet;

use crate::config::{ScreenConfig, ScreenMode};
use crate::metrics::SmtpMetrics;
use crate::server::{Reply, reply};

/// Нарушение, обнаруженное экраном.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Violation {
    /// Передача до приветствия сервера.
    Pregreet,
    /// Конвейер команд без PIPELINING.
    Pipelining,
    /// Превышена частота соединений с адреса.
    Rate,
}

impl Violation {
    /// Имя для журнала и метрик.
    pub fn as_str(self) -> &'static str {
        match self {
            Violation::Pregreet => "pregreet",
            Violation::Pipelining => "pipelining",
            Violation::Rate => "rate",
        }
    }

    /// Ответ клиенту при отказе.
    pub fn reply(self) -> Reply {
        match self {
            Violation::Pregreet => reply(554, [5, 5, 1], "Protocol error: talked before greeting"),
            Violation::Pipelining => {
                reply(554, [5, 5, 1], "Protocol error: pipelining not negotiated")
            }
            Violation::Rate => reply(421, [4, 7, 0], "Too many connections, try later"),
        }
    }
}

/// Проверки начала сессии для клиента без результата в кэше.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    /// Задержка приветствия; ноль — без проверки.
    pub greet_delay: Duration,
    /// Проверять конвейер команд без PIPELINING.
    pub pipelining: bool,
    /// Нарушение — отказ (`enforce`); иначе только журнал.
    pub enforce: bool,
}

impl Probe {
    /// Проверки по секции `screen`.
    pub fn from_config(cfg: &ScreenConfig) -> Self {
        Self {
            greet_delay: cfg.greet_delay,
            pipelining: cfg.pipelining,
            enforce: cfg.mode == ScreenMode::Enforce,
        }
    }
}

/// Решение экрана до сессии.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Адрес из `screen.exempt` или экран выключен: без проверок.
    Exempt,
    /// Адрес прошёл проверки ранее (кэш): без проверок.
    Passed,
    /// Сессия с проверками; итог — [`Screen::record`].
    Probe(Probe),
    /// Отказ: нарушение сейчас или из кэша (`cached`).
    Reject {
        /// Нарушение.
        violation: Violation,
        /// Результат взят из кэша.
        cached: bool,
    },
}

#[derive(Debug, Clone, Copy)]
enum Verdict {
    Pass,
    Reject(Violation),
}

#[derive(Debug)]
struct Entry {
    verdict: Verdict,
    until: Instant,
}

#[derive(Debug, Default)]
struct State {
    cache: HashMap<IpAddr, Entry>,
    /// Окно частоты: начало и число соединений.
    rate: HashMap<IpAddr, (Instant, u32)>,
}

/// Экран соединений узла: кэш результатов и счётчики частоты по адресам.
#[derive(Default)]
pub struct Screen {
    state: Mutex<State>,
    metrics: Option<Arc<SmtpMetrics>>,
}

impl fmt::Debug for Screen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Screen").finish_non_exhaustive()
    }
}

impl Screen {
    /// Экран с метриками `metrics`.
    pub fn new(metrics: Option<Arc<SmtpMetrics>>) -> Self {
        Self {
            state: Mutex::default(),
            metrics,
        }
    }

    /// Решение для нового соединения с адреса `ip`.
    pub fn admit(&self, cfg: &ScreenConfig, ip: IpAddr) -> Admission {
        self.admit_at(cfg, ip, Instant::now())
    }

    /// [`Screen::admit`] в момент `now`.
    pub fn admit_at(&self, cfg: &ScreenConfig, ip: IpAddr, now: Instant) -> Admission {
        if !cfg.enabled || exempt(cfg, ip) {
            return Admission::Exempt;
        }
        let enforce = cfg.mode == ScreenMode::Enforce;
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let cached = st
            .cache
            .get(&ip)
            .filter(|e| e.until > now)
            .map(|e| e.verdict);
        if let Some(Verdict::Reject(v)) = cached
            && enforce
        {
            drop(st);
            self.count(v, "cached");
            return Admission::Reject {
                violation: v,
                cached: true,
            };
        }
        if cfg.max_rate > 0 && self.over_rate(&mut st, cfg, ip, now) {
            if enforce {
                insert(
                    &mut st.cache,
                    cfg,
                    ip,
                    Verdict::Reject(Violation::Rate),
                    now,
                );
            }
            drop(st);
            self.violation(ip, Violation::Rate, enforce);
            if enforce {
                return Admission::Reject {
                    violation: Violation::Rate,
                    cached: false,
                };
            }
        }
        match cached {
            Some(Verdict::Pass) => Admission::Passed,
            _ => Admission::Probe(Probe::from_config(cfg)),
        }
    }

    /// Итог сессии с проверками: `Some(Ok)` — пройдены, `Some(Err)` — нарушение, `None` — неизвестно
    /// (клиент ушёл до первой команды).
    pub fn record(&self, cfg: &ScreenConfig, ip: IpAddr, outcome: Option<Result<(), Violation>>) {
        self.record_at(cfg, ip, outcome, Instant::now());
    }

    /// [`Screen::record`] в момент `now`.
    pub fn record_at(
        &self,
        cfg: &ScreenConfig,
        ip: IpAddr,
        outcome: Option<Result<(), Violation>>,
        now: Instant,
    ) {
        let verdict = match outcome {
            Some(Ok(())) => Verdict::Pass,
            Some(Err(v)) if cfg.mode == ScreenMode::Enforce => Verdict::Reject(v),
            // Режим журнала: нарушитель проверяется снова при следующем соединении.
            Some(Err(_)) | None => return,
        };
        let mut st = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        insert(&mut st.cache, cfg, ip, verdict, now);
    }

    /// Нарушение: журнал и метрика (`rejected` или `logged`).
    pub fn violation(&self, ip: IpAddr, v: Violation, enforce: bool) {
        violation(self.metrics.as_deref(), ip, v, enforce);
    }

    fn count(&self, v: Violation, action: &'static str) {
        if let Some(m) = &self.metrics {
            m.screen(v.as_str(), action);
        }
    }

    /// Соединение с `ip` в окне частоты; `true` — лимит превышен.
    fn over_rate(&self, st: &mut State, cfg: &ScreenConfig, ip: IpAddr, now: Instant) -> bool {
        let window = cfg.rate_window;
        if st.rate.len() >= cfg.cache_size.max(1) && !st.rate.contains_key(&ip) {
            st.rate
                .retain(|_, (start, _)| now.saturating_duration_since(*start) < window);
            if st.rate.len() >= cfg.cache_size.max(1) {
                // Таблица полна: адрес не учитывается (без роста памяти).
                return false;
            }
        }
        let (start, n) = st.rate.entry(ip).or_insert((now, 0));
        if now.saturating_duration_since(*start) >= window {
            *start = now;
            *n = 0;
        }
        *n = n.saturating_add(1);
        *n > cfg.max_rate
    }
}

/// Журнал и метрика нарушения (сессия и экран).
pub(crate) fn violation(metrics: Option<&SmtpMetrics>, ip: IpAddr, v: Violation, enforce: bool) {
    let action = if enforce { "rejected" } else { "logged" };
    tracing::info!(client = %ip, reason = v.as_str(), action, "smtp: экран соединений");
    if let Some(m) = metrics {
        m.screen(v.as_str(), action);
    }
}

fn exempt(cfg: &ScreenConfig, ip: IpAddr) -> bool {
    cfg.exempt.iter().any(|n| match n.parse::<IpNet>() {
        Ok(net) => net.contains(ip),
        // Одиночный адрес без префикса.
        Err(_) => n.parse::<IpAddr>().is_ok_and(|a| a.to_canonical() == ip),
    })
}

fn insert(
    cache: &mut HashMap<IpAddr, Entry>,
    cfg: &ScreenConfig,
    ip: IpAddr,
    verdict: Verdict,
    now: Instant,
) {
    let ttl = match verdict {
        Verdict::Pass => cfg.pass_ttl,
        Verdict::Reject(_) => cfg.reject_ttl,
    };
    if ttl.is_zero() {
        cache.remove(&ip);
        return;
    }
    if cache.len() >= cfg.cache_size.max(1) && !cache.contains_key(&ip) {
        cache.retain(|_, e| e.until > now);
        if cache.len() >= cfg.cache_size.max(1) {
            return;
        }
    }
    cache.insert(
        ip,
        Entry {
            verdict,
            until: now + ttl,
        },
    );
}

#[cfg(test)]
mod tests;
