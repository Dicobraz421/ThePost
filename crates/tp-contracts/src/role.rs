//! Роли узла, пресеты и запуск ролей (CONTRACTS.md#Roles, #Config; SRS 2.1).

use std::any::Any;
use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::FutureExt;
use futures::channel::oneshot;
use futures::future::Shared;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

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

/// Источник актуальной конфигурации роли (hot reload, CONTRACTS.md#Config).
///
/// Значение — секция конфигурации крейта роли (`Arc<XxxConfig>`), стёртая до `Any`;
/// типизированный доступ — [`RoleCtx::config`]. Реализация приложения — поверх `tokio::sync::watch`.
#[async_trait]
pub trait ConfigWatch: Send + Sync + 'static {
    /// Текущая версия секции.
    fn current(&self) -> Arc<dyn Any + Send + Sync>;

    /// Ждёт следующей версии. `Error::Unavailable` — источник закрыт (процесс завершается).
    async fn changed(&self) -> Result<()>;
}

/// Неизменяемая конфигурация (тесты, роли без hot reload): `changed` не завершается.
pub struct FixedConfig(Arc<dyn Any + Send + Sync>);

impl FixedConfig {
    /// Из значения секции.
    pub fn new<T: Any + Send + Sync>(value: T) -> Self {
        FixedConfig(Arc::new(value))
    }
}

impl fmt::Debug for FixedConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FixedConfig")
    }
}

#[async_trait]
impl ConfigWatch for FixedConfig {
    fn current(&self) -> Arc<dyn Any + Send + Sync> {
        self.0.clone()
    }

    async fn changed(&self) -> Result<()> {
        futures::future::pending().await
    }
}

/// Сигнал остановки роли. Клонируется; срабатывает при [`ShutdownTrigger::trigger`] или удалении триггера.
#[derive(Clone)]
pub struct Shutdown(Shared<oneshot::Receiver<()>>);

/// Сторона, подающая сигнал остановки (процесс приложения).
#[derive(Debug)]
pub struct ShutdownTrigger(oneshot::Sender<()>);

impl Shutdown {
    /// Новая пара «сигнал — триггер».
    pub fn new() -> (Shutdown, ShutdownTrigger) {
        let (tx, rx) = oneshot::channel();
        (Shutdown(rx.shared()), ShutdownTrigger(tx))
    }

    /// Сигнал уже подан.
    pub fn is_triggered(&self) -> bool {
        self.0.clone().now_or_never().is_some()
    }

    /// Ждёт сигнала.
    pub async fn triggered(&self) {
        // Ошибка `Canceled` — триггер удалён, что тоже означает остановку.
        let _ = self.0.clone().await;
    }
}

impl fmt::Debug for Shutdown {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Shutdown")
            .field(&self.is_triggered())
            .finish()
    }
}

impl ShutdownTrigger {
    /// Подаёт сигнал остановки.
    pub fn trigger(self) {
        let _ = self.0.send(());
    }
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
    pub config: Arc<dyn ConfigWatch>,
    /// Сигнал остановки: перестать принимать новую работу и завершить текущую.
    pub shutdown: Shutdown,
    /// Время на завершение текущей работы после сигнала; затем соединения закрываются принудительно.
    pub drain_deadline: Duration,
    /// Признак готовности.
    pub ready: ReadyHandle,
}

impl RoleCtx {
    /// Текущая секция конфигурации как `T`; другой тип — `Error::Internal`.
    pub fn config<T: Any + Send + Sync>(&self) -> Result<Arc<T>> {
        self.config
            .current()
            .downcast::<T>()
            .map_err(|_| Error::internal("role config type mismatch"))
    }
}

impl fmt::Debug for RoleCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RoleCtx")
            .field("shutdown", &self.shutdown)
            .field("drain_deadline", &self.drain_deadline)
            .field("ready", &self.ready)
            .finish_non_exhaustive()
    }
}

/// Сервис роли. Ленивая инициализация (SRS 2.7): тяжёлые ресурсы — внутри `run`, не в конструкторе.
#[async_trait]
pub trait RoleService: Send + Sync + 'static {
    /// Имя роли (`Role::as_str` или имя роли модуля).
    fn name(&self) -> &str;

    /// Работает до сигнала `ctx.shutdown`, затем завершает текущую работу за `ctx.drain_deadline`.
    /// Возврат ошибки — аварийное завершение роли.
    async fn run(&self, ctx: RoleCtx) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::_private::serde_json;

    #[test]
    fn object_safe() {
        fn _svc(_: Arc<dyn RoleService>) {}
        fn _cfg(_: Arc<dyn ConfigWatch>) {}
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
        let (sd, trigger) = Shutdown::new();
        let sd2 = sd.clone();
        assert!(!sd.is_triggered());
        trigger.trigger();
        futures::executor::block_on(sd2.triggered());
        assert!(sd.is_triggered());

        let (sd, trigger) = Shutdown::new();
        drop(trigger);
        futures::executor::block_on(sd.triggered());

        let ready = ReadyHandle::default();
        let ctx = RoleCtx {
            config: Arc::new(FixedConfig::new(5u32)),
            shutdown: sd,
            drain_deadline: Duration::from_secs(1),
            ready: ready.clone(),
        };
        ctx.ready.set(true);
        assert!(ready.is_ready());
        assert_eq!(*ctx.config::<u32>().expect("u32"), 5);
        assert!(ctx.config::<String>().is_err());
    }
}
