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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> Result<SieveConfig, String> {
        serde_yaml_ng::from_str(yaml).map_err(|e| e.to_string())
    }

    #[test]
    fn empty_section_is_default() {
        assert_eq!(parse("{}"), Ok(SieveConfig::default()));
        assert_eq!(
            parse("vacation: {}\ncache: {}\nmanagesieve: {}"),
            Ok(SieveConfig::default())
        );
    }

    #[test]
    fn all_fields() {
        let yaml = r#"
max_script_size: 65536
max_nesting: 8
max_list_len: 100
max_commands: 500
max_actions: 16
max_steps: 200000
max_redirects: 2
max_body_scan: 32768
max_received: 30
max_node_hops: 2
vacation:
  default_days: 3
  min_days: 2
  max_days: 14
cache:
  programs: 0
managesieve:
  listen:
    - listen: "[::]:4190"
    - listen: "127.0.0.1:4191"
      implicit_tls: true
  max_line: 4096
  max_literal: 131072
  idle_timeout: 1m30s
  max_connections: 64
  max_auth_failures: 5
"#;
        let c = parse(yaml).expect("секция");
        assert_eq!(
            c.limits(),
            Limits {
                max_script_size: 65536,
                max_nesting: 8,
                max_list_len: 100,
                max_commands: 500,
                max_actions: 16,
                max_steps: 200_000,
                max_redirects: 2,
                max_body_scan: 32768,
            }
        );
        assert_eq!((c.max_received, c.max_node_hops), (30, 2));
        assert_eq!(
            c.vacation,
            VacationConfig {
                default_days: 3,
                min_days: 2,
                max_days: 14,
            }
        );
        assert_eq!(c.cache, CacheConfig { programs: 0 });
        let m = &c.managesieve;
        let listen: Vec<(String, bool)> = m
            .listen
            .iter()
            .map(|e| (e.listen.to_string(), e.implicit_tls))
            .collect();
        assert_eq!(
            listen,
            [
                ("[::]:4190".to_owned(), false),
                ("127.0.0.1:4191".to_owned(), true)
            ]
        );
        assert_eq!(
            (
                m.max_line,
                m.max_literal,
                m.idle_timeout,
                m.max_connections,
                m.max_auth_failures
            ),
            (4096, 131_072, Duration::from_secs(90), 64, 5)
        );
    }

    /// Примеры из документации [`SieveConfig`], [`ManageSieveConfig`] и [`VacationConfig`].
    #[test]
    fn documented_examples() {
        let c = parse(
            "max_steps: 10000000\nmax_redirects: 4\nvacation:\n  default_days: 7\ncache:\n  programs: 4096\n\
             managesieve:\n  idle_timeout: 30m\n",
        )
        .expect("пример секции");
        assert_eq!((c.max_steps, c.max_redirects), (10_000_000, 4));
        assert_eq!(c.managesieve.idle_timeout, Duration::from_secs(30 * 60));

        let m: ManageSieveConfig =
            serde_yaml_ng::from_str("listen:\n  - listen: \"[::]:4190\"\nidle_timeout: 30m\n")
                .expect("пример managesieve");
        assert_eq!(m.listen.len(), 1);
        assert_eq!(
            serde_yaml_ng::from_str::<VacationConfig>(
                "default_days: 7\nmin_days: 1\nmax_days: 30\n"
            )
            .expect("пример vacation"),
            VacationConfig::default()
        );
    }

    #[test]
    fn unknown_field_is_named() {
        for (yaml, field) in [
            ("max_stepz: 1", "max_stepz"),
            ("vacation:\n  days: 7", "days"),
            ("cache:\n  size: 1", "size"),
            ("managesieve:\n  port: 4190", "port"),
            (
                "managesieve:\n  listen:\n    - listen: \"[::]:4190\"\n      tls: true",
                "tls",
            ),
        ] {
            let e = parse(yaml).expect_err(yaml);
            assert!(e.contains(&format!("`{field}`")), "{yaml}: {e}");
        }
    }

    #[test]
    fn bad_value_is_named() {
        for (yaml, field) in [
            (
                "managesieve:\n  idle_timeout: soon",
                "managesieve.idle_timeout",
            ),
            (
                "managesieve:\n  idle_timeout: 30",
                "managesieve.idle_timeout",
            ),
            ("managesieve:\n  max_line: -1", "managesieve.max_line"),
            ("max_steps: many", "max_steps"),
            ("vacation:\n  max_days: x", "vacation.max_days"),
        ] {
            let e = parse(yaml).expect_err(yaml);
            assert!(e.contains(field), "{yaml}: {e}");
        }
    }
}
