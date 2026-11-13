//! Общее у воркеров сроков роли maintenance (истечение корзины, хранение отключённых ящиков): часы и таймер,
//! подписка на системные сигналы шины с переподпиской (SRS 2.8).
//!
//! Подписка оформляется до сверки срока, поэтому сигнал между ними не теряется. Поток завершился (потеря связи) —
//! переподписка и сверка; поток закрылся быстрее паузы `retry` после оформления или подписка не оформлена —
//! следующая попытка через `retry` (без частых переподписок).

use std::future::Future;
use std::time::Duration;

use futures::StreamExt;
use time::{OffsetDateTime, PrimitiveDateTime};
use tp_contracts::bus::{EventBus, EventKind, EventStream};

/// Часы и таймер воркера: в работе — системные ([`SystemTimer`]), в тестах — часы testkit.
pub(crate) trait Timer: Send + Sync {
    /// Текущий момент (UTC).
    fn now(&self) -> OffsetDateTime;
    /// Завершается не раньше момента `at`.
    fn sleep_until(&self, at: OffsetDateTime) -> impl Future<Output = ()> + Send;
}

/// Системные часы и таймер tokio.
pub(crate) struct SystemTimer;

impl Timer for SystemTimer {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn sleep_until(&self, at: OffsetDateTime) -> impl Future<Output = ()> + Send {
        let wait = Duration::try_from(at - OffsetDateTime::now_utc()).unwrap_or(Duration::ZERO);
        tokio::time::sleep(wait)
    }
}

/// Момент позже любого срока: выборка истёкшего на него — записи по возрастанию срока.
pub(crate) const FAR: OffsetDateTime = PrimitiveDateTime::MAX.assume_utc();

/// `at + d`; вне диапазона — `FAR`.
pub(crate) fn after(at: OffsetDateTime, d: Duration) -> OffsetDateTime {
    time::Duration::try_from(d)
        .ok()
        .and_then(|d| at.checked_add(d))
        .unwrap_or(FAR)
}

/// Подписка воркера на системные сигналы шины (`EventBus::subscribe_system`) с переподпиской.
pub(crate) struct Signals {
    /// `None` — подписка не оформлена.
    events: Option<EventStream>,
    /// Момент оформления текущей подписки.
    opened: OffsetDateTime,
    /// Следующая попытка подписаться.
    resubscribe: OffsetDateTime,
    /// Пауза между попытками.
    retry: Duration,
    /// Чьи сигналы — для журнала («корзины», «отключённых ящиков»).
    what: &'static str,
}

impl Signals {
    /// Без подписки; первая попытка — сразу.
    pub(crate) fn new(now: OffsetDateTime, retry: Duration, what: &'static str) -> Self {
        Self {
            events: None,
            opened: now,
            resubscribe: now,
            retry,
            what,
        }
    }

    /// Оформляет подписку, если её нет и пора. Вызывается до сверки срока.
    pub(crate) async fn ensure(&mut self, bus: &dyn EventBus, now: OffsetDateTime) {
        if self.events.is_some() || now < self.resubscribe {
            return;
        }
        self.opened = now;
        match bus.subscribe_system().await {
            Ok(s) => self.events = Some(s),
            Err(e) => {
                tracing::warn!(error = %e, "mailstore: подписка на сигналы {} не оформлена", self.what);
                self.resubscribe = after(now, self.retry);
            }
        }
    }

    /// Следующая попытка подписаться, пока подписки нет; есть подписка — `None`.
    pub(crate) fn pending(&self) -> Option<OffsetDateTime> {
        self.events.is_none().then_some(self.resubscribe)
    }

    /// Завершается, когда срок пора сверить: сигнал вида `relevant`, `Resync` подписки или завершение потока
    /// (подписка снимается, переподписка — [`Signals::ensure`]). Без подписки — никогда. Отмена безопасна:
    /// другие сигналы пропускаются внутри.
    pub(crate) async fn changed(&mut self, timer: &impl Timer, relevant: fn(&EventKind) -> bool) {
        loop {
            let Some(events) = &mut self.events else {
                return std::future::pending().await;
            };
            match events.next().await.map(|e| e.kind) {
                Some(EventKind::Resync) => return,
                Some(kind) if relevant(&kind) => return,
                Some(_) => {}
                None => {
                    self.events = None;
                    let now = timer.now();
                    let soon = after(self.opened, self.retry);
                    self.resubscribe = if now < soon {
                        after(now, self.retry)
                    } else {
                        now
                    };
                    return;
                }
            }
        }
    }
}
