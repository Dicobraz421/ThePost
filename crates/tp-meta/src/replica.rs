//! Реплика для фоновых чтений `ReadIntent::Stale` (SRS 3.1, решение 2026-11-01-scale-shards-geo-admin, META-32).
//!
//! Закрытый список фоновых чтений (экспорт, отчёты Admin API, eDiscovery, verify, поиск «сирот» BLOB, первичная
//! индексация) выполняется на реплике, если её отставание `now() - pg_last_xact_replay_timestamp()` не больше
//! `database.replica_max_lag` (30 с); иначе, при неизвестном отставании и при недоступности реплики — на primary.
//! Сервер не в режиме восстановления (адрес реплики указывает на primary) отстаёт на ноль. Отмена запроса на
//! реплике из-за конфликта с восстановлением (`40001`) и другие временные отказы — повтор на primary
//! ([`crate::PgMeta::with_intent`]). Хранилища без `with_intent(Stale)` (протокольные роли) реплику не используют.
//!
//! Проверка отставания кэшируется на [`CHECK_EVERY`]: фоновые чтения не добавляют запрос на каждую операцию.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::pool::DbPool;

/// Срок действия проверки отставания.
pub const CHECK_EVERY: Duration = Duration::from_secs(1);

/// Отставание `lag` допустимо при пороге `max`: неизвестное (`None`) — нет.
pub fn fresh_enough(lag: Option<Duration>, max: Duration) -> bool {
    lag.is_some_and(|l| l <= max)
}

/// Пул реплики с проверкой отставания.
#[derive(Debug)]
pub struct Replica {
    pool: DbPool,
    max_lag: Duration,
    checked: Mutex<Option<(Instant, bool)>>,
}

impl Replica {
    /// Реплика `pool` с порогом отставания `max_lag`.
    pub fn new(pool: DbPool, max_lag: Duration) -> Self {
        Self {
            pool,
            max_lag,
            checked: Mutex::new(None),
        }
    }

    /// Пул реплики.
    pub fn pool(&self) -> &DbPool {
        &self.pool
    }

    /// Отставание реплики; `None` — неизвестно (ничего не воспроизведено). Ошибка — реплика недоступна.
    pub async fn lag(&self) -> sqlx::Result<Option<Duration>> {
        let mut conn = self.pool.acquire_sqlx().await?;
        let secs: Option<f64> = sqlx::query_scalar(
            "SELECT CASE WHEN pg_is_in_recovery()
                         THEN extract(epoch FROM now() - pg_last_xact_replay_timestamp())::float8
                         ELSE 0 END",
        )
        .fetch_one(&mut *conn)
        .await?;
        Ok(secs.map(|s| Duration::from_secs_f64(s.max(0.0))))
    }

    /// Реплику можно читать: отставание не больше порога (проверка — не чаще [`CHECK_EVERY`]).
    pub async fn usable(&self) -> bool {
        let now = Instant::now();
        if let Some((at, ok)) = *self.state()
            && now.duration_since(at) < CHECK_EVERY
        {
            return ok;
        }
        let ok = match self.lag().await {
            Ok(lag) => {
                let ok = fresh_enough(lag, self.max_lag);
                if !ok {
                    tracing::debug!(?lag, max = ?self.max_lag, "реплика отстаёт — чтение на primary");
                }
                ok
            }
            Err(e) => {
                tracing::warn!(error = %e, "реплика недоступна — чтение на primary");
                false
            }
        };
        *self.state() = Some((Instant::now(), ok));
        ok
    }

    fn state(&self) -> std::sync::MutexGuard<'_, Option<(Instant, bool)>> {
        self.checked
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lag_threshold() {
        let max = Duration::from_secs(30);
        assert!(fresh_enough(Some(Duration::ZERO), max));
        assert!(fresh_enough(Some(max), max));
        assert!(!fresh_enough(Some(max + Duration::from_millis(1)), max));
        assert!(!fresh_enough(None, max), "отставание неизвестно — primary");
    }
}
