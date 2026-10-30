//! Ограничение частоты (SRS 6.3): token bucket по IP-адресу, подсети (/24 для IPv4, /64 для IPv6)
//! и учётной записи; временная блокировка источника (адреса или подсети) с TTL при превышении.
//!
//! Время передаётся явно (`*_at`), поэтому тесты не зависят от часов; обёртки без суффикса берут `Instant::now()`.
//! Состояние — в памяти узла; число отслеживаемых ключей ограничено `max_entries`.

use std::collections::HashMap;
use std::fmt;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use schemars::JsonSchema;
use serde::Deserialize;

use crate::listen::Cidr;

/// Секция `ratelimit`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RateLimitConfig {
    /// Лимит на IP-адрес клиента.
    pub ip: BucketConfig,
    /// Лимит на подсеть IPv4 (`ipv4_prefix`).
    pub subnet_v4: BucketConfig,
    /// Лимит на подсеть IPv6 (`ipv6_prefix`).
    pub subnet_v6: BucketConfig,
    /// Лимит на учётную запись (попытки аутентификации, отправка).
    pub account: BucketConfig,
    /// Длина префикса подсети IPv4.
    pub ipv4_prefix: u8,
    /// Длина префикса подсети IPv6.
    pub ipv6_prefix: u8,
    /// Блокировка источника (адреса или подсети) при исчерпании лимита, с; 0 — без блокировки, только отказ.
    pub block_secs: u64,
    /// Предел числа отслеживаемых ключей; при превышении удаляются восполненные корзины и истёкшие блокировки.
    pub max_entries: usize,
}

impl Default for RateLimitConfig {
    fn default() -> Self {
        Self {
            ip: BucketConfig {
                burst: 60,
                per_minute: 60,
            },
            subnet_v4: BucketConfig {
                burst: 600,
                per_minute: 600,
            },
            subnet_v6: BucketConfig {
                burst: 600,
                per_minute: 600,
            },
            account: BucketConfig {
                burst: 20,
                per_minute: 10,
            },
            ipv4_prefix: 24,
            ipv6_prefix: 64,
            block_secs: 900,
            max_entries: 100_000,
        }
    }
}

/// Параметры корзины: `burst` событий сразу, восполнение `per_minute` в минуту. `burst: 0` — лимит выключен.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BucketConfig {
    /// Ёмкость корзины.
    pub burst: u32,
    /// Восполнение, событий в минуту.
    pub per_minute: u32,
}

impl BucketConfig {
    fn enabled(self) -> bool {
        self.burst > 0
    }

    /// Восполнение, событий в секунду.
    fn rate(self) -> f64 {
        f64::from(self.per_minute) / 60.0
    }
}

/// Решение ограничителя.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Разрешено.
    Allow,
    /// Лимит исчерпан; повторить не раньше чем через `retry_after`.
    Limited {
        /// Время до восполнения одного события.
        retry_after: Duration,
    },
    /// Источник заблокирован ещё на `remaining`.
    Blocked {
        /// Остаток блокировки.
        remaining: Duration,
    },
}

impl Verdict {
    /// Разрешено.
    pub fn is_allowed(self) -> bool {
        self == Verdict::Allow
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    Ip(IpAddr),
    Subnet(IpAddr, u8),
    Account(String),
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    updated: Instant,
}

impl Bucket {
    fn full(cfg: BucketConfig, now: Instant) -> Self {
        Self {
            tokens: f64::from(cfg.burst),
            updated: now,
        }
    }

    fn refill(&mut self, cfg: BucketConfig, now: Instant) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.tokens = (self.tokens + elapsed * cfg.rate()).min(f64::from(cfg.burst));
        self.updated = now;
    }

    /// Списывает событие; `Err` — время до восполнения одного события.
    fn take(&mut self, cfg: BucketConfig, now: Instant) -> Result<(), Duration> {
        self.refill(cfg, now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            return Ok(());
        }
        let rate = cfg.rate();
        Err(if rate > 0.0 {
            Duration::from_secs_f64((1.0 - self.tokens) / rate)
        } else {
            Duration::MAX
        })
    }
}

#[derive(Debug, Default)]
struct Entry {
    bucket: Option<Bucket>,
    blocked_until: Option<Instant>,
}

/// Ограничитель частоты узла. Разделяется между соединениями через `Arc`.
pub struct RateLimiter {
    cfg: RwLock<Arc<RateLimitConfig>>,
    state: Mutex<HashMap<Key, Entry>>,
}

impl fmt::Debug for RateLimiter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RateLimiter")
            .field("entries", &self.len())
            .finish_non_exhaustive()
    }
}

impl RateLimiter {
    /// Ограничитель с параметрами `cfg`.
    pub fn new(cfg: RateLimitConfig) -> Self {
        Self {
            cfg: RwLock::new(Arc::new(cfg)),
            state: Mutex::new(HashMap::new()),
        }
    }

    /// Применяет новую версию секции (hot reload); накопленное состояние сохраняется.
    pub fn apply(&self, cfg: RateLimitConfig) {
        *self.cfg.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(cfg);
    }

    /// Новое соединение или попытка с адреса `ip` (лимиты адреса и подсети).
    pub fn check_ip(&self, ip: IpAddr) -> Verdict {
        self.check_ip_at(ip, Instant::now())
    }

    /// [`Self::check_ip`] на момент `now`.
    pub fn check_ip_at(&self, ip: IpAddr, now: Instant) -> Verdict {
        let cfg = self.config();
        let ip = ip.to_canonical();
        let (subnet_cfg, prefix) = match ip {
            IpAddr::V4(_) => (cfg.subnet_v4, cfg.ipv4_prefix),
            IpAddr::V6(_) => (cfg.subnet_v6, cfg.ipv6_prefix),
        };
        let ip_key = Key::Ip(ip);
        let subnet_key = Key::Subnet(subnet(ip, prefix), prefix);
        let mut state = self.lock();
        for key in [&ip_key, &subnet_key] {
            if let Some(remaining) = blocked(&state, key, now) {
                return Verdict::Blocked { remaining };
            }
        }
        // Подсеть проверяется первой: её отказ не расходует лимит адреса.
        for (key, bucket) in [(subnet_key, subnet_cfg), (ip_key, cfg.ip)] {
            if let Err(retry_after) = take(&mut state, &cfg, key.clone(), bucket, now) {
                return self.exceeded(&mut state, &cfg, key, retry_after, now);
            }
        }
        Verdict::Allow
    }

    /// Попытка для учётной записи `account` (аутентификация, отправка). Учётная запись не блокируется:
    /// блокировка по имени позволила бы постороннему запереть владельца; отказ — `Limited`.
    pub fn check_account(&self, account: &str) -> Verdict {
        self.check_account_at(account, Instant::now())
    }

    /// [`Self::check_account`] на момент `now`.
    pub fn check_account_at(&self, account: &str, now: Instant) -> Verdict {
        let cfg = self.config();
        let key = Key::Account(account.to_lowercase());
        let mut state = self.lock();
        match take(&mut state, &cfg, key, cfg.account, now) {
            Ok(()) => Verdict::Allow,
            Err(retry_after) => Verdict::Limited { retry_after },
        }
    }

    /// Блокирует адрес `ip` на `block_secs` (например, после серии неудачных аутентификаций).
    pub fn block_ip_at(&self, ip: IpAddr, now: Instant) {
        let cfg = self.config();
        let mut state = self.lock();
        block(&mut state, &cfg, Key::Ip(ip.to_canonical()), now);
    }

    /// Адрес `ip` или его подсеть заблокированы на момент `now`.
    pub fn is_blocked_at(&self, ip: IpAddr, now: Instant) -> bool {
        let cfg = self.config();
        let ip = ip.to_canonical();
        let prefix = match ip {
            IpAddr::V4(_) => cfg.ipv4_prefix,
            IpAddr::V6(_) => cfg.ipv6_prefix,
        };
        let state = self.lock();
        blocked(&state, &Key::Ip(ip), now).is_some()
            || blocked(&state, &Key::Subnet(subnet(ip, prefix), prefix), now).is_some()
    }

    /// Удаляет восполненные корзины и истёкшие блокировки.
    pub fn purge_at(&self, now: Instant) {
        let cfg = self.config();
        purge(&mut self.lock(), &cfg, now);
    }

    /// Число отслеживаемых ключей.
    pub fn len(&self) -> usize {
        self.lock().len()
    }

    /// Нет отслеживаемых ключей.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn exceeded(
        &self,
        state: &mut HashMap<Key, Entry>,
        cfg: &RateLimitConfig,
        key: Key,
        retry_after: Duration,
        now: Instant,
    ) -> Verdict {
        if cfg.block_secs == 0 {
            return Verdict::Limited { retry_after };
        }
        tracing::warn!(source = %describe(&key), block_secs = cfg.block_secs, "rate limit exceeded, source blocked");
        block(state, cfg, key, now);
        Verdict::Blocked {
            remaining: Duration::from_secs(cfg.block_secs),
        }
    }

    fn config(&self) -> Arc<RateLimitConfig> {
        self.cfg
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Key, Entry>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

fn blocked(state: &HashMap<Key, Entry>, key: &Key, now: Instant) -> Option<Duration> {
    let until = state.get(key)?.blocked_until?;
    until
        .checked_duration_since(now)
        .filter(|left| !left.is_zero())
}

fn block(state: &mut HashMap<Key, Entry>, cfg: &RateLimitConfig, key: Key, now: Instant) {
    let until = now + Duration::from_secs(cfg.block_secs);
    reserve(state, cfg, now);
    let entry = state.entry(key).or_default();
    entry.blocked_until = Some(until);
    // После блокировки корзина начинается заново.
    entry.bucket = None;
}

fn take(
    state: &mut HashMap<Key, Entry>,
    cfg: &RateLimitConfig,
    key: Key,
    bucket: BucketConfig,
    now: Instant,
) -> Result<(), Duration> {
    if !bucket.enabled() {
        return Ok(());
    }
    if !state.contains_key(&key) && !reserve(state, cfg, now) {
        // Таблица заполнена активными ключами: новый источник не отслеживается, но и не отклоняется
        // (его ограничит корзина подсети, которая уже есть в таблице).
        return Ok(());
    }
    let entry = state.entry(key).or_default();
    entry
        .bucket
        .get_or_insert_with(|| Bucket::full(bucket, now))
        .take(bucket, now)
}

/// Освобождает место под новый ключ; `false` — место не освободилось.
fn reserve(state: &mut HashMap<Key, Entry>, cfg: &RateLimitConfig, now: Instant) -> bool {
    if state.len() < cfg.max_entries {
        return true;
    }
    purge(state, cfg, now);
    if state.len() >= cfg.max_entries {
        tracing::warn!(
            entries = state.len(),
            "rate limiter table is full, new sources are not tracked"
        );
        return false;
    }
    true
}

fn purge(state: &mut HashMap<Key, Entry>, cfg: &RateLimitConfig, now: Instant) {
    state.retain(|key, entry| {
        if entry.blocked_until.is_some_and(|until| until > now) {
            return true;
        }
        entry.blocked_until = None;
        let bucket_cfg = match key {
            Key::Ip(_) => cfg.ip,
            Key::Subnet(IpAddr::V4(_), _) => cfg.subnet_v4,
            Key::Subnet(IpAddr::V6(_), _) => cfg.subnet_v6,
            Key::Account(_) => cfg.account,
        };
        match &mut entry.bucket {
            Some(b) => {
                b.refill(bucket_cfg, now);
                b.tokens < f64::from(bucket_cfg.burst)
            }
            None => false,
        }
    });
}

/// Адрес сети: `ip` с обнулёнными битами за префиксом.
fn subnet(ip: IpAddr, prefix: u8) -> IpAddr {
    Cidr::new(ip, prefix).network()
}

fn describe(key: &Key) -> String {
    match key {
        Key::Ip(ip) => ip.to_string(),
        Key::Subnet(net, prefix) => format!("{net}/{prefix}"),
        // Имя учётной записи в логи не попадает.
        Key::Account(_) => "account".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().expect("ip")
    }

    fn cfg() -> RateLimitConfig {
        RateLimitConfig {
            ip: BucketConfig {
                burst: 3,
                per_minute: 60,
            },
            subnet_v4: BucketConfig {
                burst: 5,
                per_minute: 60,
            },
            subnet_v6: BucketConfig {
                burst: 5,
                per_minute: 60,
            },
            account: BucketConfig {
                burst: 2,
                per_minute: 6,
            },
            block_secs: 60,
            ..RateLimitConfig::default()
        }
    }

    /// Ручные часы: `Instant` от общего начала плюс сдвиг.
    struct Clock(Instant);

    impl Clock {
        fn at(&self, secs: f64) -> Instant {
            self.0 + Duration::from_secs_f64(secs)
        }
    }

    #[test]
    fn excess_from_ip_blocks_until_ttl() {
        let rl = RateLimiter::new(cfg());
        let clock = Clock(Instant::now());
        let a = ip("192.0.2.10");
        for _ in 0..3 {
            assert_eq!(rl.check_ip_at(a, clock.at(0.0)), Verdict::Allow);
        }
        assert_eq!(
            rl.check_ip_at(a, clock.at(0.0)),
            Verdict::Blocked {
                remaining: Duration::from_secs(60)
            }
        );
        assert!(rl.is_blocked_at(a, clock.at(30.0)));
        assert_eq!(
            rl.check_ip_at(a, clock.at(30.0)),
            Verdict::Blocked {
                remaining: Duration::from_secs(30)
            }
        );
        // Сосед по подсети не заблокирован: лимит подсети (5) ещё не исчерпан.
        assert!(
            rl.check_ip_at(ip("192.0.2.11"), clock.at(30.0))
                .is_allowed()
        );
        // По TTL блокировка снимается, корзина начинается заново.
        assert!(!rl.is_blocked_at(a, clock.at(60.0)));
        assert!(rl.check_ip_at(a, clock.at(60.0)).is_allowed());
    }

    #[test]
    fn bucket_refills_over_time() {
        let rl = RateLimiter::new(RateLimitConfig {
            block_secs: 0,
            ..cfg()
        });
        let clock = Clock(Instant::now());
        let a = ip("198.51.100.1");
        for _ in 0..3 {
            assert!(rl.check_ip_at(a, clock.at(0.0)).is_allowed());
        }
        assert_eq!(
            rl.check_ip_at(a, clock.at(0.0)),
            Verdict::Limited {
                retry_after: Duration::from_secs(1)
            }
        );
        assert!(!rl.is_blocked_at(a, clock.at(0.0)), "block_secs: 0");
        // 60 в минуту = 1 в секунду.
        assert!(rl.check_ip_at(a, clock.at(1.0)).is_allowed());
        assert!(!rl.check_ip_at(a, clock.at(1.0)).is_allowed());
    }

    #[test]
    fn subnet_v4_and_v6_are_limited_together() {
        let rl = RateLimiter::new(cfg());
        let clock = Clock(Instant::now());
        for i in 1..=5 {
            assert!(
                rl.check_ip_at(ip(&format!("203.0.113.{i}")), clock.at(0.0))
                    .is_allowed()
            );
        }
        // Шестой адрес той же /24 упирается в лимит подсети: блокируется вся подсеть.
        assert!(matches!(
            rl.check_ip_at(ip("203.0.113.200"), clock.at(0.0)),
            Verdict::Blocked { .. }
        ));
        assert!(rl.is_blocked_at(ip("203.0.113.1"), clock.at(1.0)));
        assert!(
            rl.check_ip_at(ip("203.0.114.1"), clock.at(1.0))
                .is_allowed()
        );
        // IPv4-mapped IPv6 — тот же адрес IPv4.
        assert!(rl.is_blocked_at(ip("::ffff:203.0.113.9"), clock.at(1.0)));

        for i in 1..=5 {
            let a = ip(&format!("2001:db8:1:2:{i}::1"));
            assert!(rl.check_ip_at(a, clock.at(0.0)).is_allowed());
        }
        assert!(
            !rl.check_ip_at(ip("2001:db8:1:2:ffff::1"), clock.at(0.0))
                .is_allowed()
        );
        assert!(
            rl.check_ip_at(ip("2001:db8:1:3::1"), clock.at(0.0))
                .is_allowed()
        );
        assert!(!rl.is_blocked_at(ip("203.0.113.1"), clock.at(60.0)));
    }

    #[test]
    fn account_is_limited_but_not_blocked() {
        let rl = RateLimiter::new(cfg());
        let clock = Clock(Instant::now());
        assert!(
            rl.check_account_at("Ann@Example.org", clock.at(0.0))
                .is_allowed()
        );
        assert!(
            rl.check_account_at("ann@example.org", clock.at(0.0))
                .is_allowed()
        );
        assert_eq!(
            rl.check_account_at("ann@example.org", clock.at(0.0)),
            Verdict::Limited {
                retry_after: Duration::from_secs(10)
            }
        );
        assert!(
            rl.check_account_at("bob@example.org", clock.at(0.0))
                .is_allowed()
        );
        assert!(
            rl.check_account_at("ann@example.org", clock.at(10.0))
                .is_allowed()
        );
    }

    #[test]
    fn explicit_block_and_disabled_limits() {
        let rl = RateLimiter::new(cfg());
        let clock = Clock(Instant::now());
        let a = ip("192.0.2.77");
        rl.block_ip_at(a, clock.at(0.0));
        assert!(matches!(
            rl.check_ip_at(a, clock.at(1.0)),
            Verdict::Blocked { .. }
        ));
        assert!(rl.check_ip_at(a, clock.at(61.0)).is_allowed());

        rl.apply(RateLimitConfig {
            ip: BucketConfig::default(),
            subnet_v4: BucketConfig::default(),
            ..cfg()
        });
        for _ in 0..100 {
            assert!(
                rl.check_ip_at(ip("192.0.2.78"), clock.at(61.0))
                    .is_allowed()
            );
        }
    }

    #[test]
    fn table_is_bounded_and_purged() {
        let rl = RateLimiter::new(RateLimitConfig {
            max_entries: 4,
            subnet_v4: BucketConfig::default(),
            ..cfg()
        });
        let clock = Clock(Instant::now());
        for i in 1..=10 {
            assert!(
                rl.check_ip_at(ip(&format!("10.0.{i}.1")), clock.at(0.0))
                    .is_allowed()
            );
        }
        assert_eq!(rl.len(), 4, "new sources beyond the limit are not tracked");
        // Через 3 с корзины (3 события, 1/с) восполнены и удаляются.
        rl.purge_at(clock.at(3.0));
        assert!(rl.is_empty());
    }

    #[test]
    fn subnet_masks() {
        assert_eq!(subnet(ip("203.0.113.77"), 24), ip("203.0.113.0"));
        assert_eq!(subnet(ip("203.0.113.77"), 0), ip("0.0.0.0"));
        assert_eq!(subnet(ip("203.0.113.77"), 32), ip("203.0.113.77"));
        assert_eq!(subnet(ip("2001:db8:1:2:3:4:5:6"), 64), ip("2001:db8:1:2::"));
        assert_eq!(subnet(ip("2001:db8::1"), 128), ip("2001:db8::1"));
    }

    #[test]
    fn config_from_yaml() {
        let cfg: RateLimitConfig =
            serde_yaml_ng::from_str("ip: { burst: 5, per_minute: 30 }\nblock_secs: 10\n")
                .expect("parse");
        assert_eq!(cfg.ip.burst, 5);
        assert_eq!(cfg.block_secs, 10);
        assert_eq!(cfg.ipv4_prefix, 24);
        assert!(serde_yaml_ng::from_str::<RateLimitConfig>("ip: { rate: 1 }").is_err());
    }
}
