//! Роли узла, пресеты и запуск ролей (CONTRACTS.md#Roles, #Config; SRS 2.1).

use std::any::Any;
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
pub use crate::load::{LoadLevel, LoadMonitor, fixed_load};

macro_rules! str_enum {
    ($(#[$m:meta])* $name:ident { $($(#[$vm:meta])* $v:ident = $s:literal,)+ }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
        pub enum $name {
            $($(#[$vm])* #[serde(rename = $s)] $v,)+
        }

        impl $name {
            /// Все значения в порядке объявления.
            pub const ALL: &'static [$name] = &[$($name::$v,)+];

            /// Строка конфигурации.
            pub const fn as_str(self) -> &'static str {
                match self { $($name::$v => $s,)+ }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl FromStr for $name {
            type Err = Error;

            fn from_str(s: &str) -> Result<Self> {
                match s {
                    $($s => Ok($name::$v),)+
                    _ => Err(Error::Invalid(format!(concat!("unknown ", stringify!($name), ": {}"), s))),
                }
            }
        }
    };
}

str_enum!(
    /// Роль ядра (SRS 2.1, таблица «Роли ядра»).
    Role {
        /// Приём почты (25); режим edge — параметр smtp-in (SRS 2.6).
        SmtpIn = "smtp-in",
        /// Отправка клиентами (587, 465).
        Submission = "submission",
        /// Очередь исходящей почты.
        SmtpOut = "smtp-out",
        /// IMAP4rev2, POP3.
        Imap = "imap",
        /// JMAP.
        Jmap = "jmap",
        /// CalDAV, CardDAV.
        Dav = "dav",
        /// Admin REST API.
        Admin = "admin",
        /// Выполнение поисковых запросов.
        Search = "search",
        /// Построение индексов.
        Indexer = "indexer",
        /// GC, «сироты», verify, фоновые задания.
        Maintenance = "maintenance",
    }
);

str_enum!(
    /// Пресет ролей (SRS 2.1, таблица «Пресеты ролей»).
    RolePreset {
        /// Все роли (SMB: одна ВМ).
        All = "all",
        /// smtp-in в режиме edge (DMZ).
        Edge = "edge",
        /// smtp-in, smtp-out.
        Transport = "transport",
        /// submission, imap, jmap, dav, admin.
        Access = "access",
        /// search, indexer, maintenance.
        Backend = "backend",
        /// transport + access + backend.
        Mailbox = "mailbox",
    }
);

impl RolePreset {
    /// Роли пресета.
    pub fn roles(self) -> &'static [Role] {
        use Role::*;
        match self {
            RolePreset::All | RolePreset::Mailbox => Role::ALL,
            RolePreset::Edge => &[SmtpIn],
            RolePreset::Transport => &[SmtpIn, SmtpOut],
            RolePreset::Access => &[Submission, Imap, Jmap, Dav, Admin],
            RolePreset::Backend => &[Search, Indexer, Maintenance],
        }
    }
}

/// Элемент параметра `roles` в config.yaml: пресет или отдельная роль (имена не пересекаются).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum RoleEntry {
    /// Пресет.
    Preset(RolePreset),
    /// Отдельная роль.
    Role(Role),
}

impl FromStr for RoleEntry {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        s.parse()
            .map(RoleEntry::Preset)
            .or_else(|_| s.parse().map(RoleEntry::Role))
            .map_err(|_| Error::Invalid(format!("unknown role or preset: {s}")))
    }
}

impl fmt::Display for RoleEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RoleEntry::Preset(p) => p.fmt(f),
            RoleEntry::Role(r) => r.fmt(f),
        }
    }
}

/// Набор ролей узла после раскрытия пресетов.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RoleSet {
    /// Роли.
    pub roles: BTreeSet<Role>,
    /// Указан пресет `edge`: smtp-in работает в режиме edge (SRS 2.6).
    pub edge: bool,
}

impl RoleSet {
    /// Раскрывает пресеты и объединяет с отдельными ролями.
    pub fn expand(entries: &[RoleEntry]) -> RoleSet {
        let mut set = RoleSet::default();
        for entry in entries {
            match entry {
                RoleEntry::Preset(p) => {
                    set.edge |= *p == RolePreset::Edge;
                    set.roles.extend(p.roles().iter().copied());
                }
                RoleEntry::Role(r) => {
                    set.roles.insert(*r);
                }
            }
        }
        set
    }

    /// Роль входит в набор.
    pub fn contains(&self, role: Role) -> bool {
        self.roles.contains(&role)
    }
}

// ---------- Запуск ролей ----------

pub use tokio::sync::watch;
pub use tokio_util::sync::CancellationToken;

/// Значение секции конфигурации роли (`Arc<XxxConfig>`), стёртое до `Any`; типизированно — [`RoleCtx::config`].
pub type ConfigValue = Arc<dyn Any + Send + Sync>;

/// Источник актуальной секции конфигурации роли (hot reload, CONTRACTS.md#Config): приложение рассылает
/// новую версию через [`watch::Sender`]. Закрытый канал — новых версий не будет (значение остаётся в силе).
pub type ConfigRx = watch::Receiver<ConfigValue>;

/// Канал секции конфигурации со значением `value`: приложение держит `Sender` и рассылает новые версии.
pub fn config_channel<T: Any + Send + Sync>(value: T) -> (watch::Sender<ConfigValue>, ConfigRx) {
    watch::channel(Arc::new(value) as ConfigValue)
}

/// Неизменяемая секция (тесты, роли без hot reload): канал сразу закрыт, [`RoleCtx::config_changed`] не завершается.
pub fn fixed_config<T: Any + Send + Sync>(value: T) -> ConfigRx {
    config_channel(value).1
}

/// Признак готовности роли (readiness probe): роль выставляет его, когда начала обслуживать.
#[derive(Debug, Clone, Default)]
pub struct ReadyHandle(Arc<AtomicBool>);

impl ReadyHandle {
    /// Роль готова (`true`) или временно не готова (`false`).
    pub fn set(&self, ready: bool) {
        self.0.store(ready, Ordering::Release);
    }

    /// Текущее состояние.
    pub fn is_ready(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Контекст запуска роли.
#[derive(Clone)]
pub struct RoleCtx {
    /// Секция конфигурации роли с hot reload.
    pub config: ConfigRx,
    /// Сигнал остановки: перестать принимать новую работу и завершить текущую.
    pub shutdown: CancellationToken,
    /// Время на завершение текущей работы после сигнала; затем соединения закрываются принудительно.
    pub drain_deadline: Duration,
    /// Признак готовности.
    pub ready: ReadyHandle,
    /// Уровень нагрузки узла (SRS 6.3): на `Elevated` фоновые задания приостанавливаются,
    /// на `High` новые сессии отклоняются или ограничиваются. Без источников — [`fixed_load`].
    pub load: Arc<dyn LoadMonitor>,
}

impl RoleCtx {
    /// Текущая секция конфигурации как `T`; другой тип — `Error::Internal`.
    pub fn config<T: Any + Send + Sync>(&self) -> Result<Arc<T>> {
        self.config
            .borrow()
            .clone()
            .downcast::<T>()
            .map_err(|_| Error::internal("role config type mismatch"))
    }

    /// Ждёт следующей версии секции (затем — [`RoleCtx::config`]). Источник закрыт — не завершается:
    /// конфигурация больше не меняется, а остановку роли сигналит `shutdown`. Удобно в `tokio::select!`.
    pub async fn config_changed(&mut self) {
        if self.config.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }

    /// Текущий уровень нагрузки узла.
    pub fn load_level(&self) -> LoadLevel {
        self.load.level()
    }
}

impl fmt::Debug for RoleCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoleCtx")
            .field("shutdown", &self.shutdown)
            .field("drain_deadline", &self.drain_deadline)
            .field("ready", &self.ready)
            .field("load", &self.load.level())
            .finish_non_exhaustive()
    }
}

/// Сервис роли. Ленивая инициализация (SRS 2.7): тяжёлые ресурсы — внутри `run`, не в конструкторе.
#[async_trait]
pub trait RoleService: Send + Sync + 'static {
    /// Имя роли (`Role::as_str` или имя роли модуля).
    fn name(&self) -> &str;

    /// Работает до отмены `ctx.shutdown`, затем завершает текущую работу за `ctx.drain_deadline`.
    /// Возврат ошибки — аварийное завершение роли.
    async fn run(&self, ctx: RoleCtx) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_safe() {
        fn _svc(_: Arc<dyn RoleService>) {}
    }

    #[test]
    fn strings_roundtrip() {
        for r in Role::ALL {
            assert_eq!(r.as_str().parse::<Role>(), Ok(*r));
            assert_eq!(
                serde_json::to_string(r).expect("ser"),
                format!("\"{}\"", r.as_str())
            );
            assert_eq!(r.as_str().parse(), Ok(RoleEntry::Role(*r)));
        }
        for p in RolePreset::ALL {
            assert_eq!(p.as_str().parse::<RolePreset>(), Ok(*p));
            assert_eq!(p.as_str().parse(), Ok(RoleEntry::Preset(*p)));
        }
        assert!("edge-in".parse::<RoleEntry>().is_err());
        let entries: Vec<RoleEntry> =
            serde_json::from_str(r#"["mailbox", "imap"]"#).expect("parse");
        assert_eq!(
            entries,
            [
                RoleEntry::Preset(RolePreset::Mailbox),
                RoleEntry::Role(Role::Imap)
            ]
        );
        assert!(serde_json::from_str::<RoleEntry>(r#""bogus""#).is_err());
    }

    #[test]
    fn presets_expand_per_srs_2_1() {
        use Role::*;
        let set = |e: &[RoleEntry]| RoleSet::expand(e).roles.into_iter().collect::<Vec<_>>();
        let p = RoleEntry::Preset;
        assert_eq!(set(&[p(RolePreset::All)]), Role::ALL);
        assert_eq!(set(&[p(RolePreset::Edge)]), [SmtpIn]);
        assert!(RoleSet::expand(&[p(RolePreset::Edge)]).edge);
        assert!(!RoleSet::expand(&[p(RolePreset::Transport)]).edge);
        assert_eq!(set(&[p(RolePreset::Transport)]), [SmtpIn, SmtpOut]);
        assert_eq!(
            set(&[p(RolePreset::Access)]),
            [Submission, Imap, Jmap, Dav, Admin]
        );
        assert_eq!(
            set(&[p(RolePreset::Backend)]),
            [Search, Indexer, Maintenance]
        );
        // mailbox = transport + access + backend = все роли.
        assert_eq!(
            set(&[p(RolePreset::Mailbox)]),
            set(&[
                p(RolePreset::Transport),
                p(RolePreset::Access),
                p(RolePreset::Backend)
            ])
        );
        assert_eq!(set(&[p(RolePreset::Mailbox)]), Role::ALL);
        assert_eq!(
            set(&[p(RolePreset::Backend), RoleEntry::Role(Imap)]),
            [Imap, Search, Indexer, Maintenance]
        );
    }

    #[test]
    fn shutdown_and_ready() {
        let shutdown = CancellationToken::new();
        let ready = ReadyHandle::default();
        let ctx = RoleCtx {
            config: fixed_config(5u32),
            shutdown: shutdown.clone(),
            drain_deadline: Duration::from_secs(1),
            ready: ready.clone(),
            load: fixed_load(LoadLevel::High),
        };
        assert!(!ctx.shutdown.is_cancelled());
        shutdown.cancel();
        futures::executor::block_on(ctx.shutdown.cancelled());
        ctx.ready.set(true);
        assert!(ready.is_ready());
        assert_eq!(*ctx.config::<u32>().expect("u32"), 5);
        assert!(ctx.config::<String>().is_err());
        assert_eq!(ctx.load_level(), LoadLevel::High);
        assert!(format!("{ctx:?}").contains("High"));
    }

    #[test]
    fn config_reload_and_fixed() {
        use futures::FutureExt;

        let (tx, rx) = config_channel(1u32);
        let mut ctx = RoleCtx {
            config: rx,
            shutdown: CancellationToken::new(),
            drain_deadline: Duration::from_secs(1),
            ready: ReadyHandle::default(),
            load: fixed_load(LoadLevel::Normal),
        };
        assert!(
            ctx.config_changed().now_or_never().is_none(),
            "no new version yet"
        );
        tx.send_replace(Arc::new(2u32));
        futures::executor::block_on(ctx.config_changed());
        assert_eq!(*ctx.config::<u32>().expect("u32"), 2);

        // Источник закрыт: значение остаётся, ожидание новой версии не завершается.
        drop(tx);
        assert!(ctx.config_changed().now_or_never().is_none());
        assert_eq!(*ctx.config::<u32>().expect("u32"), 2);
        ctx.config = fixed_config(3u32);
        assert!(ctx.config_changed().now_or_never().is_none());
        assert_eq!(*ctx.config::<u32>().expect("u32"), 3);
    }
}
