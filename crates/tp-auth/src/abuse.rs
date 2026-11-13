//! Защита от перебора на всех узлах (SRS 6.3; решение 2026-11-01; CONTRACTS.md#AbuseBlocks).
//!
//! [`AbuseGuard`] ведёт счётчики неудачных попыток на узле (по адресу, подсети и учётной записи, окно —
//! `window`); при превышении порога — [`AbuseBlocks::block`] на `block_for`. Проверка блокировки — по локальному
//! набору узла: набор организации загружается [`AbuseBlocks::active`] при первой проверке, дальше обновляется
//! по сигналам `Blocked`/`Unblocked` ([`AbuseGuard::listen`]) и перечитывается после `Resync` и переподписки.
//!
//! Протокол спрашивает [`AbuseGuard::check`] до проверки учётных данных и сообщает о результате:
//! [`AbuseGuard::failure`] — неудача, [`AbuseGuard::success`] — успех (сбрасывает счётчик учётной записи).
//!
//! ## Разделение с PLT-07 (решение владельца #253)
//! Здесь — только неудачные входы и блокировки по ним (адрес, подсеть, учётная запись). Частоту запросов
//! (соединения, команды) считает PLT-07; его счётчики сюда не попадают, и наоборот.
//!
//! ## Хранилище блокировок недоступно (`auth.abuse.fail_closed`)
//! Счётчики всегда локальны для узла и от хранилища не зависят. Блокировка, которую не удалось записать в
//! хранилище, действует на этом узле (собственный набор узла) до своего срока; после восстановления хранилища
//! узел записывает её туда при следующей загрузке набора организации или следующей блокировке — дальше она
//! общая для всех узлов. [`AbuseGuard::check`] при ошибке загрузки набора:
//! - `fail_closed = false` (по умолчанию) — пускает к проверке учётных данных, учитывая блокировки,
//!   известные узлу (ранее загруженный набор и собственный набор узла);
//! - `fail_closed = true` — отказ входа ([`Error::Unavailable`]).

use std::collections::HashMap;
use std::fmt;
use std::future::poll_fn;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tp_contracts::abuse::{AbuseBlocks, BlockSubject};
use tp_contracts::bus::{Event, EventBus, EventKind, EventStream};
use tp_contracts::config::duration;
use tp_contracts::{AccountId, Error, Result, TenantId};

/// Секция защиты от перебора (`auth.abuse`). Порог `0` — счётчик этого вида выключен.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AbuseConfig {
    /// Окно счётчика: считаются неудачи с первой неудачи окна.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub window: Duration,
    /// Неудач с одного адреса за окно до блокировки адреса.
    pub ip_failures: u32,
    /// Неудач из одной подсети за окно до блокировки подсети.
    pub net_failures: u32,
    /// Префикс подсети IPv4 (≤ 32).
    pub net_prefix_v4: u8,
    /// Префикс подсети IPv6 (≤ 128).
    pub net_prefix_v6: u8,
    /// Неудач входа под одной учётной записью за окно до её блокировки.
    pub account_failures: u32,
    /// Срок блокировки.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub block_for: Duration,
    /// Наибольшее число счётчиков на узле (старые вытесняются).
    pub max_counters: u64,
    /// Пауза перед повторной подпиской на сигналы после потери связи.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub resubscribe_delay: Duration,
    /// Хранилище блокировок недоступно: `false` — вход проверяется по блокировкам, известным узлу; `true` —
    /// отказ входа (`Unavailable`). Решение владельца #253.
    pub fail_closed: bool,
}

impl Default for AbuseConfig {
    fn default() -> Self {
        AbuseConfig {
            window: Duration::from_secs(300),
            ip_failures: 10,
            net_failures: 100,
            net_prefix_v4: 24,
            net_prefix_v6: 64,
            account_failures: 20,
            block_for: Duration::from_secs(900),
            max_counters: 100_000,
            resubscribe_delay: Duration::from_secs(1),
            fail_closed: false,
        }
    }
}

impl AbuseConfig {
    /// Проверяет секцию.
    pub fn check(&self) -> Result<()> {
        if self.net_prefix_v4 > 32 || self.net_prefix_v6 > 128 {
            return Err(Error::invalid(
                "abuse: net_prefix_v4 must be at most 32, net_prefix_v6 at most 128",
            ));
        }
        if self.window.is_zero() || self.block_for.is_zero() {
            return Err(Error::invalid(
                "abuse: window and block_for must be positive",
            ));
        }
        if self.max_counters == 0 {
            return Err(Error::invalid("abuse: max_counters must be positive"));
        }
        Ok(())
    }
}

/// Попытка входа: адрес источника и учётная запись, если известны.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Attempt {
    /// Адрес источника в канонической форме (адрес соединения от слушателя, PLT-34).
    pub ip: Option<IpAddr>,
    /// Учётная запись, под которой входят.
    pub account: Option<AccountId>,
}

type Now = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// Набор блокировок организации: субъект → срок.
type Set = HashMap<BlockSubject, SystemTime>;

/// Локальный набор блокировок узла.
#[derive(Debug, Default)]
struct Local {
    /// Наборы из хранилища по загруженным организациям (сбрасываются сверкой).
    tenants: HashMap<TenantId, Set>,
    /// Блокировки узла, не записанные в хранилище (оно было недоступно); сверкой не сбрасываются.
    own: HashMap<TenantId, Set>,
}

struct Inner {
    cfg: AbuseConfig,
    store: Arc<dyn AbuseBlocks>,
    now: Now,
    counters: moka::future::Cache<(TenantId, BlockSubject), Arc<AtomicU32>>,
    local: RwLock<Local>,
    /// Растёт при каждом изменении набора по сигналу: загрузка, начатая до него, не кэшируется.
    epoch: AtomicU64,
}

/// Счётчики попыток узла и локальный набор блокировок; клоны разделяют состояние.
#[derive(Clone)]
pub struct AbuseGuard(Arc<Inner>);

impl fmt::Debug for AbuseGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AbuseGuard")
            .field("cfg", &self.0.cfg)
            .field("local", &self.0.local)
            .finish_non_exhaustive()
    }
}

impl AbuseGuard {
    /// Защита по секции `cfg` с хранилищем блокировок `store` (`tp-meta`) и системными часами.
    pub fn new(cfg: AbuseConfig, store: Arc<dyn AbuseBlocks>) -> Self {
        Self::with_clock(cfg, store, Arc::new(SystemTime::now))
    }

    /// То же с часами `now` (тесты).
    pub fn with_clock(cfg: AbuseConfig, store: Arc<dyn AbuseBlocks>, now: Now) -> Self {
        let counters = moka::future::Cache::builder()
            .max_capacity(cfg.max_counters)
            .time_to_live(cfg.window)
            .build();
        AbuseGuard(Arc::new(Inner {
            cfg,
            store,
            now,
            counters,
            local: RwLock::new(Local::default()),
            epoch: AtomicU64::new(0),
        }))
    }

    /// Срок блокировки попытки (наибольший из адреса, покрывающих подсетей и учётной записи); `None` — можно
    /// проверять учётные данные. Хранилище недоступно — по `fail_closed`: пускает с блокировками, известными
    /// узлу, или отказывает (`Unavailable`).
    pub async fn check(&self, tenant: TenantId, attempt: &Attempt) -> Result<Option<SystemTime>> {
        if let Err(e) = self.ensure_loaded(tenant).await {
            if self.0.cfg.fail_closed {
                tracing::warn!(%tenant, error = %e, "abuse blocks: store unavailable, login refused");
                return Err(if e.is_retryable() {
                    e
                } else {
                    Error::UNAVAILABLE
                });
            }
            tracing::warn!(%tenant, error = %e, "abuse blocks: store unavailable, node blocks only");
        }
        let now = (self.0.now)();
        let local = self.read();
        let sets = [local.tenants.get(&tenant), local.own.get(&tenant)];
        Ok(subjects_to_check(attempt)
            .flat_map(|s| {
                sets.iter()
                    .flatten()
                    .filter_map(move |set| set.get(&s).copied())
            })
            .filter(|until| *until > now)
            .max())
    }

    /// Неудачная попытка: счётчики адреса, подсети и учётной записи; превышение порога блокирует субъект
    /// на всех узлах (хранилище недоступно — на этом узле, см. описание модуля). Возвращает срок блокировки,
    /// если попытка её вызвала.
    pub async fn failure(&self, tenant: TenantId, attempt: &Attempt) -> Result<Option<SystemTime>> {
        let cfg = &self.0.cfg;
        let mut counted = Vec::new();
        if let Some(ip) = attempt.ip {
            counted.push((BlockSubject::Ip(ip), cfg.ip_failures));
            let prefix = if ip.is_ipv4() {
                cfg.net_prefix_v4
            } else {
                cfg.net_prefix_v6
            };
            counted.push((BlockSubject::net(ip, prefix)?, cfg.net_failures));
        }
        if let Some(account) = attempt.account {
            counted.push((BlockSubject::Account(account), cfg.account_failures));
        }
        let mut blocked = None;
        for (subject, limit) in counted {
            if limit == 0 {
                continue;
            }
            let key = (tenant, subject);
            let counter = self
                .0
                .counters
                .get_with(key, async { Arc::new(AtomicU32::new(0)) })
                .await;
            let n = counter.fetch_add(1, Ordering::Relaxed).saturating_add(1);
            if n < limit {
                continue;
            }
            self.0.counters.invalidate(&key).await;
            let until = self.block(tenant, &subject, n).await?;
            blocked = blocked.max(Some(until));
        }
        Ok(blocked)
    }

    /// Успешный вход: счётчик учётной записи сбрасывается (счётчики адреса — нет, иначе своя учётная запись
    /// обнуляла бы перебор чужих).
    pub async fn success(&self, tenant: TenantId, attempt: &Attempt) {
        if let Some(account) = attempt.account {
            self.0
                .counters
                .invalidate(&(tenant, BlockSubject::Account(account)))
                .await;
        }
    }

    /// Слушает системные сигналы `bus` (бесконечно; запускать задачей): `Blocked`/`Unblocked` обновляют
    /// набор, `Resync` и потеря потока сбрасывают его (перечитывание — при следующей проверке).
    pub async fn listen(&self, bus: Arc<dyn EventBus>) {
        loop {
            match bus.subscribe_system().await {
                Ok(mut stream) => {
                    // Сигналы до подписки могли потеряться — сверка с хранилищем.
                    self.reset();
                    while let Some(event) = next(&mut stream).await {
                        self.apply(&event).await;
                    }
                    tracing::warn!("abuse blocks: signal stream ended, resubscribing");
                }
                Err(e) => tracing::warn!(error = %e, "abuse blocks: subscribe failed"),
            }
            self.reset();
            tokio::time::sleep(self.0.cfg.resubscribe_delay).await;
        }
    }

    /// Применяет сигнал к локальному набору.
    pub async fn apply(&self, event: &Event) {
        match &event.kind {
            EventKind::Blocked { subject } => self.on_blocked(event.tenant, subject).await,
            // Снятая блокировка подсети могла продлевать сроки адресов в наборе — организация перечитывается.
            EventKind::Unblocked { subject } => {
                self.bump();
                let mut local = self.write();
                local.tenants.remove(&event.tenant);
                if let Some(own) = local.own.get_mut(&event.tenant) {
                    own.remove(subject);
                }
            }
            EventKind::Resync => self.reset(),
            _ => {}
        }
    }

    async fn on_blocked(&self, tenant: TenantId, subject: &BlockSubject) {
        // Организация не загружена — увидит блокировку при загрузке.
        if !self.read().tenants.contains_key(&tenant) {
            return;
        }
        self.bump();
        match self.0.store.blocked(tenant, subject).await {
            Ok(Some(until)) => self.insert(tenant, *subject, SystemTime::from(until)),
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(error = %e, "abuse blocks: read after signal failed, reloading");
                self.write().tenants.remove(&tenant);
            }
        }
    }

    async fn block(&self, tenant: TenantId, subject: &BlockSubject, n: u32) -> Result<SystemTime> {
        let until = (self.0.now)()
            .checked_add(self.0.cfg.block_for)
            .ok_or_else(|| Error::invalid("abuse: block_for is too large"))?;
        let reason = format!("{n} failed attempts within {:?}", self.0.cfg.window);
        match self
            .0
            .store
            .block(tenant, subject, until.into(), &reason)
            .await
        {
            Ok(()) => {
                tracing::warn!(%tenant, %subject, failures = n, "abuse: blocked");
                // Свой узел — сразу, не дожидаясь сигнала.
                self.insert(tenant, *subject, until);
                // Хранилище снова доступно — блокировки узла становятся общими.
                self.flush_own(tenant).await;
            }
            Err(e) => {
                tracing::warn!(%tenant, %subject, failures = n, error = %e,
                    "abuse: store unavailable, blocked on this node");
                let now = (self.0.now)();
                let mut local = self.write();
                let own = local.own.entry(tenant).or_default();
                own.retain(|_, u| *u > now);
                let e = own.entry(*subject).or_insert(until);
                *e = (*e).max(until);
            }
        }
        Ok(until)
    }

    /// Записывает в хранилище блокировки узла организации; первая ошибка — остаток ждёт следующей попытки.
    async fn flush_own(&self, tenant: TenantId) {
        let now = (self.0.now)();
        let pending: Vec<(BlockSubject, SystemTime)> = {
            let mut local = self.write();
            let Some(own) = local.own.get_mut(&tenant) else {
                return;
            };
            own.retain(|_, u| *u > now);
            let pending = own.iter().map(|(s, u)| (*s, *u)).collect();
            if own.is_empty() {
                local.own.remove(&tenant);
            }
            pending
        };
        for (subject, until) in pending {
            let reason = "blocked on node while the block store was unavailable";
            if let Err(e) = self
                .0
                .store
                .block(tenant, &subject, until.into(), reason)
                .await
            {
                tracing::warn!(%tenant, error = %e, "abuse: node blocks not written yet");
                return;
            }
            self.insert(tenant, subject, until);
            let mut local = self.write();
            if let Some(own) = local.own.get_mut(&tenant) {
                if own.get(&subject).is_some_and(|u| *u <= until) {
                    own.remove(&subject);
                }
                if own.is_empty() {
                    local.own.remove(&tenant);
                }
            }
        }
    }

    async fn ensure_loaded(&self, tenant: TenantId) -> Result<()> {
        if self.read().tenants.contains_key(&tenant) {
            return Ok(());
        }
        let epoch = self.0.epoch.load(Ordering::Acquire);
        let blocks = self.0.store.active(tenant).await?;
        let set: HashMap<_, _> = blocks
            .into_iter()
            .map(|b| (b.subject, SystemTime::from(b.until)))
            .collect();
        {
            let mut local = self.write();
            // Сигнал во время загрузки: снимок мог устареть — не кэшируем, перечитаем при следующей проверке.
            if self.0.epoch.load(Ordering::Acquire) == epoch {
                local.tenants.entry(tenant).or_insert(set);
            }
        }
        // Хранилище доступно — блокировки узла, накопленные без него, становятся общими.
        self.flush_own(tenant).await;
        Ok(())
    }

    fn insert(&self, tenant: TenantId, subject: BlockSubject, until: SystemTime) {
        let now = (self.0.now)();
        let mut local = self.write();
        if let Some(set) = local.tenants.get_mut(&tenant) {
            set.retain(|_, u| *u > now);
            let e = set.entry(subject).or_insert(until);
            *e = (*e).max(until);
        }
    }

    fn reset(&self) {
        self.bump();
        self.write().tenants.clear();
    }

    fn bump(&self) {
        self.0.epoch.fetch_add(1, Ordering::AcqRel);
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, Local> {
        self.0.local.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Local> {
        self.0.local.write().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Субъекты, блокировка которых запрещает попытку: адрес, все подсети с ним, учётная запись.
fn subjects_to_check(attempt: &Attempt) -> impl Iterator<Item = BlockSubject> + '_ {
    let ip = attempt.ip.into_iter().flat_map(|ip| {
        let max = if ip.is_ipv4() { 32 } else { 128 };
        std::iter::once(BlockSubject::Ip(ip))
            .chain((0..=max).filter_map(move |p| BlockSubject::net(ip, p).ok()))
    });
    ip.chain(attempt.account.map(BlockSubject::Account))
}

async fn next(stream: &mut EventStream) -> Option<Event> {
    poll_fn(|cx| stream.as_mut().poll_next(cx)).await
}

#[cfg(test)]
mod tests;
