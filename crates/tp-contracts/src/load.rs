//! Поведение под нагрузкой: уровни нагрузки узла (CONTRACTS.md#Load; SRS 6.3, решение 2026-11-01-borrowings-security).
//!
//! [`LoadMonitor`] вычисляет уровень по показателям узла (CPU и память относительно лимитов cgroups, место на дисках
//! кэша, занятость пула PostgreSQL, длина очередей, задержка записи) с гистерезисом; реализация — `tp-platform`
//! (PLT-15), подменная — `tp_testkit::MemLoadMonitor`. Роли получают монитор в [`crate::role::RoleCtx::load`].
//!
//! ```
//! use tp_contracts::load::{LoadLevel, fixed_load};
//!
//! let load = fixed_load(LoadLevel::Normal);
//! assert!(!load.level().pauses_background());
//! assert_eq!(*load.subscribe().borrow(), LoadLevel::Normal);
//! ```

use std::fmt;
use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

/// Уровень нагрузки узла; порядок — по возрастанию нагрузки.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Default,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum LoadLevel {
    /// Штатная работа.
    #[default]
    Normal,
    /// «Повышенный» (по умолчанию 85 %): меньше параллельность новых сессий, замедление ответов
    /// подозрительным источникам, фоновые задания (индексация, GC, миграция) приостановлены.
    Elevated,
    /// «Высокий» (по умолчанию 95 %): новым сессиям SMTP — временный отказ 4xx, новые сессии IMAP и JMAP
    /// ограничены; уже принятые письма обрабатываются.
    High,
}

impl LoadLevel {
    /// Все значения по возрастанию нагрузки.
    pub const ALL: &'static [LoadLevel] =
        &[LoadLevel::Normal, LoadLevel::Elevated, LoadLevel::High];

    /// Строка метрик и журнала (`normal`, `elevated`, `high`) — та же, что в JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            LoadLevel::Normal => "normal",
            LoadLevel::Elevated => "elevated",
            LoadLevel::High => "high",
        }
    }

    /// Фоновые задания приостановлены (уровень не ниже `Elevated`).
    pub fn pauses_background(self) -> bool {
        self >= LoadLevel::Elevated
    }

    /// Новые сессии отклоняются или ограничиваются (уровень `High`).
    pub fn limits_new_sessions(self) -> bool {
        self >= LoadLevel::High
    }
}

impl fmt::Display for LoadLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Подписка на уровень нагрузки: новое значение — при переходе между уровнями.
pub type LoadRx = watch::Receiver<LoadLevel>;

/// Монитор нагрузки узла. Переходы — с гистерезисом; закрытый канал подписки — уровень больше не меняется.
pub trait LoadMonitor: Send + Sync + 'static {
    /// Подписка на уровень (`borrow()` — текущий, `changed()` — ожидание перехода).
    fn subscribe(&self) -> LoadRx;

    /// Текущий уровень.
    fn level(&self) -> LoadLevel {
        *self.subscribe().borrow()
    }
}

/// Монитор с постоянным уровнем: канал открыт, `changed()` не завершается.
struct FixedLoad(watch::Sender<LoadLevel>);

impl LoadMonitor for FixedLoad {
    fn subscribe(&self) -> LoadRx {
        self.0.subscribe()
    }
}

/// Монитор с постоянным уровнем `level` — для тестов и узлов без источников показателей.
pub fn fixed_load(level: LoadLevel) -> Arc<dyn LoadMonitor> {
    Arc::new(FixedLoad(watch::channel(level).0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::FutureExt;

    #[test]
    fn object_safe() {
        fn _monitor(_: Arc<dyn LoadMonitor>) {}
    }

    #[test]
    fn levels() {
        assert!(LoadLevel::Normal < LoadLevel::Elevated && LoadLevel::Elevated < LoadLevel::High);
        assert_eq!(LoadLevel::default(), LoadLevel::Normal);
        let flags: Vec<_> = LoadLevel::ALL
            .iter()
            .map(|l| (l.pauses_background(), l.limits_new_sessions()))
            .collect();
        assert_eq!(flags, [(false, false), (true, false), (true, true)]);
        for l in LoadLevel::ALL {
            assert_eq!(serde_json::to_string(l).expect("ser"), format!("\"{l}\""));
            assert_eq!(
                serde_json::from_str::<LoadLevel>(&format!("\"{}\"", l.as_str())).expect("de"),
                *l
            );
        }
    }

    #[test]
    fn fixed_never_changes() {
        let load = fixed_load(LoadLevel::Elevated);
        assert_eq!(load.level(), LoadLevel::Elevated);
        let mut rx = load.subscribe();
        assert!(
            rx.changed().now_or_never().is_none(),
            "канал открыт, перехода нет"
        );
        assert_eq!(*rx.borrow(), LoadLevel::Elevated);
    }
}
