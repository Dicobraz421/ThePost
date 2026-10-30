//! `thepost doctor [--json]` (SRS 5.5, 3.1; APP-02): проверки окружения узла — конфигурация, хранилище секретов,
//! СУБД (проверки `tp-meta`), BLOB (запись, чтение и удаление пробного объекта), синхронизация часов узла и СУБД.
//!
//! Вывод — таблица или JSON; код возврата ≠ 0, если хотя бы одна проверка не пройдена (`fail`).

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::TryStreamExt;
use serde::Serialize;
use sqlx::PgPool;
use tp_contracts::blob::BlobStore;
use tp_contracts::secrets::SecretStore;
use tp_contracts::{Error, Result};
use tp_meta::MetaConfig;
use tp_platform::Config;

use crate::infra;
use crate::secrets;
use crate::sections::{self, DOCTOR, DoctorSection};

/// Итог проверки; порядок — по тяжести.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// Не применяется.
    Skipped,
    /// Требование выполнено.
    Ok,
    /// Работать можно, но стоит исправить.
    Warn,
    /// Требование не выполнено: код возврата ≠ 0.
    Fail,
}

impl Status {
    /// Обозначение в таблице.
    pub fn label(self) -> &'static str {
        match self {
            Status::Skipped => "SKIP",
            Status::Ok => "OK",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }
}

impl From<tp_meta::doctor::Status> for Status {
    fn from(s: tp_meta::doctor::Status) -> Self {
        match s {
            tp_meta::doctor::Status::Skipped => Status::Skipped,
            tp_meta::doctor::Status::Ok => Status::Ok,
            tp_meta::doctor::Status::Warn => Status::Warn,
            tp_meta::doctor::Status::Fail => Status::Fail,
        }
    }
}

/// Одна проверка.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Check {
    /// Идентификатор `<группа>.<проверка>`: `config.load`, `database.rls`, `blob.write`, `time.clock_skew` …
    pub id: String,
    /// Итог.
    pub status: Status,
    /// Фактическое значение.
    pub value: String,
    /// Пояснение для администратора.
    pub message: String,
}

/// Отчёт `thepost doctor`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    /// Худший итог.
    pub status: Option<Status>,
    /// Проверки в порядке выполнения.
    pub checks: Vec<Check>,
}

impl Report {
    fn push(
        &mut self,
        id: &str,
        status: Status,
        value: impl Into<String>,
        message: impl Into<String>,
    ) {
        self.checks.push(Check {
            id: id.to_owned(),
            status,
            value: value.into(),
            message: message.into(),
        });
        self.status = Some(self.worst());
    }

    /// Худший итог; пропущенные проверки его не ухудшают.
    pub fn worst(&self) -> Status {
        self.checks
            .iter()
            .map(|c| c.status)
            .max()
            .map_or(Status::Ok, |s| s.max(Status::Ok))
    }

    /// Код возврата: 1 — есть непройденная проверка.
    pub fn exit_code(&self) -> i32 {
        i32::from(self.worst() == Status::Fail)
    }

    /// Проверка по идентификатору.
    pub fn get(&self, id: &str) -> Option<&Check> {
        self.checks.iter().find(|c| c.id == id)
    }

    /// JSON (`--json`).
    pub fn to_json(&self) -> Result<String> {
        serde_json::to_string_pretty(self).map_err(|e| Error::internal(e.to_string()))
    }

    /// Таблица для терминала.
    pub fn to_table(&self) -> String {
        let width = |f: fn(&Check) -> usize, title: &str| {
            self.checks
                .iter()
                .map(f)
                .max()
                .unwrap_or(0)
                .max(title.chars().count())
        };
        let w_id = width(|c| c.id.chars().count(), "ПРОВЕРКА");
        let w_value = width(|c| c.value.chars().count(), "ЗНАЧЕНИЕ");
        let pad =
            |s: &str, w: usize| format!("{s}{}", " ".repeat(w.saturating_sub(s.chars().count())));
        let mut out = format!(
            "{}  {}  {}  ПОЯСНЕНИЕ\n",
            pad("ИТОГ", 4),
            pad("ПРОВЕРКА", w_id),
            pad("ЗНАЧЕНИЕ", w_value)
        );
        for c in &self.checks {
            out.push_str(&format!(
                "{}  {}  {}  {}\n",
                pad(c.status.label(), 4),
                pad(&c.id, w_id),
                pad(&c.value, w_value),
                c.message
            ));
        }
        out.push_str(&format!("Итог: {}\n", self.worst().label()));
        out
    }
}

/// Сообщение ошибки без префикса варианта.
fn message(e: &Error) -> String {
    match e {
        Error::Invalid(m) | Error::Internal(m) => m.clone(),
        other => other.to_string(),
    }
}

/// Проверки окружения по config.yaml `path`; `store` — хранилище секретов (`None` — не настроено).
pub async fn run(path: &Path, store: Option<Arc<dyn SecretStore>>) -> Report {
    let mut report = Report::default();
    let loaded = sections::registry().and_then(|r| r.load(path));
    let cfg = match loaded {
        Ok(cfg) => cfg,
        Err(e) => {
            report.push(
                "config.load",
                Status::Fail,
                path.display().to_string(),
                message(&e),
            );
            return report;
        }
    };
    report.push(
        "config.load",
        Status::Ok,
        path.display().to_string(),
        "конфигурация разобрана",
    );
    for w in cfg.warnings() {
        report.push("config.unknown", Status::Warn, "", w.clone());
    }
    check(&cfg, store, &mut report).await;
    report
}

/// Проверки по разобранной конфигурации.
pub async fn check(cfg: &Config, store: Option<Arc<dyn SecretStore>>, report: &mut Report) {
    let doctor = cfg
        .section::<DoctorSection>(DOCTOR)
        .map(|d| (*d).clone())
        .unwrap_or_default();
    let refs = secrets::references(cfg);
    let store = match secrets::store(cfg, store) {
        Ok(store) => {
            let (status, msg) = if refs.is_empty() {
                (Status::Skipped, "ссылок на секреты нет".to_owned())
            } else {
                (Status::Ok, format!("ссылок на секреты: {}", refs.len()))
            };
            report.push("secrets.store", status, "", msg);
            Some(store)
        }
        Err(e) => {
            report.push("secrets.store", Status::Fail, "", message(&e));
            None
        }
    };

    match &store {
        Some(store) => database(cfg, store.as_ref(), &doctor, report).await,
        None => report.push(
            "database.connect",
            Status::Skipped,
            "",
            "хранилище секретов не настроено",
        ),
    }
    match &store {
        Some(store) => match infra::open_blob(cfg, store.as_ref()).await {
            Ok(blob) => {
                report.push("blob.open", Status::Ok, blob_kind(cfg), "хранилище открыто");
                probe_blob(blob.as_ref(), &doctor, report).await;
            }
            Err(e) => report.push("blob.open", Status::Fail, blob_kind(cfg), message(&e)),
        },
        None => report.push(
            "blob.open",
            Status::Skipped,
            "",
            "хранилище секретов не настроено",
        ),
    }
}

fn blob_kind(cfg: &Config) -> String {
    match cfg
        .section::<sections::BlobSection>(sections::BLOB)
        .and_then(|b| b.location())
    {
        Ok(sections::BlobLocation::Local(root)) => format!("local {}", root.display()),
        Ok(sections::BlobLocation::S3 { bucket, .. }) => format!("s3 {bucket}"),
        Err(_) => String::new(),
    }
}

/// СУБД: подключение, проверки `tp-meta` (версия, репликация, часовой пояс, RLS, TLS), часы.
async fn database(
    cfg: &Config,
    store: &dyn SecretStore,
    doctor: &DoctorSection,
    report: &mut Report,
) {
    let meta = cfg
        .section::<MetaConfig>(MetaConfig::SECTION)
        .map(|m| m.doctor.clone())
        .unwrap_or_default();
    let pool = match tokio::time::timeout(meta.timeout, infra::connect_db(cfg, store)).await {
        Ok(Ok(pool)) => pool,
        Ok(Err(e)) => {
            report.push("database.connect", Status::Fail, "", message(&e));
            return;
        }
        Err(_) => {
            report.push(
                "database.connect",
                Status::Fail,
                "",
                format!(
                    "нет ответа за {}",
                    tp_contracts::config::duration::format(meta.timeout)
                ),
            );
            return;
        }
    };
    report.push("database.connect", Status::Ok, "", "соединение установлено");
    match tp_meta::doctor::check(&pool, &meta).await {
        Ok(db) => {
            for c in db.checks {
                report.push(
                    &format!("database.{}", c.id),
                    c.status.into(),
                    c.value,
                    c.message,
                );
            }
        }
        Err(e) => report.push("database.checks", Status::Fail, "", message(&e)),
    }
    clock_skew(&pool, meta.timeout, doctor, report).await;
    pool.close().await;
}

/// Расхождение часов узла и СУБД: момент СУБД сравнивается с серединой интервала запроса.
async fn clock_skew(pool: &PgPool, timeout: Duration, doctor: &DoctorSection, report: &mut Report) {
    let id = "time.clock_skew";
    let before = unix_seconds(SystemTime::now());
    let query =
        sqlx::query_scalar::<_, f64>("SELECT extract(epoch FROM clock_timestamp())::float8")
            .fetch_one(pool);
    let db = match tokio::time::timeout(timeout, query).await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            report.push(id, Status::Fail, "", format!("время СУБД не получено: {e}"));
            return;
        }
        Err(_) => {
            report.push(id, Status::Fail, "", "время СУБД не получено: нет ответа");
            return;
        }
    };
    let after = unix_seconds(SystemTime::now());
    let skew = (db - (before + after) / 2.0).abs();
    let (status, msg) = skew_status(Duration::from_secs_f64(skew.min(1e9)), doctor);
    report.push(id, status, format!("{:.3}s", skew), msg);
}

/// Итог по расхождению часов и порогам секции `doctor`.
pub fn skew_status(skew: Duration, doctor: &DoctorSection) -> (Status, String) {
    if skew >= doctor.max_clock_skew {
        (
            Status::Fail,
            "часы узла и СУБД расходятся: настройте синхронизацию времени (NTP/chrony) на узлах и сервере СУБД".to_owned(),
        )
    } else if skew >= doctor.warn_clock_skew {
        (
            Status::Warn,
            "часы узла и СУБД заметно расходятся: проверьте синхронизацию времени".to_owned(),
        )
    } else {
        (Status::Ok, "часы синхронизированы".to_owned())
    }
}

fn unix_seconds(t: SystemTime) -> f64 {
    t.duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Пробный объект: запись, чтение со сверкой содержимого, удаление.
pub async fn probe_blob(store: &dyn BlobStore, doctor: &DoctorSection, report: &mut Report) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let path = format!("doctor/probe-{}-{nanos}", std::process::id());
    let data = Bytes::from(format!("thepost doctor {nanos}"));
    let limit = doctor.blob_timeout;
    let timed = |what: &'static str| {
        move |r: std::result::Result<Result<()>, tokio::time::error::Elapsed>| match r {
            Ok(r) => r,
            Err(_) => Err(Error::Invalid(format!("{what}: нет ответа"))),
        }
    };

    let write = tokio::time::timeout(limit, store.put_raw(&path, data.clone())).await;
    if let Err(e) = timed("запись")(write) {
        report.push("blob.write", Status::Fail, &path, message(&e));
        return;
    }
    report.push("blob.write", Status::Ok, &path, "пробный объект записан");

    let read = tokio::time::timeout(limit, async {
        let stream = store.get_raw(&path, None).await?;
        let chunks: Vec<Bytes> = stream.try_collect().await?;
        if chunks.concat() == data.as_ref() {
            Ok(())
        } else {
            Err(Error::Invalid("прочитано не то, что записано".to_owned()))
        }
    })
    .await;
    match timed("чтение")(read) {
        Ok(()) => report.push("blob.read", Status::Ok, &path, "содержимое совпадает"),
        Err(e) => report.push("blob.read", Status::Fail, &path, message(&e)),
    }

    let delete = tokio::time::timeout(limit, store.delete_raw(&path)).await;
    match timed("удаление")(delete) {
        Ok(()) => report.push("blob.delete", Status::Ok, &path, "пробный объект удалён"),
        Err(e) => report.push("blob.delete", Status::Fail, &path, message(&e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_testkit::{BlobOp, MemBlobStore};

    #[tokio::test]
    async fn blob_probe_ok_and_failures() {
        let store = MemBlobStore::default();
        let mut report = Report::default();
        probe_blob(&store, &DoctorSection::default(), &mut report).await;
        for id in ["blob.write", "blob.read", "blob.delete"] {
            assert_eq!(report.get(id).map(|c| c.status), Some(Status::Ok), "{id}");
        }
        assert!(store.paths().is_empty(), "пробный объект удалён");
        assert_eq!(report.exit_code(), 0);

        store.fail_next(BlobOp::PutRaw, Error::UNAVAILABLE);
        let mut report = Report::default();
        probe_blob(&store, &DoctorSection::default(), &mut report).await;
        assert_eq!(
            report.get("blob.write").map(|c| c.status),
            Some(Status::Fail)
        );
        assert!(report.get("blob.read").is_none());
        assert_eq!(report.exit_code(), 1);
    }

    #[test]
    fn clock_skew_thresholds() {
        let d = DoctorSection::default();
        assert_eq!(skew_status(Duration::from_millis(10), &d).0, Status::Ok);
        assert_eq!(skew_status(Duration::from_secs(1), &d).0, Status::Warn);
        assert_eq!(skew_status(Duration::from_secs(60), &d).0, Status::Fail);
    }

    fn write_config(dir: &Path, yaml: &str) -> std::path::PathBuf {
        let path = dir.join("config.yaml");
        std::fs::write(&path, yaml).expect("write");
        path
    }

    #[tokio::test]
    async fn bad_config_fails_with_exit_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(dir.path(), "domain: example.org\n");
        let report = run(&path, None).await;
        assert_eq!(
            report.get("config.load").map(|c| c.status),
            Some(Status::Fail)
        );
        assert_eq!(report.exit_code(), 1);
        let json = report.to_json().expect("json");
        assert!(json.contains("\"status\": \"fail\""), "{json}");
        assert!(report.to_table().contains("FAIL"));
    }

    #[tokio::test]
    async fn secrets_without_store_fail() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blob = dir.path().join("blob");
        let path = write_config(
            dir.path(),
            &format!(
                "domain: example.org\nprofile: dev\ndatabase:\n  url: postgres://thepost@127.0.0.1:1/thepost?sslmode=disable\n  password: {{ secret: \"thepost/db#password\" }}\n  tls: {{ allow_plaintext: true }}\nblob:\n  url: file://{}\n",
                blob.display().to_string().replace('\\', "/")
            ),
        );
        let report = run(&path, None).await;
        assert_eq!(
            report.get("secrets.store").map(|c| c.status),
            Some(Status::Fail)
        );
        assert_eq!(
            report.get("database.connect").map(|c| c.status),
            Some(Status::Skipped)
        );
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test]
    async fn local_blob_and_unreachable_database() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blob = dir.path().join("blob");
        let path = write_config(
            dir.path(),
            &format!(
                "domain: example.org\nprofile: dev\ndatabase:\n  url: postgres://thepost@127.0.0.1:1/thepost?sslmode=disable\n  acquire_timeout: 1s\n  tls: {{ allow_plaintext: true }}\nmeta:\n  doctor:\n    timeout: 2s\nblob:\n  url: file://{}\n",
                blob.display().to_string().replace('\\', "/")
            ),
        );
        let report = run(&path, None).await;
        assert_eq!(
            report.get("database.connect").map(|c| c.status),
            Some(Status::Fail)
        );
        for id in ["blob.open", "blob.write", "blob.read", "blob.delete"] {
            assert_eq!(
                report.get(id).map(|c| c.status),
                Some(Status::Ok),
                "{id}: {report:?}"
            );
        }
        assert_eq!(report.exit_code(), 1);
    }

    #[tokio::test]
    async fn database_checks_with_test_database() {
        let Some(url) = tp_testkit::pg_url() else {
            eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let blob = dir.path().join("blob");
        let path = write_config(
            dir.path(),
            &format!(
                "domain: example.org\nprofile: dev\ndatabase:\n  url: \"{}\"\n  tls: {{ allow_plaintext: true }}\nblob:\n  url: file://{}\n",
                crate::testutil::dev_url(&url),
                blob.display().to_string().replace('\\', "/")
            ),
        );
        let report = run(&path, None).await;
        assert_eq!(
            report.get("database.connect").map(|c| c.status),
            Some(Status::Ok),
            "{report:?}"
        );
        assert!(
            report.get("database.server_version").is_some(),
            "{report:?}"
        );
        assert!(report.get("time.clock_skew").is_some(), "{report:?}");
    }
}
