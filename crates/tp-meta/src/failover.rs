//! Поиск primary и поведение узла при переключении СУБД (META-25; SRS 3.1, решение 2026-11-01
//! «Требования к СУБД»).
//!
//! Адрес primary — один из способов: единый адрес [`PoolConfig::url`] (VIP, DNS, прокси, Service Kubernetes)
//! или список адресов `url` + [`PoolConfig::urls`]: узел выбирает первый доступный для записи
//! (семантика `target_session_attrs=read-write` — `SHOW transaction_read_only` = `off`). Учётные данные и
//! параметры соединения — из `url`; из адресов списка берутся хост, порт и имя БД.
//!
//! Признаки переключения — обрыв соединения, коды `25006` (транзакция только для чтения) и `57P01`
//! (остановка сервера); на границе крейта это `Error::Unavailable` ([`crate::schema::db_err`]). После
//! временного отказа операции [`Failover`] проверяет соединение пула: оно доступно для записи — отказ
//! временный (конфликт сериализации, взаимоблокировка), пул не трогается; иначе — поиск primary по
//! списку, сброс пула (новые соединения — к найденному primary, открытые раньше закрываются при выдаче) и
//! повтор операции с нарастающей паузой ([`FailoverConfig`]). Дольше `retry_for` — временный отказ
//! вызывающему (SMTP 4xx, временная ошибка IMAP/JMAP): письма в памяти не накапливаются.
//!
//! Повторяются только идемпотентные операции — их вызывающий передаёт явно ([`Failover::retry`],
//! [`Failover::with_tenant`]); остальные ([`Failover::once`]) выполняются без повтора и отдают временный
//! отказ с рекомендуемой паузой. Хранилища [`crate::PgMeta::from_failover`] повторяют чтения, записи — без
//! повтора. Исход фиксации
//! при обрыве во время COMMIT устанавливается по ключу операции (идентификатор письма, ключ журнала
//! событий): [`Failover::with_tenant_keyed`] перед повтором проверяет, применена ли операция.
//!
//! Длительность переключения и число повторов — [`Failover::metrics`] (платформа публикует их в `/metrics`).
//!
//! ```yaml
//! database:
//!   url: postgres://thepost@db1.example.ru:5432/thepost
//!   urls: [postgres://thepost@db2.example.ru:5432/thepost]
//!   failover: { retry_initial: 100ms, retry_max: 2s, retry_for: 30s, probe_timeout: 3s }
//! ```

use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, PgConnection, PgPool};
use tp_contracts::config::duration;
use tp_contracts::secrets::SecretStore;
use tp_contracts::{Error, Result, TenantId};

use crate::credentials::Rotated;
use crate::pool::{self, PoolConfig, TxFuture};
use crate::schema::{begin, begin_system, db_err};

/// Подсекция `database.failover`: повтор операций и поиск primary при переключении СУБД (SRS 3.1, 5.5).
/// Параметры требуют перезапуска.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FailoverConfig {
    /// Пауза перед первым повтором операции; дальше пауза удваивается до `retry_max`.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retry_initial: Duration,
    /// Наибольшая пауза между повторами; она же — рекомендуемая пауза во временном отказе.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retry_max: Duration,
    /// Сколько повторять операцию с первой неудачи (и искать primary при запуске); дольше — временный
    /// отказ. По умолчанию — RTO кластера (30 с).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retry_for: Duration,
    /// Ожидание соединения и ответа при проверке адреса и соединения пула.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub probe_timeout: Duration,
}

impl Default for FailoverConfig {
    fn default() -> Self {
        Self {
            retry_initial: Duration::from_millis(100),
            retry_max: Duration::from_secs(2),
            retry_for: Duration::from_secs(30),
            probe_timeout: Duration::from_secs(3),
        }
    }
}

impl FailoverConfig {
    /// Проверяет подсекцию: паузы и ожидания больше нуля, `retry_initial` ≤ `retry_max`.
    pub fn check(&self) -> Result<()> {
        if self.retry_initial.is_zero() || self.probe_timeout.is_zero() {
            return Err(Error::invalid(
                "database.failover: retry_initial и probe_timeout должны быть больше нуля",
            ));
        }
        if self.retry_initial > self.retry_max {
            return Err(Error::invalid(
                "database.failover: retry_initial больше retry_max",
            ));
        }
        Ok(())
    }

    /// Пауза после `pause`: удвоение до `retry_max`.
    fn next_pause(&self, pause: Duration) -> Duration {
        pause.saturating_mul(2).min(self.retry_max)
    }
}

/// Снимок метрик переключения СУБД.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FailoverMetrics {
    /// Переключений (сбросов пула на найденный primary) с запуска.
    pub switches: u64,
    /// Повторов операций после временного отказа.
    pub retries: u64,
    /// Операций, не выполненных за `retry_for` (временный отказ вызывающему).
    pub gave_up: u64,
    /// Длительность последнего переключения: от обнаружения отказа primary до найденного нового.
    pub last_switch: Option<Duration>,
    /// Суммарная длительность переключений.
    pub switch_total: Duration,
    /// Идёт переключение: primary не доступен для записи, новый не найден.
    pub switching: bool,
    /// Номер текущего primary в списке `url` + `urls` (0 — `url`).
    pub primary: usize,
}

#[derive(Debug, Default)]
struct Counters {
    switches: AtomicU64,
    retries: AtomicU64,
    gave_up: AtomicU64,
    /// Микросекунды; `u64::MAX` — переключений не было.
    last_switch_us: AtomicU64,
    switch_total_us: AtomicU64,
    switching: AtomicBool,
    primary: AtomicUsize,
}

/// Хост, порт и имя БД адреса из списка.
#[derive(Clone)]
pub(crate) struct Target {
    opts: PgConnectOptions,
}

impl std::fmt::Debug for Target {
    // Параметры соединения в журнал не попадают (в dev адрес может содержать пароль).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.label())
    }
}

impl Target {
    pub(crate) fn new(opts: PgConnectOptions) -> Self {
        Self { opts }
    }

    /// `хост:порт` для журнала (адрес целиком может содержать пароль).
    fn label(&self) -> String {
        format!("{}:{}", self.opts.get_host(), self.opts.get_port())
    }
}

/// `opts` с хостом, портом, сокетом и именем БД из `from`; учётные данные и остальное — из `opts`.
pub(crate) fn retarget(opts: PgConnectOptions, from: &PgConnectOptions) -> PgConnectOptions {
    let mut opts = opts.host(from.get_host()).port(from.get_port());
    if let Some(socket) = from.get_socket() {
        opts = opts.socket(socket);
    }
    match from.get_database() {
        Some(db) => opts.database(db),
        None => opts,
    }
}

/// Состояние соединения пула после временного отказа.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Health {
    /// Доступно для записи: отказ временный, переключения нет.
    Writable,
    /// Сервер только для чтения (бывший primary): нужен поиск.
    ReadOnly,
    /// Соединение не открывается или оборвано: нужен поиск.
    Broken,
    /// Пул занят (ожидание соединения истекло): проверяется адрес текущего primary.
    Busy,
}

/// Пул узла с поиском primary и повтором идемпотентных операций при переключении СУБД.
/// Клон — тот же пул и те же метрики.
#[derive(Debug, Clone)]
pub struct Failover(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    pool: PgPool,
    cfg: FailoverConfig,
    targets: Vec<Target>,
    reset: Rotated,
    /// Номер переключения: повтор после чужого переключения не ищет primary заново.
    epoch: AtomicU64,
    /// Один поиск primary на пул.
    search: tokio::sync::Mutex<()>,
    /// Обнаружен отказ primary, новый ещё не найден.
    down_since: Mutex<Option<Instant>>,
    counters: Counters,
    /// Номер переключения для наблюдателей ([`Failover::switches`]).
    switched: tokio::sync::watch::Sender<u64>,
}

/// Состояние повторов одной операции.
struct Retrier<'a> {
    inner: &'a Inner,
    started: Option<Instant>,
    pause: Duration,
}

impl Retrier<'_> {
    /// После отказа `e` попытки с номером переключения `seen`: проверка primary и пауза перед повтором;
    /// неповторяемая ошибка и истёкший `retry_for` — ошибка вызывающему.
    async fn after(&mut self, e: Error, seen: u64) -> Result<()> {
        if !e.is_retryable() {
            return Err(e);
        }
        let started = *self.started.get_or_insert_with(Instant::now);
        self.inner.recover(seen).await;
        let cfg = &self.inner.cfg;
        if started.elapsed().saturating_add(self.pause) > cfg.retry_for {
            self.inner.counters.gave_up.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                after = ?started.elapsed(),
                "СУБД: операция не выполнена за database.failover.retry_for — временный отказ"
            );
            return Err(Error::Unavailable {
                retry_after: Some(cfg.retry_max),
            });
        }
        self.inner.counters.retries.fetch_add(1, Ordering::Relaxed);
        tokio::time::sleep(self.pause).await;
        self.pause = cfg.next_pause(self.pause);
        Ok(())
    }
}

/// Область транзакции.
#[derive(Debug, Clone, Copy)]
enum Scope {
    Tenant(TenantId),
    System,
}

impl Scope {
    async fn begin(self, pool: &PgPool) -> Result<sqlx::Transaction<'static, sqlx::Postgres>> {
        match self {
            Scope::Tenant(t) => begin(pool, t).await,
            Scope::System => begin_system(pool).await,
        }
    }
}

impl Failover {
    /// Пул `pool` с адресами `targets` (первый — `url`); соединения, открытые до смены адреса, `pool`
    /// закрывает по отметке `reset`.
    pub(crate) fn new(
        pool: PgPool,
        targets: Vec<Target>,
        reset: Rotated,
        cfg: FailoverConfig,
    ) -> Self {
        let counters = Counters {
            last_switch_us: AtomicU64::new(u64::MAX),
            ..Counters::default()
        };
        Self(Arc::new(Inner {
            pool,
            cfg,
            targets,
            reset,
            epoch: AtomicU64::new(0),
            search: tokio::sync::Mutex::new(()),
            down_since: Mutex::new(None),
            counters,
            switched: tokio::sync::watch::Sender::new(0),
        }))
    }

    /// Пул узла (новые соединения — к текущему primary).
    pub fn pool(&self) -> &PgPool {
        &self.0.pool
    }

    /// Номер переключения primary: новое значение — пул перенаправлен к другому адресу (слушатель сигналов
    /// переподключается, [`crate::signals`]). Выбор primary при запуске — тоже переключение.
    pub fn switches(&self) -> tokio::sync::watch::Receiver<u64> {
        self.0.switched.subscribe()
    }

    /// Снимок метрик переключения.
    pub fn metrics(&self) -> FailoverMetrics {
        let c = &self.0.counters;
        let last = c.last_switch_us.load(Ordering::Relaxed);
        FailoverMetrics {
            switches: c.switches.load(Ordering::Relaxed),
            retries: c.retries.load(Ordering::Relaxed),
            gave_up: c.gave_up.load(Ordering::Relaxed),
            last_switch: (last != u64::MAX).then(|| Duration::from_micros(last)),
            switch_total: Duration::from_micros(c.switch_total_us.load(Ordering::Relaxed)),
            switching: c.switching.load(Ordering::Relaxed),
            primary: c.primary.load(Ordering::Relaxed),
        }
    }

    /// Выбор primary при запуске: первый доступный для записи адрес; не найден — повтор с нарастающей
    /// паузой до `retry_for`, затем `Unavailable`.
    pub(crate) async fn start(&self) -> Result<()> {
        let inner = &self.0;
        let started = Instant::now();
        let mut pause = inner.cfg.retry_initial;
        loop {
            if let Some(i) = inner.search().await {
                inner.switch_to(i, false);
                return inner.pool.acquire().await.map(drop).map_err(db_err);
            }
            if started.elapsed().saturating_add(pause) > inner.cfg.retry_for {
                tracing::warn!(
                    "СУБД: нет адреса, доступного для записи (database.url, database.urls)"
                );
                return Err(Error::Unavailable {
                    retry_after: Some(inner.cfg.retry_max),
                });
            }
            tokio::time::sleep(pause).await;
            pause = inner.cfg.next_pause(pause);
        }
    }

    /// Выполняет **идемпотентную** операцию `op` над пулом; временный отказ — проверка primary
    /// (при переключении — поиск и сброс пула) и повтор с нарастающей паузой до `retry_for`.
    pub async fn retry<T, F, Fut>(&self, mut op: F) -> Result<T>
    where
        F: FnMut(PgPool) -> Fut,
        Fut: Future<Output = Result<T>>,
    {
        let mut r = self.0.retrier();
        loop {
            let seen = self.0.epoch();
            match op(self.0.pool.clone()).await {
                Ok(v) => return Ok(v),
                Err(e) => r.after(e, seen).await?,
            }
        }
    }

    /// Выполняет **неидемпотентную** операцию `op` (запись без ключа) один раз: временный отказ —
    /// проверка primary (при переключении — поиск и сброс пула) и `Unavailable` с рекомендуемой паузой
    /// `retry_max`; повторяет операцию вызывающий (SMTP 4xx, временная ошибка IMAP/JMAP).
    pub async fn once<T, Fut>(&self, op: Fut) -> Result<T>
    where
        Fut: Future<Output = Result<T>>,
    {
        let seen = self.0.epoch();
        match op.await {
            Err(e) if e.is_retryable() => {
                self.0.recover(seen).await;
                Err(Error::Unavailable {
                    retry_after: Some(self.0.cfg.retry_max),
                })
            }
            r => r,
        }
    }

    /// Как [`pool::with_tenant`] с повтором при переключении: тело `f` — **идемпотентное** (повтор после
    /// обрыва во время COMMIT применяет его ещё раз).
    pub async fn with_tenant<T, F>(&self, tenant: TenantId, f: F) -> Result<T>
    where
        F: for<'c> Fn(&'c mut PgConnection) -> TxFuture<'c, T>,
    {
        self.tx(Scope::Tenant(tenant), &f, None::<&NoKey<T>>).await
    }

    /// Как [`with_tenant`](Self::with_tenant), а после обрыва во время COMMIT исход устанавливается по
    /// ключу операции: `committed` в новой транзакции возвращает результат применённой операции
    /// (`Some`) или `None` — операция не применена, тело `f` выполняется снова.
    pub async fn with_tenant_keyed<T, F, C>(
        &self,
        tenant: TenantId,
        f: F,
        committed: C,
    ) -> Result<T>
    where
        F: for<'c> Fn(&'c mut PgConnection) -> TxFuture<'c, T>,
        C: for<'c> Fn(&'c mut PgConnection) -> TxFuture<'c, Option<T>>,
    {
        self.tx(Scope::Tenant(tenant), &f, Some(&committed)).await
    }

    /// Как [`with_tenant`](Self::with_tenant), но по всем организациям ([`pool::with_system`]).
    pub async fn with_system<T, F>(&self, f: F) -> Result<T>
    where
        F: for<'c> Fn(&'c mut PgConnection) -> TxFuture<'c, T>,
    {
        self.tx(Scope::System, &f, None::<&NoKey<T>>).await
    }

    /// Как [`with_tenant_keyed`](Self::with_tenant_keyed), но по всем организациям (журнал событий).
    pub async fn with_system_keyed<T, F, C>(&self, f: F, committed: C) -> Result<T>
    where
        F: for<'c> Fn(&'c mut PgConnection) -> TxFuture<'c, T>,
        C: for<'c> Fn(&'c mut PgConnection) -> TxFuture<'c, Option<T>>,
    {
        self.tx(Scope::System, &f, Some(&committed)).await
    }

    async fn tx<T, F, C>(&self, scope: Scope, f: &F, committed: Option<&C>) -> Result<T>
    where
        F: for<'c> Fn(&'c mut PgConnection) -> TxFuture<'c, T>,
        C: for<'c> Fn(&'c mut PgConnection) -> TxFuture<'c, Option<T>>,
    {
        let inner = &self.0;
        let mut r = inner.retrier();
        // Прошлая попытка оборвалась во время COMMIT: исход неизвестен.
        let mut ambiguous = false;
        loop {
            let seen = inner.epoch();
            if ambiguous && let Some(check) = committed {
                match run(&inner.pool, scope, check).await {
                    Ok(Some(v)) => return Ok(v),
                    Ok(None) => ambiguous = false,
                    Err(e) => {
                        r.after(e, seen).await?;
                        continue;
                    }
                }
            }
            let mut tx = match scope.begin(&inner.pool).await {
                Ok(tx) => tx,
                Err(e) => {
                    r.after(e, seen).await?;
                    continue;
                }
            };
            let out = match f(&mut tx).await {
                Ok(v) => v,
                Err(e) => {
                    drop(tx);
                    r.after(e, seen).await?;
                    continue;
                }
            };
            match tx.commit().await {
                Ok(()) => return Ok(out),
                Err(e) => {
                    let e = db_err(e);
                    ambiguous = e.is_retryable();
                    r.after(e, seen).await?;
                }
            }
        }
    }
}

/// Тип проверки ключа для операций без ключа.
type NoKey<T> = for<'c> fn(&'c mut PgConnection) -> TxFuture<'c, Option<T>>;

/// Тело `f` в транзакции области `scope` без повтора.
async fn run<T, F>(pool: &PgPool, scope: Scope, f: &F) -> Result<T>
where
    F: for<'c> Fn(&'c mut PgConnection) -> TxFuture<'c, T>,
{
    match scope {
        Scope::Tenant(t) => pool::with_tenant(pool, t, f).await,
        Scope::System => pool::with_system(pool, f).await,
    }
}

impl Inner {
    fn retrier(&self) -> Retrier<'_> {
        Retrier {
            inner: self,
            started: None,
            pause: self.cfg.retry_initial,
        }
    }

    fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    /// После временного отказа попытки с номером переключения `seen`: проверка соединения пула и, при
    /// отказе primary, поиск нового и сброс пула. Переключение, случившееся после `seen`, не повторяется.
    async fn recover(&self, seen: u64) {
        let _search = self.search.lock().await;
        if self.epoch() != seen {
            return;
        }
        match self.health().await {
            Health::Writable => return,
            Health::Busy => {
                let cur = self.counters.primary.load(Ordering::Relaxed);
                if let Some(t) = self.targets.get(cur)
                    && self.probe(t).await == Some(true)
                {
                    return;
                }
            }
            Health::ReadOnly | Health::Broken => {}
        }
        let since = *self
            .down_since
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_or_insert_with(Instant::now);
        if !self.counters.switching.swap(true, Ordering::Relaxed) {
            tracing::warn!("СУБД: primary не доступен для записи — поиск primary");
        }
        match self.search().await {
            Some(i) => {
                self.switch_to(i, true);
                let took = since.elapsed();
                *self
                    .down_since
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = None;
                let us = u64::try_from(took.as_micros()).unwrap_or(u64::MAX - 1);
                self.counters.last_switch_us.store(us, Ordering::Relaxed);
                self.counters
                    .switch_total_us
                    .fetch_add(us, Ordering::Relaxed);
                self.counters.switching.store(false, Ordering::Relaxed);
                tracing::info!(primary = %self.targets[i].label(), ?took, "СУБД: переключение на primary");
            }
            None => tracing::warn!("СУБД: primary не найден — повтор"),
        }
    }

    /// Новые соединения — к адресу `i`, открытые раньше закрываются при выдаче; `count` — учитывать в метриках.
    fn switch_to(&self, i: usize, count: bool) {
        let target = &self.targets[i].opts;
        self.reset
            .replace(&self.pool, |cur| retarget(cur.clone(), target));
        self.counters.primary.store(i, Ordering::Relaxed);
        let epoch = self.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        self.switched.send_replace(epoch);
        if count {
            self.counters.switches.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Первый адрес списка, доступный для записи.
    async fn search(&self) -> Option<usize> {
        for (i, t) in self.targets.iter().enumerate() {
            match self.probe(t).await {
                Some(true) => return Some(i),
                Some(false) => tracing::debug!(addr = %t.label(), "СУБД: адрес только для чтения"),
                None => tracing::debug!(addr = %t.label(), "СУБД: адрес недоступен"),
            }
        }
        None
    }

    /// Адрес доступен для записи (`Some(true)`), только для чтения (`Some(false)`), недоступен (`None`):
    /// отдельное соединение с учётными данными пула.
    async fn probe(&self, t: &Target) -> Option<bool> {
        let opts = retarget((*self.pool.connect_options()).clone(), &t.opts);
        let check = async {
            let mut conn = PgConnection::connect_with(&opts).await?;
            let writable = writable(&mut conn).await;
            let _ = conn.close().await;
            writable
        };
        match tokio::time::timeout(self.cfg.probe_timeout, check).await {
            Ok(Ok(w)) => Some(w),
            Ok(Err(e)) => {
                tracing::debug!(addr = %t.label(), error = %e, "СУБД: проверка адреса");
                None
            }
            Err(_) => None,
        }
    }

    /// Проверка соединения пула (обрывы отсекает проверка соединения при выдаче).
    async fn health(&self) -> Health {
        let timeout = self.cfg.probe_timeout;
        let mut conn = match tokio::time::timeout(timeout, self.pool.acquire()).await {
            Err(_) | Ok(Err(sqlx::Error::PoolTimedOut)) => return Health::Busy,
            Ok(Err(_)) => return Health::Broken,
            Ok(Ok(c)) => c,
        };
        match tokio::time::timeout(timeout, writable(&mut conn)).await {
            Ok(Ok(true)) => Health::Writable,
            Ok(Ok(false)) => Health::ReadOnly,
            _ => {
                conn.close_on_drop();
                Health::Broken
            }
        }
    }
}

/// Сеанс доступен для записи (`target_session_attrs=read-write`).
async fn writable(conn: &mut PgConnection) -> std::result::Result<bool, sqlx::Error> {
    let ro: String = sqlx::query_scalar("SHOW transaction_read_only")
        .fetch_one(conn)
        .await?;
    Ok(ro != "on")
}

/// Пул узла с поиском primary по `url` + `urls` и повтором при переключении (SRS 3.1); с
/// [`PoolConfig::dynamic_credentials`] — с ротацией учётных данных ([`pool::connect_rotating`]).
/// Primary выбирается при запуске (не найден за `failover.retry_for` — `Unavailable`).
pub async fn connect_failover(cfg: &PoolConfig, secrets: Arc<dyn SecretStore>) -> Result<Failover> {
    cfg.check()?;
    let targets = cfg.targets()?;
    let reset = Rotated::default();
    let pool = pool::node_pool(cfg, secrets, reset.clone()).await?;
    let failover = Failover::new(pool, targets, reset, cfg.failover.clone());
    if let Err(e) = failover.start().await {
        failover.pool().close().await;
        return Err(e);
    }
    Ok(failover)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn config_defaults_and_check() {
        let c = FailoverConfig::default();
        assert_eq!(c.check(), Ok(()));
        assert_eq!(c.retry_for, Duration::from_secs(30));
        let c: FailoverConfig =
            serde_json::from_str(r#"{"retry_initial": "50ms", "retry_max": "1s"}"#).expect("json");
        assert_eq!(c.retry_initial, Duration::from_millis(50));
        assert_eq!(c.probe_timeout, Duration::from_secs(3));
        let bad = FailoverConfig {
            retry_initial: Duration::from_secs(5),
            ..FailoverConfig::default()
        };
        assert!(bad.check().is_err());
        let zero = FailoverConfig {
            probe_timeout: Duration::ZERO,
            ..FailoverConfig::default()
        };
        assert!(zero.check().is_err());
        assert!(serde_json::from_str::<FailoverConfig>(r#"{"retries": 3}"#).is_err());
    }

    #[test]
    fn pause_doubles_to_max() {
        let c = FailoverConfig {
            retry_initial: Duration::from_millis(100),
            retry_max: Duration::from_millis(350),
            ..FailoverConfig::default()
        };
        let mut p = c.retry_initial;
        let mut seen = Vec::new();
        for _ in 0..4 {
            seen.push(p.as_millis());
            p = c.next_pause(p);
        }
        assert_eq!(seen, [100, 200, 350, 350]);
    }

    #[test]
    fn retarget_keeps_credentials() {
        let cur = PgConnectOptions::from_str("postgres://app:pw@db1:5432/a").expect("cur");
        let to = PgConnectOptions::from_str("postgres://other@db2:6432/b").expect("to");
        let o = retarget(cur, &to);
        assert_eq!(o.get_host(), "db2");
        assert_eq!(o.get_port(), 6432);
        assert_eq!(o.get_database(), Some("b"));
        assert_eq!(o.get_username(), "app");
    }
}
