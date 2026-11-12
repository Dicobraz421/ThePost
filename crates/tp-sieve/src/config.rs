//! Секция `sieve` config.yaml (SRS 5.5): автоответ; лимиты, кэш и ManageSieve — SIEVE-07.

use schemars::JsonSchema;
use serde::Deserialize;

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
