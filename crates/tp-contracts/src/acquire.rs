//! Наблюдатель ожидания соединения с СУБД (CONTRACTS.md#Metadata, CTR-51; решение 57).
//!
//! Хранилище метаданных не зависит от крейта метрик: оно получает [`AcquireObserver`] и сообщает ему о каждом
//! ожидании соединения пула (`pool.acquire()`, `pool.begin()`). Реализация с метриками — `tp-platform`
//! (число ожидающих, время ожидания, счётчик отказов); без метрик — [`NoAcquireObserver`].
//!
//! Начало ожидания возвращает сторож [`AcquireWait`]. Сторож завершается исходом — [`AcquireWait::acquired`] или
//! [`AcquireWait::failed`] (в том числе тайм-аут пула). Сторож, брошенный без исхода (задача отменена), снимает
//! «ожидает» без записи времени: [`AcquireEnd::Cancelled`].
//!
//! ```
//! use std::sync::Arc;
//! use tp_contracts::acquire::{AcquireCounter, AcquireObserver};
//!
//! # async fn connect() -> Result<u32, &'static str> { Ok(7) }
//! # tokio::runtime::Builder::new_current_thread().build().expect("rt").block_on(async {
//! let counter = Arc::new(AcquireCounter::default());
//! let observer: Arc<dyn AcquireObserver> = counter.clone();
//!
//! // Короткая запись: исход — по результату получения соединения.
//! let conn = observer.observe(connect()).await;
//! assert_eq!(conn, Ok(7));
//!
//! // То же вручную.
//! let wait = observer.wait();
//! assert_eq!(counter.waiting(), 1);
//! wait.failed();
//!
//! assert_eq!((counter.waiting(), counter.acquired(), counter.failed()), (0, 1, 1));
//! # });
//! ```

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Чем закончилось ожидание соединения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AcquireEnd {
    /// Соединение получено; время ожидания.
    Acquired(Duration),
    /// Отказ пула, в том числе тайм-аут ожидания; время ожидания.
    Failed(Duration),
    /// Ожидание брошено без исхода (задача отменена): время не записывается, отказом не считается.
    Cancelled,
}

/// Наблюдатель ожидания соединения пула. Вызовы парные: на каждый [`started`](Self::started) — ровно один
/// [`finished`](Self::finished); пару обеспечивает сторож [`AcquireWait`], вызывать методы напрямую не нужно.
///
/// Методы вызываются на пути каждого запроса: без блокировок, ввода-вывода и паники.
pub trait AcquireObserver: Send + Sync {
    /// Началось ожидание соединения: число ожидающих +1.
    fn started(&self) {}

    /// Ожидание закончилось: число ожидающих −1; время и отказ — по исходу.
    fn finished(&self, end: AcquireEnd) {
        let _ = end;
    }
}

impl dyn AcquireObserver + '_ {
    /// Начинает ожидание соединения и возвращает сторож.
    pub fn wait(&self) -> AcquireWait<'_> {
        AcquireWait::start(self)
    }

    /// Оборачивает получение соединения: `Ok` — «получено», `Err` — «отказ», отмена — «брошено».
    pub async fn observe<T, E>(&self, acquire: impl Future<Output = Result<T, E>>) -> Result<T, E> {
        let wait = self.wait();
        let out = acquire.await;
        match &out {
            Ok(_) => wait.acquired(),
            Err(_) => wait.failed(),
        }
        out
    }
}

/// Сторож одного ожидания соединения. Завершается [`acquired`](Self::acquired) или [`failed`](Self::failed);
/// брошенный без исхода сообщает наблюдателю [`AcquireEnd::Cancelled`].
#[must_use = "dropping the guard reports a cancelled wait"]
pub struct AcquireWait<'a> {
    observer: &'a dyn AcquireObserver,
    since: Instant,
    done: bool,
}

impl<'a> AcquireWait<'a> {
    /// Начинает ожидание: сообщает наблюдателю [`AcquireObserver::started`].
    pub fn start(observer: &'a dyn AcquireObserver) -> Self {
        observer.started();
        AcquireWait {
            observer,
            since: Instant::now(),
            done: false,
        }
    }

    /// Соединение получено.
    pub fn acquired(mut self) {
        self.done = true;
        self.observer
            .finished(AcquireEnd::Acquired(self.since.elapsed()));
    }

    /// Отказ пула (ошибка или тайм-аут ожидания).
    pub fn failed(mut self) {
        self.done = true;
        self.observer
            .finished(AcquireEnd::Failed(self.since.elapsed()));
    }
}

impl Drop for AcquireWait<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.observer.finished(AcquireEnd::Cancelled);
        }
    }
}

impl fmt::Debug for AcquireWait<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AcquireWait")
            .field("since", &self.since)
            .finish_non_exhaustive()
    }
}

/// Пустой наблюдатель: хранилище без метрик (значение по умолчанию у потребителя).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NoAcquireObserver;

impl AcquireObserver for NoAcquireObserver {}

/// Счётчик ожиданий в памяти: тесты потребителей и простая диагностика.
#[derive(Debug, Default)]
pub struct AcquireCounter {
    waiting: AtomicU64,
    acquired: AtomicU64,
    failed: AtomicU64,
    cancelled: AtomicU64,
    waited_nanos: AtomicU64,
}

impl AcquireCounter {
    /// Сколько задач сейчас ждёт соединения.
    pub fn waiting(&self) -> u64 {
        self.waiting.load(Ordering::Relaxed)
    }

    /// Сколько ожиданий закончилось получением соединения.
    pub fn acquired(&self) -> u64 {
        self.acquired.load(Ordering::Relaxed)
    }

    /// Сколько ожиданий закончилось отказом.
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }

    /// Сколько ожиданий брошено без исхода.
    pub fn cancelled(&self) -> u64 {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Число записей времени ожидания: «получено» и «отказ», без брошенных.
    pub fn observed(&self) -> u64 {
        self.acquired() + self.failed()
    }

    /// Суммарное записанное время ожидания.
    pub fn waited(&self) -> Duration {
        Duration::from_nanos(self.waited_nanos.load(Ordering::Relaxed))
    }
}

impl AcquireObserver for AcquireCounter {
    fn started(&self) {
        self.waiting.fetch_add(1, Ordering::Relaxed);
    }

    fn finished(&self, end: AcquireEnd) {
        // Непарный вызов не должен уводить счётчик ниже нуля.
        let _ = self
            .waiting
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));
        let (counter, waited) = match end {
            AcquireEnd::Acquired(d) => (&self.acquired, Some(d)),
            AcquireEnd::Failed(d) => (&self.failed, Some(d)),
            AcquireEnd::Cancelled => (&self.cancelled, None),
        };
        counter.fetch_add(1, Ordering::Relaxed);
        if let Some(d) = waited {
            let nanos = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
            let _ = self
                .waited_nanos
                .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                    Some(n.saturating_add(nanos))
                });
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    #[tokio::test]
    async fn outcome_is_counted() {
        let counter = Arc::new(AcquireCounter::default());
        let observer: Arc<dyn AcquireObserver> = counter.clone();

        let wait = observer.wait();
        assert_eq!(counter.waiting(), 1);
        tokio::time::sleep(Duration::from_millis(5)).await;
        wait.acquired();
        assert_eq!(counter.waiting(), 0);
        assert_eq!((counter.acquired(), counter.failed()), (1, 0));
        assert!(counter.waited() >= Duration::from_millis(5));

        let ok: Result<u8, ()> = observer.observe(async { Ok(1) }).await;
        assert_eq!(ok, Ok(1));
        let err: Result<u8, &str> = observer.observe(async { Err("pool timed out") }).await;
        assert_eq!(err, Err("pool timed out"));
        assert_eq!(counter.waiting(), 0);
        assert_eq!((counter.acquired(), counter.failed()), (2, 1));
        assert_eq!((counter.observed(), counter.cancelled()), (3, 0));
    }

    #[tokio::test]
    async fn cancel_leaves_no_waiting() {
        let counter = Arc::new(AcquireCounter::default());
        let observer: Arc<dyn AcquireObserver> = counter.clone();

        // Задача ждёт соединения, которого не будет, и отменяется.
        let task = {
            let observer = observer.clone();
            tokio::spawn(async move {
                observer
                    .observe(std::future::pending::<Result<(), ()>>())
                    .await
            })
        };
        while counter.waiting() == 0 {
            tokio::task::yield_now().await;
        }
        task.abort();
        assert!(task.await.expect_err("aborted").is_cancelled());

        assert_eq!(counter.waiting(), 0);
        assert_eq!((counter.cancelled(), counter.observed()), (1, 0));
        assert_eq!(counter.waited(), Duration::ZERO);

        // Сторож, брошенный вручную, — то же самое.
        drop(observer.wait());
        assert_eq!((counter.waiting(), counter.cancelled()), (0, 2));
    }

    #[test]
    fn noop_and_unpaired() {
        let none: &dyn AcquireObserver = &NoAcquireObserver;
        none.wait().acquired();
        none.wait().failed();
        drop(none.wait());

        let counter = AcquireCounter::default();
        counter.finished(AcquireEnd::Cancelled);
        assert_eq!(counter.waiting(), 0);

        fn object_safe(_: &dyn AcquireObserver) {}
        fn send_sync<T: Send + Sync + ?Sized>() {}
        object_safe(&counter);
        send_sync::<dyn AcquireObserver>();
        send_sync::<AcquireWait<'_>>();
    }
}
