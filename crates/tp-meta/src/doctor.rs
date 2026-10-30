//! Проверки СУБД для `thepost doctor` (SRS 3.1): структурированный отчёт, вывод — забота приложения.

use std::time::Duration;

use sqlx::{FromRow, PgPool};
use tp_contracts::Result;

use crate::schema::db_err;

/// Пороги проверок СУБД (SRS 3.1, 5.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorConfig {
    /// Наименьшая версия сервера (`server_version_num`).
    pub min_server_version: u32,
    /// Требовать синхронную репликацию (Enterprise: RPO = 0).
    pub sync_replication: bool,
    /// Наименьшее число кандидатов в синхронные реплики.
    pub min_sync_candidates: u32,
    /// Наименьшее значение `max_connections`.
    pub min_max_connections: u32,
    /// Предел ожидания ответа СУБД на проверки.
    pub timeout: Duration,
}

impl Default for DoctorConfig {
    fn default() -> Self {
        Self {
            min_server_version: 160_000,
            sync_replication: false,
            min_sync_candidates: 2,
            min_max_connections: 100,
            timeout: Duration::from_secs(10),
        }
    }
}

/// Итог проверки; порядок — по тяжести.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    /// Не применяется в этой конфигурации.
    Skipped,
    /// Требование выполнено.
    Ok,
    /// Работать можно, но стоит исправить.
    Warn,
    /// Требование не выполнено.
    Fail,
}

/// Одна проверка.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Идентификатор: `server_version`, `sync_replication`, `max_connections`, `timezone`,
    /// `log_timezone`, `rls`.
    pub id: &'static str,
    /// Итог.
    pub status: Status,
    /// Фактическое значение параметра СУБД.
    pub value: String,
    /// Пояснение для администратора.
    pub message: String,
}

/// Отчёт проверок СУБД.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    /// Проверки в порядке выполнения.
    pub checks: Vec<Check>,
}

impl Report {
    /// Худший итог; пропущенные проверки не ухудшают его (пустой отчёт — `Ok`).
    pub fn status(&self) -> Status {
        self.checks
            .iter()
            .map(|c| c.status)
            .max()
            .map_or(Status::Ok, |s| s.max(Status::Ok))
    }

    /// Проверка по идентификатору.
    pub fn get(&self, id: &str) -> Option<&Check> {
        self.checks.iter().find(|c| c.id == id)
    }

    fn push(&mut self, id: &'static str, status: Status, value: impl Into<String>, message: &str) {
        self.checks.push(Check {
            id,
            status,
            value: value.into(),
            message: message.to_owned(),
        });
    }
}

#[derive(FromRow)]
struct Settings {
    version_num: i32,
    version: String,
    max_connections: i32,
    reserved: i32,
    sync_names: String,
    sync_commit: String,
    timezone: String,
    log_timezone: String,
    rolsuper: bool,
    rolbypassrls: bool,
}

/// Число кандидатов в синхронные реплики из `synchronous_standby_names`
/// (`FIRST n (a, b)`, `ANY n (a, b)`, `n (a, b)`, `a, b`); `None` — `*` (любая реплика).
pub fn sync_candidates(names: &str) -> Option<u32> {
    let list = match (names.find('('), names.rfind(')')) {
        (Some(l), Some(r)) if l < r => &names[l + 1..r],
        _ => names,
    };
    let mut n = 0u32;
    for name in list.split(',').map(|s| s.trim().trim_matches('"')) {
        if name == "*" {
            return None;
        }
        if !name.is_empty() {
            n += 1;
        }
    }
    Some(n)
}

/// Часовой пояс — UTC под любым из имён PostgreSQL.
pub fn is_utc(tz: &str) -> bool {
    let tz = tz.trim().to_ascii_lowercase();
    let tz = tz.strip_prefix("etc/").unwrap_or(&tz);
    matches!(
        tz,
        "utc" | "uct" | "gmt" | "gmt0" | "gmt+0" | "gmt-0" | "universal" | "zulu" | "greenwich"
    )
}

/// Проверяет СУБД пула. Ошибка — только недоступность СУБД; несоответствия — в отчёте.
pub async fn check(pool: &PgPool, cfg: &DoctorConfig) -> Result<Report> {
    let query = sqlx::query_as::<_, Settings>(
        "SELECT current_setting('server_version_num')::int AS version_num,
                current_setting('server_version') AS version,
                current_setting('max_connections')::int AS max_connections,
                current_setting('superuser_reserved_connections')::int AS reserved,
                current_setting('synchronous_standby_names') AS sync_names,
                current_setting('synchronous_commit') AS sync_commit,
                current_setting('TimeZone') AS timezone,
                current_setting('log_timezone') AS log_timezone,
                r.rolsuper, r.rolbypassrls
         FROM pg_roles r WHERE r.rolname = current_user",
    )
    .fetch_one(pool);
    let s = tokio::time::timeout(cfg.timeout, query)
        .await
        .map_err(|_| tp_contracts::Error::UNAVAILABLE)?
        .map_err(db_err)?;
    Ok(evaluate(&s, cfg, pool.options().get_max_connections()))
}

fn evaluate(s: &Settings, cfg: &DoctorConfig, pool_max: u32) -> Report {
    let mut r = Report::default();

    let version_ok = u32::try_from(s.version_num).is_ok_and(|v| v >= cfg.min_server_version);
    r.push(
        "server_version",
        if version_ok { Status::Ok } else { Status::Fail },
        &s.version,
        if version_ok {
            "версия PostgreSQL подходит"
        } else {
            "нужен PostgreSQL 16 или новее"
        },
    );

    let sync_value = format!(
        "synchronous_standby_names = '{}', synchronous_commit = {}",
        s.sync_names, s.sync_commit
    );
    let (status, message) = if !cfg.sync_replication {
        (
            Status::Skipped,
            "синхронная репликация не требуется (не Enterprise)",
        )
    } else if !matches!(s.sync_commit.as_str(), "on" | "remote_apply") {
        (
            Status::Fail,
            "synchronous_commit должен быть on или remote_apply (RPO = 0)",
        )
    } else {
        match sync_candidates(&s.sync_names) {
            Some(n) if n >= cfg.min_sync_candidates => {
                (Status::Ok, "синхронная репликация настроена")
            }
            Some(_) => (
                Status::Fail,
                "в synchronous_standby_names недостаточно кандидатов в синхронные реплики",
            ),
            None => (
                Status::Warn,
                "synchronous_standby_names = '*': число кандидатов не проверяется",
            ),
        }
    };
    r.push("sync_replication", status, sync_value, message);

    let available = u32::try_from(s.max_connections - s.reserved).unwrap_or(0);
    let max_ok = u32::try_from(s.max_connections).is_ok_and(|m| m >= cfg.min_max_connections);
    let (status, message) = if !max_ok {
        (Status::Fail, "max_connections меньше требуемого")
    } else if available < pool_max {
        (
            Status::Warn,
            "пул узла больше доступных соединений сервера: нужен PgBouncer или меньший пул",
        )
    } else {
        (Status::Ok, "соединений достаточно")
    };
    r.push(
        "max_connections",
        status,
        format!(
            "{} (резерв {}, пул узла {pool_max})",
            s.max_connections, s.reserved
        ),
        message,
    );

    let tz_ok = is_utc(&s.timezone);
    r.push(
        "timezone",
        if tz_ok { Status::Ok } else { Status::Fail },
        &s.timezone,
        if tz_ok {
            "часовой пояс сеанса — UTC"
        } else {
            "часовой пояс сеанса должен быть UTC"
        },
    );
    let log_ok = is_utc(&s.log_timezone);
    r.push(
        "log_timezone",
        if log_ok { Status::Ok } else { Status::Warn },
        &s.log_timezone,
        if log_ok {
            "журналы СУБД — в UTC"
        } else {
            "журналы СУБД не в UTC: время расходится с журналами The Post"
        },
    );

    let rls_ok = !s.rolsuper && !s.rolbypassrls;
    r.push(
        "rls",
        if rls_ok { Status::Ok } else { Status::Fail },
        format!("superuser = {}, bypassrls = {}", s.rolsuper, s.rolbypassrls),
        if rls_ok {
            "изоляция организаций (RLS) действует"
        } else {
            "роль приложения обходит RLS: нужна роль без SUPERUSER и BYPASSRLS"
        },
    );
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        Settings {
            version_num: 160_004,
            version: "16.4".into(),
            max_connections: 200,
            reserved: 3,
            sync_names: String::new(),
            sync_commit: "on".into(),
            timezone: "UTC".into(),
            log_timezone: "Etc/UTC".into(),
            rolsuper: false,
            rolbypassrls: false,
        }
    }

    #[test]
    fn candidates() {
        assert_eq!(sync_candidates(""), Some(0));
        assert_eq!(sync_candidates("a"), Some(1));
        assert_eq!(sync_candidates("a, b"), Some(2));
        assert_eq!(sync_candidates("FIRST 1 (a, \"b\", c)"), Some(3));
        assert_eq!(sync_candidates("ANY 1 (a,b)"), Some(2));
        assert_eq!(sync_candidates("2 (a)"), Some(1));
        assert_eq!(sync_candidates("ANY 1 (*)"), None);
    }

    #[test]
    fn utc_names() {
        for tz in ["UTC", "Etc/UTC", "GMT", "Etc/Universal", "Zulu"] {
            assert!(is_utc(tz), "{tz}");
        }
        assert!(!is_utc("Europe/Moscow"));
    }

    #[test]
    fn healthy_server() {
        let r = evaluate(&settings(), &DoctorConfig::default(), 32);
        assert_eq!(r.status(), Status::Ok);
        assert_eq!(r.checks.len(), 6);
        assert_eq!(
            r.get("sync_replication").map(|c| c.status),
            Some(Status::Skipped)
        );
    }

    #[test]
    fn enterprise_and_failures() {
        let cfg = DoctorConfig {
            sync_replication: true,
            ..DoctorConfig::default()
        };
        let mut s = settings();
        s.sync_names = "ANY 1 (a, b)".into();
        assert_eq!(evaluate(&s, &cfg, 32).status(), Status::Ok);

        s.sync_names = "a".into();
        s.version_num = 150_008;
        s.timezone = "Europe/Moscow".into();
        s.rolbypassrls = true;
        let r = evaluate(&s, &cfg, 500);
        for id in ["server_version", "sync_replication", "timezone", "rls"] {
            assert_eq!(r.get(id).map(|c| c.status), Some(Status::Fail), "{id}");
        }
        assert_eq!(
            r.get("max_connections").map(|c| c.status),
            Some(Status::Warn)
        );
        assert_eq!(r.status(), Status::Fail);

        s.sync_names = "ANY 1 (a, b)".into();
        s.sync_commit = "local".into();
        let r = evaluate(&s, &cfg, 32);
        assert_eq!(
            r.get("sync_replication").map(|c| c.status),
            Some(Status::Fail)
        );
    }
}
