//! Секция `sieve` config.yaml (SRS 5.5): лимиты разбора и выполнения скриптов, защита `redirect` от петли,
//! автоответ, кэш программ, ManageSieve.

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tp_contracts::config::duration;
use tp_platform::listen::EndpointConfig;

use crate::Limits;

/// Конфигурация Sieve (поля необязательны, неизвестное — ошибка).
///
/// ```yaml
/// max_steps: 10000000
/// max_redirects: 4
/// vacation:
///   default_days: 7
/// cache:
///   programs: 4096
/// managesieve:
///   idle_timeout: 30m
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SieveConfig {
    /// Размер текста скрипта, байт; больше — ошибка проверки.
    pub max_script_size: usize,
    /// Глубина вложенности блоков и тестов.
    pub max_nesting: usize,
    /// Элементов в списке строк или тестов.
    pub max_list_len: usize,
    /// Команд и тестов в скрипте.
    pub max_commands: usize,
    /// Действий за одно выполнение; больше — ошибка выполнения (письмо остаётся во «Входящих»).
    pub max_actions: usize,
    /// Шагов выполнения (команды, тесты, сравнения строк); больше — ошибка выполнения.
    pub max_steps: u64,
    /// Перенаправлений (`redirect`) на письмо за одно выполнение скрипта; больше — ошибка выполнения.
    pub max_redirects: usize,
    /// Объём текста письма, который читает тест `body`, байт.
    pub max_body_scan: usize,
    /// Заголовков `Received`, при превышении которых `redirect` отменяется (петля пересылки).
    pub max_received: usize,
    /// Заголовков `Received` этого узла (метка узла — `by <имя узла>`), начиная с которых `redirect` отменяется:
    /// письмо уже проходило узел столько раз.
    pub max_node_hops: usize,
    /// Автоответ.
    pub vacation: VacationConfig,
    /// Кэш скомпилированных программ.
    pub cache: CacheConfig,
    /// ManageSieve (RFC 5804; SIEVE-08).
    pub managesieve: ManageSieveConfig,
}

impl Default for SieveConfig {
    fn default() -> Self {
        let l = Limits::default();
        Self {
            max_script_size: l.max_script_size,
            max_nesting: l.max_nesting,
            max_list_len: l.max_list_len,
            max_commands: l.max_commands,
            max_actions: l.max_actions,
            max_steps: l.max_steps,
            max_redirects: l.max_redirects,
            max_body_scan: l.max_body_scan,
            max_received: 50,
            max_node_hops: 3,
            vacation: VacationConfig::default(),
            cache: CacheConfig::default(),
            managesieve: ManageSieveConfig::default(),
        }
    }
}

impl SieveConfig {
    /// Лимиты разбора и выполнения.
    pub fn limits(&self) -> Limits {
        Limits {
            max_script_size: self.max_script_size,
            max_nesting: self.max_nesting,
            max_list_len: self.max_list_len,
            max_commands: self.max_commands,
            max_actions: self.max_actions,
            max_steps: self.max_steps,
            max_redirects: self.max_redirects,
            max_body_scan: self.max_body_scan,
        }
    }
}

/// Кэш скомпилированных программ: по учётной записи и версии её скриптов (`SieveStore::state`), скрипт организации —
/// по тексту.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// Записей в кэше (учётных записей и отдельно организаций); сверх — вытесняется давно не использованная.
    /// 0 — без кэша.
    pub programs: usize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { programs: 4096 }
    }
}

/// Лимиты сеанса ManageSieve (SIEVE-08).
///
/// ```yaml
/// listen:
///   - listen: "[::]:4190"
/// idle_timeout: 30m
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ManageSieveConfig {
    /// Слушатели (порт 4190, `STARTTLS`); пусто — ManageSieve выключен. Требует перезапуска.
    pub listen: Vec<EndpointConfig>,
    /// Длина строки команды, байт; длиннее — `BYE` и закрытие.
    pub max_line: usize,
    /// Размер литерала `{n+}`, байт; больше — `NO (QUOTA/MAXSIZE)`.
    pub max_literal: usize,
    /// Одновременных соединений на слушатель; сверх лимита — `BYE (TRYLATER)` и закрытие.
    pub max_connections: usize,
    /// Неудачных попыток входа за сеанс; затем `BYE` и закрытие.
    pub max_auth_failures: u32,
    /// Таймаут бездействия сеанса.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub idle_timeout: Duration,
}

impl Default for ManageSieveConfig {
    fn default() -> Self {
        Self {
            listen: Vec::new(),
            max_line: 8192,
            max_literal: 256 * 1024,
            max_connections: 1024,
            max_auth_failures: 3,
            idle_timeout: Duration::from_secs(30 * 60),
        }
    }
}

/// Период автоответа `vacation` (RFC 5230 `:days`).
///
/// ```yaml
/// default_days: 7
/// min_days: 1
/// max_days: 30
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct VacationConfig {
    /// Период повторного ответа одному отправителю, дней, если скрипт не задал `:days`; он же — для автоответа из
    /// настройки учётной записи.
    pub default_days: u64,
    /// Наименьший период, дней: меньшее `:days` заменяется им.
    pub min_days: u64,
    /// Наибольший период, дней: большее `:days` заменяется им.
    pub max_days: u64,
}

impl Default for VacationConfig {
    fn default() -> Self {
        Self {
            default_days: 7,
            min_days: 1,
            max_days: 30,
        }
    }
}
