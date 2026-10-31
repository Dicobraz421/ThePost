//! Динамические учётные данные PostgreSQL (META-18; решение 2026-10-30 «Секреты — только через хранилище секретов»).
//!
//! По настройке `database.dynamic_credentials` логин и пароль роли выдаёт database secrets engine OpenBao/Vault:
//! короткоживущая роль с арендой (lease), контракт [`SecretStore`] (CTR-15). Пул с ротацией —
//! [`crate::pool::connect_rotating`]: задача ротации продлевает аренду, когда прошла доля срока
//! `renew_at_percent`. Отказ продления, непродлеваемая аренда или срок, урезанный хранилищем (`max_ttl`), —
//! новые учётные данные и [`PgPool::set_connect_options`]: новые соединения открываются с ними, текущие
//! запросы не обрываются, соединение со старыми учётными данными закрывается при следующей выдаче из пула.
//! Отказ хранилища — повтор через `retry_interval`. Задача завершается с закрытием пула.
//!
//! ```yaml
//! database:
//!   url: postgres://db.example.ru:5432/thepost
//!   dynamic_credentials:
//!     username: { secret: "database/creds/thepost#username" }
//!     password: { secret: "database/creds/thepost#password" }
//! ```

use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use sqlx::postgres::PgConnectOptions;
use tp_contracts::config::duration;
use tp_contracts::secrets::{Secret, SecretLease, SecretRef, SecretStore};
use tp_contracts::{Error, Result};

use crate::pool::{PoolConfig, fetch_secret};

/// Секция `database.dynamic_credentials`: логин и пароль из одной выдачи database secrets engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DynamicCredentials {
    /// Логин роли — поле выдачи, например `{ secret: "database/creds/thepost#username" }`.
    pub username: SecretRef,
    /// Пароль роли — поле той же выдачи, например `{ secret: "database/creds/thepost#password" }`.
    pub password: SecretRef,
    /// Аренда продлевается, когда прошла эта доля её срока, % (от 10 до 90).
    #[serde(default = "default_renew_at_percent")]
    pub renew_at_percent: u8,
    /// Повтор после отказа хранилища секретов.
    #[serde(default = "default_retry_interval", with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retry_interval: Duration,
}

fn default_renew_at_percent() -> u8 {
    67
}

fn default_retry_interval() -> Duration {
    Duration::from_secs(5)
}

impl DynamicCredentials {
    /// Проверка параметров (`thepost config check` и запуск).
    pub fn check(&self) -> Result<()> {
        if !(10..=90).contains(&self.renew_at_percent) {
            return Err(Error::invalid(
                "database.dynamic_credentials.renew_at_percent: ожидается от 10 до 90",
            ));
        }
        if self.retry_interval.is_zero() {
            return Err(Error::invalid(
                "database.dynamic_credentials.retry_interval: должен быть больше нуля",
            ));
        }
        Ok(())
    }

    /// Доля срока `ttl`, после которой аренда продлевается.
    fn renew_after(&self, ttl: Duration) -> Duration {
        ttl * u32::from(self.renew_at_percent) / 100
    }
}

/// Выданные учётные данные и их аренды.
pub(crate) struct Issued {
    username: Secret,
    password: Secret,
    /// Аренды без повторов (логин и пароль одной выдачи — одна аренда).
    leases: Vec<SecretLease>,
    /// Наименьший срок аренды при выдаче; `None` — секреты без аренды (ротации нет).
    issued_ttl: Option<Duration>,
}

impl Issued {
    fn same_leases(&self, other: &Issued) -> bool {
        self.leases.len() == other.leases.len()
            && self
                .leases
                .iter()
                .all(|l| other.leases.iter().any(|o| o.id == l.id))
    }

    /// Параметры соединения `opts` с этими учётными данными.
    pub(crate) fn apply(&self, opts: PgConnectOptions) -> PgConnectOptions {
        opts.username(self.username.expose())
            .password(self.password.expose())
    }
}

/// Логин и пароль из хранилища. Отказ хранилища — `Unavailable` (ссылка и причина — в журнале).
pub(crate) async fn issue(dc: &DynamicCredentials, secrets: &dyn SecretStore) -> Result<Issued> {
    let username = fetch_secret(
        secrets,
        &dc.username,
        "database.dynamic_credentials.username",
    )
    .await?;
    let password = fetch_secret(
        secrets,
        &dc.password,
        "database.dynamic_credentials.password",
    )
    .await?;
    let mut leases: Vec<SecretLease> = Vec::new();
    for lease in [username.lease(), password.lease()].into_iter().flatten() {
        if !leases.iter().any(|l| l.id == lease.id) {
            leases.push(lease.clone());
        }
    }
    let issued_ttl = leases.iter().map(|l| l.ttl).min();
    Ok(Issued {
        username,
        password,
        leases,
        issued_ttl,
    })
}

/// Время последней смены учётных данных: соединения, открытые раньше, пул закрывает при выдаче.
#[derive(Debug, Clone, Default)]
pub(crate) struct Rotated(Arc<Mutex<Option<Instant>>>);

impl Rotated {
    fn mark(&self) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(Instant::now());
    }

    /// Соединение возрастом `age` открыто с текущими учётными данными.
    pub(crate) fn current(&self, age: Duration) -> bool {
        let at = *self.0.lock().unwrap_or_else(PoisonError::into_inner);
        match (at, Instant::now().checked_sub(age)) {
            (Some(at), Some(opened)) => opened >= at,
            _ => true,
        }
    }
}

/// Задача ротации учётных данных пула.
pub(crate) struct Rotation {
    pub(crate) cfg: PoolConfig,
    pub(crate) dc: DynamicCredentials,
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) pool: PgPool,
    pub(crate) rotated: Rotated,
}

/// Исход продления аренд.
enum Renewal {
    /// Аренды продлены не меньше чем на половину срока выдачи.
    Renewed(Duration),
    /// Продлить нельзя или срок урезан — нужны новые учётные данные.
    Exhausted,
}

impl Rotation {
    /// Продлевает аренды `cur` и ротирует учётные данные до закрытия пула.
    pub(crate) async fn run(self, mut cur: Issued) {
        let close = self.pool.close_event();
        tokio::pin!(close);
        let Some(mut ttl) = cur.issued_ttl else {
            tracing::info!("database.dynamic_credentials: секреты без аренды — ротации нет");
            return;
        };
        let mut wait = self.dc.renew_after(ttl);
        loop {
            tokio::select! {
                () = &mut close => return,
                () = tokio::time::sleep(wait) => {}
            }
            match self.renew(&mut cur).await {
                Ok(Renewal::Renewed(t)) => {
                    ttl = t;
                    wait = self.dc.renew_after(ttl);
                    continue;
                }
                Ok(Renewal::Exhausted) => {}
                Err(e) => {
                    tracing::warn!(error = %e, "database.dynamic_credentials: аренда не продлена");
                }
            }
            match issue(&self.dc, self.secrets.as_ref()).await {
                // Хранилище вернуло прежнюю выдачу (действующую аренду): повтор позже.
                Ok(next) if next.same_leases(&cur) => wait = self.dc.retry_interval,
                Ok(next) => match self.switch(&next) {
                    Ok(()) => {
                        cur = next;
                        match cur.issued_ttl {
                            Some(t) => wait = self.dc.renew_after(t),
                            None => {
                                tracing::info!(
                                    "database.dynamic_credentials: секреты без аренды — ротации нет"
                                );
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "database.dynamic_credentials: учётные данные не применены");
                        wait = self.dc.retry_interval;
                    }
                },
                // Причина — в журнале (fetch_secret).
                Err(_) => wait = self.dc.retry_interval,
            }
        }
    }

    /// Продлевает все аренды; срок — наименьший из новых.
    async fn renew(&self, cur: &mut Issued) -> Result<Renewal> {
        let Some(issued) = cur.issued_ttl else {
            return Ok(Renewal::Exhausted);
        };
        if cur.leases.iter().any(|l| !l.renewable) {
            return Ok(Renewal::Exhausted);
        }
        let mut renewed = Vec::with_capacity(cur.leases.len());
        for lease in &cur.leases {
            match self.secrets.renew(lease).await {
                Ok(l) => renewed.push(l),
                // Аренда истекла, отозвана или непродлеваема — новая выдача.
                Err(Error::NotFound) => return Ok(Renewal::Exhausted),
                Err(e) => return Err(e),
            }
        }
        let ttl = renewed.iter().map(|l| l.ttl).min().unwrap_or_default();
        cur.leases = renewed;
        // Хранилище урезает продление до max_ttl: заранее — новая выдача.
        Ok(if ttl >= issued / 2 && !ttl.is_zero() {
            Renewal::Renewed(ttl)
        } else {
            Renewal::Exhausted
        })
    }

    /// Новые соединения пула — с учётными данными `next`.
    fn switch(&self, next: &Issued) -> Result<()> {
        let opts = self
            .cfg
            .connect_options(&self.cfg.url, self.cfg.pgbouncer)?;
        self.pool.set_connect_options(next.apply(opts));
        self.rotated.mark();
        tracing::info!("database.dynamic_credentials: учётные данные обновлены");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_contracts::tls::{InternalTls, Profile};
    use tp_testkit::MemSecretStore;

    fn r(s: &str) -> SecretRef {
        s.parse().expect("ссылка")
    }

    fn dc(retry: Duration) -> DynamicCredentials {
        DynamicCredentials {
            username: r("database/creds/thepost#username"),
            password: r("database/creds/thepost#password"),
            renew_at_percent: 50,
            retry_interval: retry,
        }
    }

    fn cfg(dc: DynamicCredentials) -> PoolConfig {
        PoolConfig {
            url: "postgres://db.invalid/thepost?sslmode=disable".into(),
            dynamic_credentials: Some(dc),
            tls: InternalTls {
                allow_plaintext: true,
                ..InternalTls::default()
            },
            profile: Profile::Dev,
            ..PoolConfig::default()
        }
    }

    fn set(store: &MemSecretStore, user: &str, ttl: Duration, renewable: bool) {
        let d = dc(Duration::ZERO);
        store.set_dynamic(&d.username, user, ttl, renewable);
        store.set_dynamic(&d.password, format!("{user}-pw"), ttl, renewable);
    }

    async fn wait_user(pool: &PgPool, user: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while pool.connect_options().get_username() != user {
            assert!(Instant::now() < deadline, "логин {user} не применён");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[test]
    fn section_check_and_defaults() {
        let d: DynamicCredentials = serde_json::from_value(serde_json::json!({
            "username": {"secret": "database/creds/thepost#username"},
            "password": {"secret": "database/creds/thepost#password"},
        }))
        .expect("секция");
        assert_eq!(d.renew_at_percent, 67);
        assert_eq!(d.retry_interval, Duration::from_secs(5));
        assert!(d.check().is_ok());
        assert_eq!(
            d.renew_after(Duration::from_secs(300)),
            Duration::from_secs(201)
        );
        for pct in [0, 9, 91, 100] {
            let bad = DynamicCredentials {
                renew_at_percent: pct,
                ..d.clone()
            };
            assert!(bad.check().is_err(), "{pct}");
        }
        assert!(dc(Duration::ZERO).check().is_err());
        // Логин и пароль — только ссылки.
        assert!(
            serde_json::from_value::<DynamicCredentials>(serde_json::json!({
                "username": "thepost",
                "password": {"secret": "database/creds/thepost#password"},
            }))
            .is_err()
        );
    }

    #[tokio::test]
    async fn renews_lease_then_rotates_when_renewal_refused() {
        let store = MemSecretStore::default();
        set(&store, "v-1", Duration::from_millis(300), true);
        let d = dc(Duration::from_millis(50));
        let pool = crate::pool::rotating_pool(&cfg(d), Arc::new(store.clone()))
            .await
            .expect("пул");
        assert_eq!(pool.connect_options().get_username(), "v-1");
        assert_eq!(store.gets(), 2);
        // Продление каждые 150 мс: учётные данные прежние.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(store.renews() >= 2, "продлений: {}", store.renews());
        assert_eq!(store.gets(), 2);
        assert_eq!(pool.connect_options().get_username(), "v-1");
        // Отказ продления (аренда отозвана) — новая выдача.
        set(&store, "v-2", Duration::from_millis(300), true);
        store.fail_next(Error::NotFound);
        wait_user(&pool, "v-2").await;
        pool.close().await;
    }

    #[tokio::test]
    async fn non_renewable_lease_rotates_and_store_outage_is_retried() {
        let store = MemSecretStore::default();
        set(&store, "v-1", Duration::from_millis(200), false);
        let pool = crate::pool::rotating_pool(
            &cfg(dc(Duration::from_millis(30))),
            Arc::new(store.clone()),
        )
        .await
        .expect("пул");
        set(&store, "v-2", Duration::from_millis(200), false);
        wait_user(&pool, "v-2").await;
        assert_eq!(store.renews(), 0, "непродлеваемая аренда не продлевается");
        // Хранилище недоступно: прежние учётные данные остаются, после распечатывания — новые.
        store.seal();
        set(&store, "v-3", Duration::from_millis(200), false);
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(pool.connect_options().get_username(), "v-2");
        store.unseal();
        wait_user(&pool, "v-3").await;
        // Пул закрыт — задача ротации завершается: новых чтений нет.
        pool.close().await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let gets = store.gets();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(store.gets(), gets);
    }

    #[test]
    fn rotated_marks_older_connections() {
        let r = Rotated::default();
        assert!(r.current(Duration::from_secs(3600)));
        r.mark();
        std::thread::sleep(Duration::from_millis(5));
        assert!(r.current(Duration::ZERO));
        assert!(!r.current(Duration::from_secs(1)));
    }
}
