//! Метрики SMTP (SRS 6.4) в реестре узла [`tp_platform::Metrics`] (`/metrics`):
//!
//! - `tp_smtp_sessions_total{role}`, `tp_smtp_sessions_active{role}` — сессии smtp-in и submission;
//! - `tp_smtp_connections_refused_total{role}` — отказ до сессии (лимит соединений, частота с адреса);
//! - `tp_smtp_messages_total{role,result}` — письма после данных: `accepted` (2xx) или `rejected`;
//! - `tp_smtp_send_limited_total{kind}` — отказы по лимитам отправки (`messages`, `recipients`);
//! - `tp_smtp_queue_claimed_total`, `tp_smtp_queue_inflight` — захваченные и обрабатываемые записи очереди;
//! - `tp_smtp_queue_recipients_total{result}` — итоги попыток по получателям: `delivered`, `deferred`, `failed`;
//! - `tp_smtp_queue_delivery_seconds` — время от постановки в очередь до доставки.

use std::fmt;

use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};

use crate::server::Mode;

type Labels1 = [(&'static str, &'static str); 1];
type Labels2 = [(&'static str, &'static str); 2];

/// Метрики SMTP узла. Клоны разделяют значения.
#[derive(Clone)]
pub struct SmtpMetrics {
    sessions: Family<Labels1, Counter>,
    active: Family<Labels1, Gauge>,
    refused: Family<Labels1, Counter>,
    messages: Family<Labels2, Counter>,
    limited: Family<Labels1, Counter>,
    claimed: Counter,
    inflight: Gauge,
    rcpts: Family<Labels1, Counter>,
    delivery: Histogram,
}

impl Default for SmtpMetrics {
    fn default() -> Self {
        Self {
            sessions: Family::default(),
            active: Family::default(),
            refused: Family::default(),
            messages: Family::default(),
            limited: Family::default(),
            claimed: Counter::default(),
            inflight: Gauge::default(),
            rcpts: Family::default(),
            // 1 с … ~4,5 сут.
            delivery: Histogram::new(exponential_buckets(1.0, 4.0, 10)),
        }
    }
}

impl fmt::Debug for SmtpMetrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SmtpMetrics").finish_non_exhaustive()
    }
}

/// Итог попытки доставки получателю.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RcptOutcome {
    /// Доставлено.
    Delivered,
    /// Отложено до повтора.
    Deferred,
    /// Окончательный отказ.
    Failed,
}

/// Активная сессия: уменьшает `tp_smtp_sessions_active` при завершении.
#[derive(Debug)]
pub struct SessionGuard {
    gauge: Gauge,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.gauge.dec();
    }
}

impl SmtpMetrics {
    /// Новые метрики (не зарегистрированы).
    pub fn new() -> Self {
        Self::default()
    }

    /// Регистрирует метрики в реестре узла.
    pub fn register(&self, registry: &tp_platform::Metrics) {
        registry.register("tp_smtp_sessions", "Сессии SMTP", self.sessions.clone());
        registry.register(
            "tp_smtp_sessions_active",
            "Активные сессии SMTP",
            self.active.clone(),
        );
        registry.register(
            "tp_smtp_connections_refused",
            "Соединения SMTP, отклонённые до сессии",
            self.refused.clone(),
        );
        registry.register(
            "tp_smtp_messages",
            "Письма SMTP после данных",
            self.messages.clone(),
        );
        registry.register(
            "tp_smtp_send_limited",
            "Отказы по лимитам отправки учётной записи",
            self.limited.clone(),
        );
        registry.register(
            "tp_smtp_queue_claimed",
            "Захваченные записи очереди исходящей почты",
            self.claimed.clone(),
        );
        registry.register(
            "tp_smtp_queue_inflight",
            "Обрабатываемые записи очереди исходящей почты",
            self.inflight.clone(),
        );
        registry.register(
            "tp_smtp_queue_recipients",
            "Итоги попыток доставки по получателям",
            self.rcpts.clone(),
        );
        registry.register(
            "tp_smtp_queue_delivery_seconds",
            "Время от постановки в очередь до доставки",
            self.delivery.clone(),
        );
    }

    /// Начало сессии роли `mode`.
    pub fn session(&self, mode: Mode) -> SessionGuard {
        let role = [("role", mode.name())];
        self.sessions.get_or_create(&role).inc();
        let gauge = self.active.get_or_create(&role).clone();
        gauge.inc();
        SessionGuard { gauge }
    }

    /// Соединение отклонено до сессии.
    pub fn refused(&self, mode: Mode) {
        self.refused.get_or_create(&[("role", mode.name())]).inc();
    }

    /// Письмо после данных: принято (`accepted`) или отклонено.
    pub fn message(&self, mode: Mode, accepted: bool) {
        let result = if accepted { "accepted" } else { "rejected" };
        self.messages
            .get_or_create(&[("role", mode.name()), ("result", result)])
            .inc();
    }

    /// Отказ по лимиту отправки: `messages` или `recipients`.
    pub fn limited(&self, kind: &'static str) {
        self.limited.get_or_create(&[("kind", kind)]).inc();
    }

    /// Захвачено `n` записей очереди.
    pub fn claimed(&self, n: usize) {
        self.claimed.inc_by(n as u64);
    }

    /// Обработка записи очереди: `+1` в начале, `-1` в конце.
    pub fn inflight(&self, delta: i64) {
        self.inflight.inc_by(delta);
    }

    /// Итог попытки по получателю; для доставленных — время в очереди, секунд.
    pub fn rcpt(&self, outcome: RcptOutcome, queued_secs: f64) {
        let result = match outcome {
            RcptOutcome::Delivered => "delivered",
            RcptOutcome::Deferred => "deferred",
            RcptOutcome::Failed => "failed",
        };
        self.rcpts.get_or_create(&[("result", result)]).inc();
        if outcome == RcptOutcome::Delivered {
            self.delivery.observe(queued_secs.max(0.0));
        }
    }
}
