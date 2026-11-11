//! `thepost doctor [--json]` (SRS 5.5, 3.1; APP-02): проверки окружения узла — конфигурация, хранилище секретов,
//! СУБД (проверки `tp-meta`: версия, синхронная реплика, часовой пояс, RLS, TLS, роль узла; кодировка и LC_CTYPE —
//! `database.locale`, APP-24; сторонние расширения — `database.extensions`, APP-08), BLOB (запись, чтение и удаление
//! пробного объекта), синхронизация часов узла и СУБД,
//! минимальные привилегии процесса (PLT-16): учётная запись, capabilities, no_new_privs, seccomp, Landlock;
//! DNS почтового домена (MX, SPF, DKIM, DMARC, MTA-STS, PTR), сертификаты и PROXY v2 — `doctor_dns` (APP-06);
//! сроки внутренних сертификатов (CA СУБД и OpenBao, сертификат узла от них) — `pki.*` (APP-25).
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

use crate::doctor_dns::{self, Dns};
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

impl From<tp_platform::secrets::DoctorStatus> for Status {
    fn from(s: tp_platform::secrets::DoctorStatus) -> Self {
        use tp_platform::secrets::DoctorStatus as S;
        match s {
            S::Skipped => Status::Skipped,
            S::Ok => Status::Ok,
            S::Warn => Status::Warn,
            S::Fail => Status::Fail,
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
    /// Идентификатор `<группа>.<проверка>`: `config.load`, `secrets.store`, `database.rls`, `blob.write`,
    /// `time.clock_skew` …
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
    pub(crate) fn push(
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
    run_with(path, store, None).await
}

/// То же с резолвером DNS `dns` (`None` — системный).
pub async fn run_with(
    path: &Path,
    store: Option<Arc<dyn SecretStore>>,
    dns: Option<&dyn Dns>,
) -> Report {
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
    let text = tokio::fs::read_to_string(path).await.unwrap_or_default();
    let confine = crate::startup::has_section(&text, tp_platform::privileges::CONFINE);
    check(&cfg, confine, store, dns, &mut report).await;
    report
}

/// Проверки по разобранной конфигурации; `confine` — в файле есть секция `confine` (нет — seccomp в режиме `log`,
/// как при запуске `serve`); `dns` — резолвер (`None` — системный).
pub async fn check(
    cfg: &Config,
    confine: bool,
    store: Option<Arc<dyn SecretStore>>,
    dns: Option<&dyn Dns>,
    report: &mut Report,
) {
    let doctor = cfg
        .section::<DoctorSection>(DOCTOR)
        .map(|d| (*d).clone())
        .unwrap_or_default();
    // Хранилище секретов (PLT-12): `secrets_store` → `secrets.store` и т. д.
    for c in secrets::doctor(cfg).await.checks {
        let id = c.id.replacen('_', ".", 1);
        report.push(&id, c.status.into(), c.value, c.message);
    }
    // Привилегии процесса (PLT-16): `privileges_user` → `privileges.user` и т. д.
    for mut c in tp_platform::privileges::doctor(cfg).await.checks {
        if !confine {
            seccomp_as_serve(&mut c);
        }
        let id = c.id.replacen('_', ".", 1);
        report.push(&id, c.status.into(), c.value, c.message);
    }
    let refs = secrets::references(cfg);
    let store = match secrets::store(cfg, store).await {
        Ok(store) => {
            let (status, msg) = if refs.is_empty() {
                (Status::Skipped, "ссылок на секреты нет".to_owned())
            } else {
                (Status::Ok, format!("ссылок на секреты: {}", refs.len()))
            };
            report.push("secrets.refs", status, "", msg);
            Some(store)
        }
        Err(e) => {
            report.push("secrets.refs", Status::Fail, "", message(&e));
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
    doctor_dns::check_with(cfg, dns, &doctor, report).await;
    internal_pki(cfg, SystemTime::now(), report).await;
}

/// Режим seccomp без секции `confine` — `log`, как при запуске `serve` (`startup::confine`): проверка
/// `privileges_seccomp` по умолчанию секции (`enforce`) так не показывала бы итог запуска.
fn seccomp_as_serve(c: &mut tp_platform::secrets::DoctorCheck) {
    use tp_platform::secrets::DoctorStatus;
    if c.id != "privileges_seccomp" || c.value != "enforce" {
        return;
    }
    c.value = "log".to_owned();
    if c.status == DoctorStatus::Ok {
        c.status = DoctorStatus::Warn;
        c.message = "секции confine нет: режим журнала, как при запуске — вызовы вне профиля не блокируются \
                     (confine.seccomp: enforce — блокировать)"
            .to_owned();
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
    locale(&pool, meta.timeout, report).await;
    extensions(&pool, meta.timeout, report).await;
    clock_skew(&pool, meta.timeout, doctor, report).await;
    pool.close().await;
}

/// Кодировка и `LC_CTYPE` базы узла (решение 2026-11-11 №23): `lower(login)` переводит кириллицу только в UTF8 с
/// `LC_CTYPE` UTF-8.
async fn locale(pool: &PgPool, timeout: Duration, report: &mut Report) {
    let id = "database.locale";
    let query = sqlx::query_as::<_, (String, String)>(
        "SELECT pg_encoding_to_char(encoding)::text, datctype::text FROM pg_database \
         WHERE datname = current_database()",
    )
    .fetch_one(pool);
    let (encoding, ctype) = match tokio::time::timeout(timeout, query).await {
        Ok(Ok(row)) => row,
        Ok(Err(e)) => {
            report.push(
                id,
                Status::Fail,
                "",
                format!("кодировка базы не получена: {e}"),
            );
            return;
        }
        Err(_) => {
            report.push(
                id,
                Status::Fail,
                "",
                "кодировка базы не получена: нет ответа",
            );
            return;
        }
    };
    let (status, msg) = locale_status(&encoding, &ctype);
    report.push(id, status, format!("{encoding}, {ctype}"), msg);
}

/// Итог по кодировке (`pg_database.encoding`) и `LC_CTYPE` (`datctype`) базы: нужны UTF8 и `*.UTF-8`/`*.utf8`.
pub fn locale_status(encoding: &str, ctype: &str) -> (Status, &'static str) {
    let ctype = ctype.to_ascii_lowercase();
    if encoding.eq_ignore_ascii_case("UTF8")
        && (ctype.ends_with(".utf-8") || ctype.ends_with(".utf8"))
    {
        (Status::Ok, "кодировка UTF8, LC_CTYPE UTF-8")
    } else {
        (
            Status::Fail,
            "база не в UTF8 с LC_CTYPE UTF-8 (C.UTF-8 или <язык>.UTF-8): lower() не переводит кириллицу в именах входа \
             в нижний регистр — ломаются регистронезависимая уникальность и поиск по префиксу; создайте базу \
             с ENCODING 'UTF8' LC_CTYPE 'C.UTF-8' TEMPLATE template0 — docs/PREREQUISITES.md",
        )
    }
}

/// Расширения PostgreSQL базы узла, кроме встроенного `plpgsql` (по имени, по возрастанию).
async fn extensions(pool: &PgPool, timeout: Duration, report: &mut Report) {
    let id = "database.extensions";
    let query = sqlx::query_scalar::<_, String>(
        "SELECT extname::text FROM pg_extension WHERE extname <> 'plpgsql' ORDER BY 1",
    )
    .fetch_all(pool);
    let names = match tokio::time::timeout(timeout, query).await {
        Ok(Ok(names)) => names,
        Ok(Err(e)) => {
            report.push(
                id,
                Status::Fail,
                "",
                format!("список расширений не получен: {e}"),
            );
            return;
        }
        Err(_) => {
            report.push(
                id,
                Status::Fail,
                "",
                "список расширений не получен: нет ответа",
            );
            return;
        }
    };
    let (status, msg) = extensions_status(&names);
    report.push(id, status, names.join(", "), msg);
}

/// Итог по списку сторонних расширений: продукт работает без расширений (SRS 3.1, решение
/// 2026-11-01-interaction-db-ha) — установленные не нужны и не должны требоваться механизмом HA.
pub fn extensions_status(names: &[String]) -> (Status, &'static str) {
    if names.is_empty() {
        (Status::Ok, "сторонних расширений нет")
    } else {
        (
            Status::Warn,
            "установлены сторонние расширения: продукт их не использует; механизм HA и перенос базы не должны от них зависеть",
        )
    }
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

/// Порог предупреждения о сроке внутренних сертификатов (решение 2026-11-11 №27): плейбук перевыпускает
/// сертификат за 90 сут. (DEP-18), предупреждение за 60 — прогона не было.
pub const PKI_WARN_BEFORE: Duration = Duration::from_secs(60 * 86_400);

/// Как обновить сертификат узла от внутреннего CA.
const PKI_RENEW_CERT: &str = "перевыпуск — повторный прогон плейбука (DEP-18)";
/// Как обновить сам CA.
const PKI_RENEW_CA: &str = "новый CA — удалить каталог роли thepost_pki (/etc/thepost/pki) и повторно прогнать \
                            плейбук; клиентам — новый ca.pem";

/// Сертификаты PEM-файла: ближайшее окончание срока, субъекты (DER) и издатель первого сертификата (DER).
struct PemCerts {
    not_after: SystemTime,
    subjects: Vec<Vec<u8>>,
    issuer: Vec<u8>,
}

fn pem_certs(pem: &[u8]) -> std::result::Result<PemCerts, String> {
    let mut out: Option<PemCerts> = None;
    for block in x509_parser::pem::Pem::iter_from_buffer(pem) {
        let block = block.map_err(|e| e.to_string())?;
        if block.label != "CERTIFICATE" {
            continue;
        }
        let cert = block.parse_x509().map_err(|e| e.to_string())?;
        let secs = cert.validity().not_after.timestamp();
        let not_after = UNIX_EPOCH + Duration::from_secs(u64::try_from(secs).unwrap_or(0));
        let subject = cert.subject().as_raw().to_vec();
        match &mut out {
            Some(c) => {
                c.not_after = c.not_after.min(not_after);
                c.subjects.push(subject);
            }
            None => {
                out = Some(PemCerts {
                    not_after,
                    subjects: vec![subject],
                    issuer: cert.issuer().as_raw().to_vec(),
                });
            }
        }
    }
    out.ok_or_else(|| "сертификатов PEM нет".to_owned())
}

/// Системный набор CA: сроки публичных корней — забота ОС, не плейбука.
fn not_system_bundle(
    path: std::path::PathBuf,
) -> std::result::Result<std::path::PathBuf, &'static str> {
    if path == tp_platform::secrets::SecretsTlsConfig::default().ca_file {
        Err("системный набор CA")
    } else {
        Ok(path)
    }
}

/// CA для проверки СУБД: `database.tls.ca_file`, иначе `sslrootcert` адреса `database.url` / `database.urls`.
fn database_ca(cfg: &Config) -> std::result::Result<std::path::PathBuf, &'static str> {
    const NONE: &str = "CA не задан: системные корни";
    let db = cfg
        .section::<tp_meta::pool::PoolConfig>(tp_meta::pool::PoolConfig::SECTION)
        .map_err(|_| NONE)?;
    let from_url = || {
        std::iter::once(&db.url).chain(&db.urls).find_map(|url| {
            let (_, query) = url.split_once('?')?;
            let parsed = reqwest::Url::parse(&format!("http://h/?{query}")).ok()?;
            parsed
                .query_pairs()
                .find(|(k, v)| k == "sslrootcert" && !v.is_empty())
                .map(|(_, v)| std::path::PathBuf::from(v.into_owned()))
        })
    };
    not_system_bundle(db.tls.ca_file.clone().or_else(from_url).ok_or(NONE)?)
}

/// CA для проверки OpenBao: `secrets.tls.ca_file`, если хранилище настроено.
fn secrets_ca(cfg: &Config) -> std::result::Result<std::path::PathBuf, &'static str> {
    const NONE: &str = "OpenBao не настроен";
    let s = cfg
        .section::<tp_platform::secrets::SecretsConfig>(secrets::SECRETS)
        .map_err(|_| NONE)?;
    if s.url.is_none() {
        return Err(NONE);
    }
    not_system_bundle(s.tls.ca_file.clone())
}

/// Итог по сроку `not_after` с порогом [`PKI_WARN_BEFORE`]; `renew` — как обновить.
fn pki_expiry(not_after: SystemTime, now: SystemTime, renew: &str) -> (Status, String) {
    let days = not_after.duration_since(now).unwrap_or_default().as_secs() / 86_400;
    match doctor_dns::expiry_status(not_after, now, PKI_WARN_BEFORE) {
        Status::Fail => (Status::Fail, format!("срок действия истёк: {renew}")),
        Status::Warn => (Status::Warn, format!("истекает через {days} сут.: {renew}")),
        s => (s, format!("действителен ещё {days} сут.")),
    }
}

/// Срок CA из файла `path` (проверка `id`); субъекты CA — для сопоставления с издателем сертификата узла.
async fn pki_ca(id: &str, path: &Path, now: SystemTime, report: &mut Report) -> Vec<Vec<u8>> {
    let shown = path.display().to_string();
    let certs = match tokio::fs::read(path).await {
        Ok(pem) => pem_certs(&pem),
        Err(e) => Err(format!("файл не прочитан: {e}")),
    };
    match certs {
        Ok(c) => {
            let (status, msg) = pki_expiry(c.not_after, now, PKI_RENEW_CA);
            report.push(id, status, shown, msg);
            c.subjects
        }
        Err(e) => {
            report.push(id, Status::Warn, shown, e);
            Vec::new()
        }
    }
}

/// Сроки внутренних сертификатов (решение 2026-11-11 №27, APP-25): CA СУБД (`pki.database_ca`), CA OpenBao
/// (`pki.secrets_ca`) и сертификат узла `tls.cert_file`, если его выпустил один из этих CA (`pki.node`;
/// сертификат другого издателя — проверка `tls.certificate`). `WARN` — за [`PKI_WARN_BEFORE`], `FAIL` — после срока.
pub async fn internal_pki(cfg: &Config, now: SystemTime, report: &mut Report) {
    let mut issuers = Vec::new();
    match database_ca(cfg) {
        Ok(path) => issuers.extend(pki_ca("pki.database_ca", &path, now, report).await),
        Err(why) => report.push("pki.database_ca", Status::Skipped, "", why),
    }
    match secrets_ca(cfg) {
        Ok(path) => issuers.extend(pki_ca("pki.secrets_ca", &path, now, report).await),
        Err(why) => report.push("pki.secrets_ca", Status::Skipped, "", why),
    }
    let id = "pki.node";
    let file = cfg
        .section::<tp_platform::TlsConfig>("tls")
        .map(|t| t.cert_file.clone())
        .unwrap_or_default();
    let shown = file.display().to_string();
    if issuers.is_empty() {
        report.push(id, Status::Skipped, shown, "внутреннего CA нет");
        return;
    }
    let node = match tokio::fs::read(&file).await {
        Ok(pem) => pem_certs(&pem),
        Err(e) => Err(format!("файл не прочитан: {e}")),
    };
    match node {
        Ok(c) if issuers.contains(&c.issuer) => {
            let (status, msg) = pki_expiry(c.not_after, now, PKI_RENEW_CERT);
            report.push(id, status, shown, msg);
        }
        Ok(_) => report.push(
            id,
            Status::Skipped,
            shown,
            "выпущен не внутренним CA: срок — проверка tls.certificate",
        ),
        Err(e) => report.push(
            id,
            Status::Skipped,
            shown,
            format!("{e}: см. tls.certificate"),
        ),
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

    /// Без секции `confine` проверка seccomp показывает режим запуска `serve` — `log`.
    #[test]
    fn seccomp_without_confine_section_is_log() {
        use tp_platform::secrets::{DoctorCheck, DoctorStatus};
        let check = |id: &'static str, status, value: &str| DoctorCheck {
            id,
            status,
            value: value.to_owned(),
            message: "m".to_owned(),
        };
        let mut ok = check("privileges_seccomp", DoctorStatus::Ok, "enforce");
        seccomp_as_serve(&mut ok);
        assert_eq!((ok.status, ok.value.as_str()), (DoctorStatus::Warn, "log"));
        assert!(ok.message.contains("confine"), "{}", ok.message);
        let mut no_kernel = check("privileges_seccomp", DoctorStatus::Fail, "enforce");
        seccomp_as_serve(&mut no_kernel);
        assert_eq!(
            (
                no_kernel.status,
                no_kernel.value.as_str(),
                no_kernel.message.as_str()
            ),
            (DoctorStatus::Fail, "log", "m")
        );
        for mut same in [
            check("privileges_seccomp", DoctorStatus::Warn, "off"),
            check("privileges_landlock", DoctorStatus::Ok, "enforce"),
        ] {
            let before = same.clone();
            seccomp_as_serve(&mut same);
            assert_eq!(same, before);
        }
    }

    /// `thepost doctor` без секции `confine` и с ней: режим seccomp — как у `serve` (Linux).
    #[tokio::test]
    async fn doctor_seccomp_mode_follows_serve() {
        if !Path::new("/proc/self/status").exists() {
            return;
        }
        let dir = tempfile::tempdir().expect("tempdir");
        let base = "domain: example.org\nprofile: dev\ndatabase:\n  url: postgres://thepost@127.0.0.1:1/thepost?sslmode=disable\n  acquire_timeout: 1s\n  tls: { allow_plaintext: true }\nblob:\n  url: file:///nonexistent/blob\n";
        let dns = crate::doctor_dns::MockDns::default();
        for (extra, mode) in [("", "log"), ("confine:\n  seccomp: enforce\n", "enforce")] {
            let path = write_config(dir.path(), &format!("{base}{extra}"));
            let report = run_with(&path, None, Some(&dns)).await;
            assert_eq!(
                report.get("privileges.seccomp").map(|c| c.value.as_str()),
                Some(mode),
                "{report:?}"
            );
        }
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
        let report = run_with(&path, None, Some(&crate::doctor_dns::MockDns::default())).await;
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
        let report = run_with(&path, None, Some(&crate::doctor_dns::MockDns::default())).await;
        assert_eq!(
            report.get("secrets.refs").map(|c| c.status),
            Some(Status::Fail)
        );
        assert_eq!(
            report.get("secrets.config").map(|c| c.status),
            Some(Status::Skipped),
            "{report:?}"
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
        let report = run_with(&path, None, Some(&crate::doctor_dns::MockDns::default())).await;
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
        let report = run_with(&path, None, Some(&crate::doctor_dns::MockDns::default())).await;
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
        assert!(report.get("database.extensions").is_some(), "{report:?}");
        assert_eq!(
            report.get("database.locale").map(|c| c.status),
            Some(Status::Ok),
            "{report:?}"
        );
        assert!(
            report.get("database.sync_replication").is_some(),
            "{report:?}"
        );
    }

    #[test]
    fn database_locale_needs_utf8() {
        for (enc, ctype) in [
            ("UTF8", "C.UTF-8"),
            ("UTF8", "en_US.utf8"),
            ("UTF8", "ru_RU.UTF-8"),
        ] {
            assert_eq!(locale_status(enc, ctype).0, Status::Ok, "{enc} {ctype}");
        }
        for (enc, ctype) in [
            ("UTF8", "C"),
            ("UTF8", "POSIX"),
            ("SQL_ASCII", "C"),
            ("LATIN1", "en_US.utf8"),
        ] {
            assert_eq!(locale_status(enc, ctype).0, Status::Fail, "{enc} {ctype}");
        }
    }

    #[test]
    fn third_party_extensions_warn() {
        assert_eq!(extensions_status(&[]).0, Status::Ok);
        assert_eq!(
            extensions_status(&["pg_stat_statements".to_owned()]).0,
            Status::Warn
        );
    }

    /// Внутренний CA (10 лет) и сертификат узла от него (825 сут.), как у роли thepost_pki.
    const PKI_CA: &[u8] = include_bytes!("testdata/pki-ca.pem");
    const PKI_NODE: &[u8] = include_bytes!("testdata/pki-node.pem");
    /// Самоподписанный сертификат другого издателя.
    const OTHER: &[u8] = include_bytes!("testdata/cert.pem");

    fn pki_config(dir: &Path, db_ca: &str, secrets: &str, node: &Path) -> Config {
        let slash = |p: &Path| p.display().to_string().replace('\\', "/");
        sections::registry()
            .expect("registry")
            .parse(
                &format!(
                    "domain: example.org\nprofile: dev\ndatabase:\n  url: \"postgres://thepost@db:5432/thepost?sslmode=verify-full{db_ca}\"\n\
                     blob:\n  url: file:///tmp/b\ntls:\n  cert_file: \"{node}\"\n  key_file: \"{node}\"\n{secrets}",
                    node = slash(node),
                ),
                Vec::new(),
            )
            .unwrap_or_else(|e| panic!("config {}: {e}", dir.display()))
    }

    async fn pki_report(cfg: &Config, now: SystemTime) -> Report {
        let mut report = Report::default();
        internal_pki(cfg, now, &mut report).await;
        report
    }

    fn pki_status(r: &Report, id: &str) -> Option<Status> {
        r.get(id).map(|c| c.status)
    }

    /// Сроки внутренних сертификатов (APP-25): OK, WARN за 60 сут., FAIL после срока; издатель узла — другой CA.
    #[tokio::test]
    async fn internal_certificates_expiry() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ca = dir.path().join("ca.pem");
        let node = dir.path().join("fullchain.pem");
        let other = dir.path().join("other.pem");
        std::fs::write(&ca, PKI_CA).expect("ca");
        std::fs::write(&node, PKI_NODE).expect("node");
        std::fs::write(&other, OTHER).expect("other");
        let slash = |p: &Path| p.display().to_string().replace('\\', "/");
        let day = Duration::from_secs(86_400);
        let node_end = pem_certs(PKI_NODE).expect("node").not_after;
        let ca_end = pem_certs(PKI_CA).expect("ca").not_after;
        assert!(ca_end > node_end + 365 * day, "CA дольше сертификата узла");

        // SMB: CA СУБД — sslrootcert адреса, OpenBao — secrets.tls.ca_file.
        let secrets = format!(
            "secrets:\n  url: https://bao.internal:8200\n  tls:\n    ca_file: \"{}\"\n  auth:\n    token: {{ token_file: /run/thepost/openbao-token }}\n",
            slash(&ca)
        );
        let cfg = pki_config(
            dir.path(),
            &format!("&sslrootcert={}", slash(&ca)),
            &secrets,
            &node,
        );
        for (now, node_status) in [
            (node_end - 200 * day, Status::Ok),
            (node_end - 59 * day, Status::Warn),
            (node_end + day, Status::Fail),
        ] {
            let r = pki_report(&cfg, now).await;
            assert_eq!(pki_status(&r, "pki.node"), Some(node_status), "{r:?}");
            assert_eq!(pki_status(&r, "pki.database_ca"), Some(Status::Ok), "{r:?}");
            assert_eq!(pki_status(&r, "pki.secrets_ca"), Some(Status::Ok), "{r:?}");
            if node_status != Status::Ok {
                let msg = &r.get("pki.node").expect("node").message;
                assert!(msg.contains("плейбук"), "{msg}");
            }
        }
        let r = pki_report(&cfg, node_end - 30 * day).await;
        assert!(
            r.get("pki.node")
                .expect("node")
                .message
                .contains("через 30 сут."),
            "{r:?}"
        );
        let r = pki_report(&cfg, ca_end - 10 * day).await;
        assert_eq!(
            pki_status(&r, "pki.database_ca"),
            Some(Status::Warn),
            "{r:?}"
        );
        let r = pki_report(&cfg, ca_end + day).await;
        assert_eq!(
            pki_status(&r, "pki.secrets_ca"),
            Some(Status::Fail),
            "{r:?}"
        );
        assert!(
            r.get("pki.secrets_ca")
                .expect("ca")
                .message
                .contains("thepost_pki"),
            "{r:?}"
        );
        assert_eq!(r.exit_code(), 1);

        // Сертификат узла другого издателя (ACME, коммерческий) — проверка tls.certificate.
        let cfg = pki_config(
            dir.path(),
            &format!("&sslrootcert={}", slash(&ca)),
            "",
            &other,
        );
        let r = pki_report(&cfg, node_end - 59 * day).await;
        assert_eq!(pki_status(&r, "pki.node"), Some(Status::Skipped), "{r:?}");
        assert_eq!(
            pki_status(&r, "pki.secrets_ca"),
            Some(Status::Skipped),
            "{r:?}"
        );

        // Без внутреннего CA и с системным набором — пропуск; нет файла CA — WARN.
        let cfg = pki_config(dir.path(), "", "", &node);
        let r = pki_report(&cfg, SystemTime::now()).await;
        for id in ["pki.database_ca", "pki.secrets_ca", "pki.node"] {
            assert_eq!(pki_status(&r, id), Some(Status::Skipped), "{id}: {r:?}");
        }
        let cfg = pki_config(
            dir.path(),
            "&sslrootcert=/etc/ssl/certs/ca-certificates.crt",
            "",
            &node,
        );
        let r = pki_report(&cfg, SystemTime::now()).await;
        assert_eq!(
            pki_status(&r, "pki.database_ca"),
            Some(Status::Skipped),
            "{r:?}"
        );
        let cfg = pki_config(dir.path(), "&sslrootcert=/nonexistent/ca.pem", "", &node);
        let r = pki_report(&cfg, SystemTime::now()).await;
        assert_eq!(
            pki_status(&r, "pki.database_ca"),
            Some(Status::Warn),
            "{r:?}"
        );
    }
}
