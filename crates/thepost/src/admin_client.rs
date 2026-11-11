//! `thepost admin <ресурс> <действие>` — клиент Admin API (APP-11, SRS 5.2): интерфейс без собственной логики.
//!
//! - Адрес — `--url`, иначе `THEPOST_ADMIN_URL`, иначе `http.admin.listen` из config.yaml (`allow_plaintext` — `http`,
//!   иначе `https`; сертификат на другое имя — `--ca-file` или `--insecure`).
//! - Вход — `Authorization: Bearer`: сессия Admin API (`tps_…`) или токен OIDC; значение — `THEPOST_ADMIN_TOKEN`
//!   (или `--token`, но ключ виден в списке процессов). Одноразовый токен `admin init` (`tpa_…`) обменивает на сессию
//!   [`ApiAction::Login`] (`POST /api/v1/session`); для остальных запросов он не принимается и узлом.
//! - Организация — `--org`, иначе `THEPOST_ADMIN_ORG`, иначе организация администратора (`GET /api/v1/whoami`).
//! - Вывод — таблица или ответ API (`--json`); и то и другое — из типизированного ответа.
//!
//! Тела запросов и ответы — структуры tp-admin, из которых строится спецификация OpenAPI (APP-21, [`wire`]).
//! Операции клиента — [`OPERATIONS`]: тест сверяет каждую со спецификацией OpenAPI tp-admin
//! (`tp_admin::server::openapi_spec`), команды проверяются против роутера tp-admin. Генератор клиента (progenitor)
//! не используется: спецификация — OpenAPI 3.1, progenitor читает только 3.0.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand};
use reqwest::header::CONTENT_TYPE;
use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tp_admin::AdminRole;
use tp_contracts::admin_token::is_local_admin_token;
use tp_contracts::transport::{AcceptedDomain, DomainKind};
use tp_contracts::{AccountId, TenantId};

use crate::sections::{self, HTTP, HttpSection};

/// Переменная окружения: адрес Admin API.
pub const ENV_URL: &str = "THEPOST_ADMIN_URL";
/// Переменная окружения: токен (сессия `tps_…` или токен OIDC).
pub const ENV_TOKEN: &str = "THEPOST_ADMIN_TOKEN";
/// Переменная окружения: организация (UUID).
pub const ENV_ORG: &str = "THEPOST_ADMIN_ORG";

/// Подключение к Admin API (общие ключи `thepost admin`; `admin init` их не использует).
#[derive(Debug, Clone, Args)]
pub struct ApiArgs {
    /// Адрес Admin API (по умолчанию — THEPOST_ADMIN_URL или http.admin.listen из config.yaml).
    #[arg(long, global = true)]
    pub url: Option<String>,
    /// Токен: сессия tps_… или токен OIDC (по умолчанию — THEPOST_ADMIN_TOKEN; ключ виден в списке процессов).
    #[arg(long, global = true)]
    pub token: Option<String>,
    /// Организация, UUID (по умолчанию — THEPOST_ADMIN_ORG или организация администратора).
    #[arg(long, global = true)]
    pub org: Option<String>,
    /// Вывод — ответ Admin API в JSON.
    #[arg(long, global = true)]
    pub json: bool,
    /// Корневые сертификаты (PEM) для проверки TLS Admin API.
    #[arg(long, global = true)]
    pub ca_file: Option<PathBuf>,
    /// Не проверять сертификат TLS (как curl -k) — только для loopback узла с сертификатом на другое имя.
    #[arg(long, global = true)]
    pub insecure: bool,
    /// Предельное время запроса, секунд.
    #[arg(long, global = true, default_value_t = DEFAULT_TIMEOUT_SECS)]
    pub timeout: u64,
}

/// Предельное время запроса по умолчанию, секунд.
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

impl Default for ApiArgs {
    fn default() -> Self {
        Self {
            url: None,
            token: None,
            org: None,
            json: false,
            ca_file: None,
            insecure: false,
            timeout: DEFAULT_TIMEOUT_SECS,
        }
    }
}

/// Команды Admin API.
#[derive(Debug, Clone, Subcommand)]
pub enum ApiAction {
    /// Обмен одноразового токена admin init (tpa_…, из THEPOST_ADMIN_TOKEN или --token) на сессию Admin API (tps_…).
    Login,
    /// Текущий администратор и его роли.
    Whoami,
    /// Учётные записи.
    Account {
        /// Действие.
        #[command(subcommand)]
        action: AccountAction,
    },
    /// Пароли приложений учётной записи (IMAP, SMTP, JMAP).
    AppPassword {
        /// Действие.
        #[command(subcommand)]
        action: AppPasswordAction,
    },
    /// Принятые домены организации (транспорт).
    Domain {
        /// Действие.
        #[command(subcommand)]
        action: DomainAction,
    },
}

/// Действия с учётными записями.
#[derive(Debug, Clone, Subcommand)]
pub enum AccountAction {
    /// Новая локальная учётная запись в принятом домене; пароль — `app-password create`.
    Create {
        /// Имя входа (адрес).
        login: String,
        /// Отображаемое имя (по умолчанию — имя входа).
        #[arg(long)]
        name: Option<String>,
    },
    /// Поиск по имени входа (полного списка Admin API пока не отдаёт).
    List {
        /// Имя входа (без учёта регистра).
        #[arg(long)]
        login: String,
    },
    /// Учётная запись по идентификатору или имени входа.
    Get {
        /// Идентификатор (UUID) или имя входа.
        account: String,
    },
}

/// Действия с паролями приложений.
#[derive(Debug, Clone, Subcommand)]
pub enum AppPasswordAction {
    /// Выпуск пароля: значение показывается один раз.
    Create {
        /// Учётная запись: идентификатор (UUID) или имя входа.
        account: String,
        /// Подпись («Телефон», «Thunderbird дома»).
        #[arg(long, default_value = "")]
        label: String,
    },
    /// Пароли учётной записи, включая отозванные (без значений).
    List {
        /// Учётная запись: идентификатор (UUID) или имя входа.
        account: String,
    },
    /// Отзыв пароля.
    Revoke {
        /// Учётная запись: идентификатор (UUID) или имя входа.
        account: String,
        /// Идентификатор пароля (из `app-password list`).
        id: String,
    },
}

/// Действия с принятыми доменами.
#[derive(Debug, Clone, Subcommand)]
pub enum DomainAction {
    /// Принятые домены организации.
    List,
    /// Новый принятый домен.
    Add {
        /// Имя домена.
        name: String,
        /// Внутренняя пересылка на узел host[:port] вместо собственного домена (authoritative).
        #[arg(long)]
        relay: Option<String>,
    },
    /// Удаление принятого домена.
    Remove {
        /// Идентификатор домена (из `domain list`).
        id: String,
    },
}

/// Операция Admin API: метод и шаблон пути, как в спецификации OpenAPI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Op {
    /// Метод HTTP.
    pub method: &'static str,
    /// Шаблон пути (`{имя}` — параметр).
    pub path: &'static str,
}

/// Обмен одноразового токена на сессию.
pub const SESSION: Op = Op {
    method: "POST",
    path: "/api/v1/session",
};
/// Текущий администратор.
pub const WHOAMI: Op = Op {
    method: "GET",
    path: "/api/v1/whoami",
};
/// Поиск учётной записи по имени входа.
pub const ACCOUNT_FIND: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/accounts",
};
/// Новая учётная запись.
pub const ACCOUNT_CREATE: Op = Op {
    method: "POST",
    path: "/api/v1/orgs/{org}/accounts",
};
/// Учётная запись по идентификатору.
pub const ACCOUNT_GET: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/accounts/{account}",
};
/// Пароли приложений учётной записи.
pub const APP_PASSWORD_LIST: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/accounts/{account}/app-passwords",
};
/// Выпуск пароля приложения.
pub const APP_PASSWORD_CREATE: Op = Op {
    method: "POST",
    path: "/api/v1/orgs/{org}/accounts/{account}/app-passwords",
};
/// Отзыв пароля приложения.
pub const APP_PASSWORD_REVOKE: Op = Op {
    method: "DELETE",
    path: "/api/v1/orgs/{org}/accounts/{account}/app-passwords/{id}",
};
/// Принятые домены организации.
pub const DOMAIN_LIST: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/transport/domains",
};
/// Новый принятый домен.
pub const DOMAIN_ADD: Op = Op {
    method: "POST",
    path: "/api/v1/orgs/{org}/transport/domains",
};
/// Удаление принятого домена.
pub const DOMAIN_REMOVE: Op = Op {
    method: "DELETE",
    path: "/api/v1/orgs/{org}/transport/domains/{id}",
};

/// Все операции клиента (сверка со спецификацией OpenAPI — тест).
pub const OPERATIONS: &[Op] = &[
    SESSION,
    WHOAMI,
    ACCOUNT_FIND,
    ACCOUNT_CREATE,
    ACCOUNT_GET,
    APP_PASSWORD_LIST,
    APP_PASSWORD_CREATE,
    APP_PASSWORD_REVOKE,
    DOMAIN_LIST,
    DOMAIN_ADD,
    DOMAIN_REMOVE,
];

/// Тела запросов и ответы Admin API — структуры tp-admin, из которых строится спецификация OpenAPI.
///
/// У структур запросов tp-admin есть только `Deserialize`, у структур ответов — только `Serialize` (стороны сервера).
/// Клиенту нужны обратные, поэтому здесь — их отражения:
/// - запрос клиент собирает структурой tp-admin и переводит в тело полной деструктуризацией (`From`);
/// - ответ читается отражением; [`Mirror::fields`] полностью деструктурирует структуру ответа tp-admin.
///
/// Переименование, добавление или удаление поля структуры tp-admin ломает сборку CLI. Имена и форматы полей JSON
/// сверяет тест: тело отражения читает структура запроса tp-admin (`deny_unknown_fields`), ответ, сериализованный
/// tp-admin, читается отражением и сериализуется обратно без изменений. Типы с обеими сторонами (`AdminRole`,
/// `PolicyBody`, `AcceptedDomain`) используются как есть.
pub mod wire {
    use serde::{Deserialize, Serialize};
    use tp_admin::AdminRole;
    use tp_admin::accounts::PolicyBody;
    use tp_contracts::transport::{AcceptedDomain, DomainKind};

    /// Ответ Admin API — отражение структуры ответа tp-admin `S` (у неё только `Serialize`).
    pub trait Mirror<S> {
        /// Полная деструктуризация `S`: проверка полей при сборке (не вызывается).
        fn fields(s: S);
    }

    // ---------- запросы ----------

    /// Тело `POST /api/v1/orgs/{org}/accounts` (`tp_admin::accounts::NewAccount`).
    #[derive(Debug, Serialize)]
    pub struct NewAccount {
        /// Имя входа.
        pub login: String,
        /// Отображаемое имя.
        pub display_name: Option<String>,
    }

    impl From<tp_admin::accounts::NewAccount> for NewAccount {
        fn from(r: tp_admin::accounts::NewAccount) -> Self {
            let tp_admin::accounts::NewAccount {
                login,
                display_name,
            } = r;
            Self {
                login,
                display_name,
            }
        }
    }

    /// Тело `POST …/app-passwords` (`tp_admin::app_passwords::NewAppPassword`).
    #[derive(Debug, Serialize)]
    pub struct NewAppPassword {
        /// Подпись.
        pub label: String,
    }

    impl From<tp_admin::app_passwords::NewAppPassword> for NewAppPassword {
        fn from(r: tp_admin::app_passwords::NewAppPassword) -> Self {
            let tp_admin::app_passwords::NewAppPassword { label } = r;
            Self { label }
        }
    }

    /// Тело `POST /api/v1/orgs/{org}/transport/domains` (`tp_admin::transport::DomainBody`).
    #[derive(Debug, Serialize)]
    pub struct DomainBody {
        /// Версия снимка кластера.
        pub version: u64,
        /// Имя домена.
        pub name: String,
        /// Вид.
        pub kind: DomainKind,
    }

    impl From<tp_admin::transport::DomainBody> for DomainBody {
        fn from(r: tp_admin::transport::DomainBody) -> Self {
            let tp_admin::transport::DomainBody {
                version,
                name,
                kind,
            } = r;
            Self {
                version,
                name,
                kind,
            }
        }
    }

    /// Параметры запроса поиска учётной записи (`tp_admin::accounts::AccountQuery`).
    pub fn account_query(q: tp_admin::accounts::AccountQuery) -> Vec<(&'static str, String)> {
        let tp_admin::accounts::AccountQuery { login } = q;
        vec![("login", login)]
    }

    /// Параметры запроса удаления по версии снимка (`tp_admin::transport::VersionQuery`).
    pub fn version_query(q: tp_admin::transport::VersionQuery) -> Vec<(&'static str, String)> {
        let tp_admin::transport::VersionQuery { version } = q;
        vec![("version", version.to_string())]
    }

    // ---------- ответы ----------

    /// Сессия Admin API (`tp_admin::init::AdminSession`).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Session {
        /// Значение для `Authorization: Bearer` (`tps_…`).
        pub session: String,
        /// Действует до (RFC 3339).
        pub expires_at: String,
        /// Организация.
        pub tenant: String,
        /// Учётная запись.
        pub account: String,
        /// Имя входа.
        pub login: String,
        /// Роли сессии.
        pub roles: Vec<AdminRole>,
    }

    impl Mirror<tp_admin::init::AdminSession> for Session {
        fn fields(s: tp_admin::init::AdminSession) {
            let tp_admin::init::AdminSession {
                session: _,
                expires_at: _,
                tenant: _,
                account: _,
                login: _,
                roles: _,
            } = s;
        }
    }

    /// Текущий администратор (`tp_admin::server::WhoAmI`).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct WhoAmI {
        /// Организация.
        pub tenant: String,
        /// Учётная запись.
        pub account: String,
        /// Имя входа.
        pub login: String,
        /// Роли.
        pub roles: Vec<AdminRole>,
    }

    impl Mirror<tp_admin::server::WhoAmI> for WhoAmI {
        fn fields(s: tp_admin::server::WhoAmI) {
            let tp_admin::server::WhoAmI {
                tenant: _,
                account: _,
                login: _,
                roles: _,
            } = s;
        }
    }

    /// Использование квоты (`tp_admin::accounts::QuotaInfo`).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Quota {
        /// Занято байт, включая корзину.
        pub used_bytes: u64,
        /// Число писем.
        pub messages: u64,
        /// Лимит; `None` — без ограничения.
        pub limit_bytes: Option<u64>,
    }

    impl Mirror<tp_admin::accounts::QuotaInfo> for Quota {
        fn fields(s: tp_admin::accounts::QuotaInfo) {
            let tp_admin::accounts::QuotaInfo {
                used_bytes: _,
                messages: _,
                limit_bytes: _,
            } = s;
        }
    }

    /// Политика уровня и действующая (`tp_admin::accounts::Policies`).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Policies {
        /// Заданное на этом уровне.
        pub policy: PolicyBody,
        /// Действующее с учётом вышестоящих уровней.
        pub effective: PolicyBody,
    }

    impl Mirror<tp_admin::accounts::Policies> for Policies {
        fn fields(s: tp_admin::accounts::Policies) {
            let tp_admin::accounts::Policies {
                policy: _,
                effective: _,
            } = s;
        }
    }

    /// Учётная запись (`tp_admin::accounts::AccountInfo`).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Account {
        /// Идентификатор.
        pub id: String,
        /// Основной домен.
        pub domain: String,
        /// Имя входа.
        pub login: String,
        /// Отображаемое имя.
        pub display_name: String,
        /// Идентификатор в каталоге; `None` — локальная запись.
        pub external_id: Option<String>,
        /// Вход разрешён.
        pub enabled: bool,
        /// Квота.
        pub quota: Quota,
        /// Политики ящика.
        #[serde(flatten)]
        pub policies: Policies,
    }

    impl Mirror<tp_admin::accounts::AccountInfo> for Account {
        fn fields(s: tp_admin::accounts::AccountInfo) {
            let tp_admin::accounts::AccountInfo {
                id: _,
                domain: _,
                login: _,
                display_name: _,
                external_id: _,
                enabled: _,
                quota: _,
                policies: _,
            } = s;
        }
    }

    /// Пароль приложения без значения (`tp_admin::app_passwords::AppPasswordInfo`).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct AppPassword {
        /// Идентификатор.
        pub id: String,
        /// Подпись.
        pub label: String,
        /// Выпущен (RFC 3339).
        pub created_at: String,
        /// Отозван (RFC 3339); `None` — действует.
        pub revoked_at: Option<String>,
    }

    impl Mirror<tp_admin::app_passwords::AppPasswordInfo> for AppPassword {
        fn fields(s: tp_admin::app_passwords::AppPasswordInfo) {
            let tp_admin::app_passwords::AppPasswordInfo {
                id: _,
                label: _,
                created_at: _,
                revoked_at: _,
            } = s;
        }
    }

    /// Выпущенный пароль (`tp_admin::app_passwords::IssuedAppPassword`).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct IssuedAppPassword {
        /// Пароль.
        #[serde(flatten)]
        pub info: AppPassword,
        /// Значение пароля.
        pub password: String,
    }

    impl Mirror<tp_admin::app_passwords::IssuedAppPassword> for IssuedAppPassword {
        fn fields(s: tp_admin::app_passwords::IssuedAppPassword) {
            let tp_admin::app_passwords::IssuedAppPassword {
                info: _,
                password: _,
            } = s;
        }
    }

    /// Принятые домены организации (`tp_admin::transport::DomainList`).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DomainList {
        /// Версия снимка кластера.
        pub version: u64,
        /// Домены.
        pub domains: Vec<AcceptedDomain>,
    }

    impl Mirror<tp_admin::transport::DomainList> for DomainList {
        fn fields(s: tp_admin::transport::DomainList) {
            let tp_admin::transport::DomainList {
                version: _,
                domains: _,
            } = s;
        }
    }

    /// Записанный домен (`tp_admin::transport::DomainWritten`).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct DomainWritten {
        /// Новая версия снимка кластера.
        pub version: u64,
        /// Домен.
        pub domain: AcceptedDomain,
    }

    impl Mirror<tp_admin::transport::DomainWritten> for DomainWritten {
        fn fields(s: tp_admin::transport::DomainWritten) {
            let tp_admin::transport::DomainWritten {
                version: _,
                domain: _,
            } = s;
        }
    }

    /// Новая версия снимка после записи (`tp_admin::transport::Written`).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Written {
        /// Версия снимка.
        pub version: u64,
    }

    impl Mirror<tp_admin::transport::Written> for Written {
        fn fields(s: tp_admin::transport::Written) {
            let tp_admin::transport::Written { version: _ } = s;
        }
    }

    /// Ошибка RFC 9457 (`tp_admin::server::Problem`).
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct Problem {
        /// Код HTTP.
        pub status: u16,
        /// Краткое описание кода.
        pub title: String,
        /// Подробность.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub detail: Option<String>,
    }

    impl Mirror<tp_admin::server::Problem> for Problem {
        fn fields(s: tp_admin::server::Problem) {
            let tp_admin::server::Problem {
                status: _,
                title: _,
                detail: _,
            } = s;
        }
    }
}

/// Результат команды: текст для stdout или сообщение об ошибке.
type CliResult<T> = std::result::Result<T, String>;

/// Запрос без тела.
const NO_BODY: Option<&()> = None;

/// Клиент Admin API: адрес и токен входа.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base: Url,
    token: String,
}

impl Client {
    /// Клиент к `base` с токеном `token`; TLS — по `args` (`--ca-file`, `--insecure`), предельное время — `--timeout`.
    pub fn new(base: Url, token: String, args: &ApiArgs) -> CliResult<Self> {
        let mut b = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(args.timeout.max(1)));
        if let Some(path) = &args.ca_file {
            let pem = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            let certs = reqwest::Certificate::from_pem_bundle(&pem)
                .map_err(|e| format!("{}: не PEM: {e}", path.display()))?;
            for c in certs {
                b = b.add_root_certificate(c);
            }
        }
        if args.insecure {
            eprintln!("thepost: admin: сертификат TLS Admin API не проверяется (--insecure)");
            b = b.danger_accept_invalid_certs(true);
        }
        let http = b.build().map_err(|e| format!("клиент HTTP: {e}"))?;
        Ok(Self { http, base, token })
    }

    /// Адрес операции: параметры шаблона по порядку (кодируются как сегменты пути) и запрос `query`.
    fn url(&self, op: Op, params: &[&str], query: &[(&str, String)]) -> CliResult<Url> {
        let mut url = self.base.clone();
        {
            let mut segs = url
                .path_segments_mut()
                .map_err(|()| format!("адрес Admin API {}: не HTTP", self.base))?;
            segs.pop_if_empty();
            let mut params = params.iter();
            for seg in op.path.split('/').filter(|s| !s.is_empty()) {
                if seg.starts_with('{') {
                    let value = params
                        .next()
                        .ok_or_else(|| format!("{}: нет параметра {seg}", op.path))?;
                    segs.push(value);
                } else {
                    segs.push(seg);
                }
            }
        }
        if !query.is_empty() {
            url.query_pairs_mut().extend_pairs(query);
        }
        Ok(url)
    }

    /// Запрос `op` с телом `body` (JSON); ответ 2xx — тело как есть, иначе — ошибка с кодом и причиной из
    /// problem details.
    async fn send<B: Serialize + ?Sized>(
        &self,
        op: Op,
        params: &[&str],
        query: &[(&str, String)],
        body: Option<&B>,
    ) -> CliResult<Vec<u8>> {
        let url = self.url(op, params, query)?;
        let method = Method::from_bytes(op.method.as_bytes()).map_err(|e| e.to_string())?;
        let mut req = self.http.request(method, url).bearer_auth(&self.token);
        if let Some(body) = body {
            let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
            req = req.header(CONTENT_TYPE, "application/json").body(bytes);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("Admin API {}: {e}", self.base))?;
        let status = resp.status();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("Admin API: ответ: {e}"))?;
        if status.is_success() {
            Ok(bytes.to_vec())
        } else {
            Err(problem(op, status, &bytes))
        }
    }

    /// Запрос `op`; ответ — отражение структуры ответа tp-admin `R` ([`wire`]).
    pub async fn call<B: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        op: Op,
        params: &[&str],
        query: &[(&str, String)],
        body: Option<&B>,
    ) -> CliResult<R> {
        let bytes = self.send(op, params, query, body).await?;
        serde_json::from_slice(&bytes).map_err(|e| {
            format!(
                "{} {}: ответ не по спецификации Admin API: {e}",
                op.method, op.path
            )
        })
    }

    /// Запрос `op` без тела ответа (`204`).
    pub async fn call_empty<B: Serialize + ?Sized>(
        &self,
        op: Op,
        params: &[&str],
        query: &[(&str, String)],
        body: Option<&B>,
    ) -> CliResult<()> {
        self.send(op, params, query, body).await.map(drop)
    }
}

/// Сообщение об отказе: код, описание и подробность ответа (RFC 9457, [`wire::Problem`]), подсказка для 401.
fn problem(op: Op, status: StatusCode, body: &[u8]) -> String {
    let p = serde_json::from_slice::<wire::Problem>(body).ok();
    let title = p
        .as_ref()
        .map(|p| p.title.as_str())
        .filter(|t| !t.is_empty())
        .or_else(|| status.canonical_reason())
        .unwrap_or_default();
    let mut msg = format!("{} {}: {} {title}", op.method, op.path, status.as_u16());
    if let Some(detail) = p.as_ref().and_then(|p| p.detail.as_deref()) {
        msg.push_str(&format!(": {detail}"));
    }
    if status == StatusCode::UNAUTHORIZED {
        msg.push_str(if op == SESSION {
            " (токен admin init одноразовый: уже обменян, истёк или отозван; новый — thepost admin init --force)"
        } else {
            " (токен не принят: сессия истекла или токен OIDC неверен; новая сессия — thepost admin login)"
        });
    }
    msg
}

/// `thepost admin <ресурс> <действие>`: код возврата (0 — успех, 1 — отказ или ошибка).
pub async fn command(config: &Path, args: ApiArgs, action: ApiAction) -> i32 {
    match run(config, &args, action).await {
        Ok(out) => {
            print!("{out}");
            0
        }
        Err(msg) => {
            eprintln!("thepost: admin: {msg}");
            1
        }
    }
}

async fn run(config: &Path, args: &ApiArgs, action: ApiAction) -> CliResult<String> {
    let token = token(args, matches!(action, ApiAction::Login))?;
    let base = base_url(config, args)?;
    let client = Client::new(base, token, args)?;
    execute(&client, args, action).await
}

/// Токен из `--token` или `THEPOST_ADMIN_TOKEN`; `login` ждёт одноразовый токен, остальные команды — не его.
fn token(args: &ApiArgs, login: bool) -> CliResult<String> {
    let token = args
        .token
        .clone()
        .or_else(|| std::env::var(ENV_TOKEN).ok())
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
        .ok_or_else(|| {
            format!(
                "нет токена: задайте {ENV_TOKEN} (сессия tps_… из thepost admin login или токен OIDC) или --token"
            )
        })?;
    match (login, is_local_admin_token(&token)) {
        (true, false) => Err(format!(
            "login обменивает одноразовый токен thepost admin init (tpa_…); в {ENV_TOKEN} — другой токен"
        )),
        (false, true) => Err(
            "одноразовый токен admin init (tpa_…) не принимается для запросов: обменяйте его на сессию — \
             thepost admin login"
                .to_owned(),
        ),
        _ => Ok(token),
    }
}

/// Адрес Admin API: `--url`, `THEPOST_ADMIN_URL` или `http.admin` из config.yaml.
fn base_url(config: &Path, args: &ApiArgs) -> CliResult<Url> {
    let explicit = args
        .url
        .clone()
        .or_else(|| std::env::var(ENV_URL).ok())
        .filter(|u| !u.trim().is_empty());
    let raw = match explicit {
        Some(u) => u.trim().to_owned(),
        None => url_from_config(config)?,
    };
    let url = Url::parse(&raw).map_err(|e| format!("адрес Admin API {raw:?}: {e}"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(format!(
            "адрес Admin API {raw:?}: нужна схема http или https"
        ));
    }
    Ok(url)
}

/// `https://<http.admin.listen>` (`http://` при `allow_plaintext`); адрес — только внутренний (проверка секции).
fn url_from_config(config: &Path) -> CliResult<String> {
    let hint = format!("укажите --url или {ENV_URL}");
    let cfg = sections::registry()
        .and_then(|r| r.load(config))
        .map_err(|e| format!("{}: {e} ({hint})", config.display()))?;
    let http = cfg
        .section::<HttpSection>(HTTP)
        .map_err(|e| format!("{}: секция http: {e} ({hint})", config.display()))?;
    let listener = http.admin.as_ref().ok_or_else(|| {
        format!(
            "{}: нет http.admin — Admin API на узле не слушает ({hint})",
            config.display()
        )
    })?;
    let scheme = if listener.allow_plaintext {
        "http"
    } else {
        "https"
    };
    Ok(format!("{scheme}://{}", listener.listen))
}

/// Вывод команды: ответ в JSON (`--json`) или текст `text`.
fn out<T: Serialize>(args: &ApiArgs, v: &T, text: String) -> CliResult<String> {
    if args.json {
        serde_json::to_string_pretty(v)
            .map(|j| format!("{j}\n"))
            .map_err(|e| e.to_string())
    } else {
        Ok(text)
    }
}

/// Выполняет команду через `client`; текст для stdout.
pub async fn execute(client: &Client, args: &ApiArgs, action: ApiAction) -> CliResult<String> {
    match action {
        ApiAction::Login => {
            let s: wire::Session = client.call(SESSION, &[], &[], NO_BODY).await?;
            let text = format!(
                "{}Значение сессии показывается один раз. Дальше: export {ENV_TOKEN}=<сессия>\n",
                record(&[
                    ("Сессия", s.session.clone()),
                    ("Действует до", s.expires_at.clone()),
                    ("Администратор", s.login.clone()),
                    ("Организация", s.tenant.clone()),
                    ("Роли", roles(&s.roles)),
                ])
            );
            out(args, &s, text)
        }
        ApiAction::Whoami => {
            let w: wire::WhoAmI = client.call(WHOAMI, &[], &[], NO_BODY).await?;
            out(args, &w, whoami_record(&w))
        }
        ApiAction::Account { action } => {
            let org = org(client, args).await?;
            match action {
                AccountAction::Create { login, name } => {
                    let body = wire::NewAccount::from(tp_admin::accounts::NewAccount {
                        login,
                        display_name: name,
                    });
                    let a: wire::Account = client
                        .call(ACCOUNT_CREATE, &[org.as_str()], &[], Some(&body))
                        .await?;
                    out(args, &a, account_record(&a))
                }
                AccountAction::List { login } => {
                    let found = find_accounts(client, &org, login).await?;
                    let rows: Vec<Vec<String>> = found.iter().map(account_row).collect();
                    out(args, &found, table(ACCOUNT_COLUMNS, &rows))
                }
                AccountAction::Get { account } => {
                    let id = account_id(client, &org, &account).await?;
                    let a: wire::Account = client
                        .call(ACCOUNT_GET, &[org.as_str(), id.as_str()], &[], NO_BODY)
                        .await?;
                    out(args, &a, account_record(&a))
                }
            }
        }
        ApiAction::AppPassword { action } => {
            let org = org(client, args).await?;
            match action {
                AppPasswordAction::Create { account, label } => {
                    let id = account_id(client, &org, &account).await?;
                    let body =
                        wire::NewAppPassword::from(tp_admin::app_passwords::NewAppPassword {
                            label,
                        });
                    let p: wire::IssuedAppPassword = client
                        .call(
                            APP_PASSWORD_CREATE,
                            &[org.as_str(), id.as_str()],
                            &[],
                            Some(&body),
                        )
                        .await?;
                    let text = format!(
                        "{}Пароль показывается один раз.\n",
                        record(&[
                            ("Пароль", p.password.clone()),
                            ("Идентификатор", p.info.id.clone()),
                            ("Подпись", or_dash(&p.info.label)),
                            ("Выпущен", p.info.created_at.clone()),
                        ])
                    );
                    out(args, &p, text)
                }
                AppPasswordAction::List { account } => {
                    let id = account_id(client, &org, &account).await?;
                    let list: Vec<wire::AppPassword> = client
                        .call(
                            APP_PASSWORD_LIST,
                            &[org.as_str(), id.as_str()],
                            &[],
                            NO_BODY,
                        )
                        .await?;
                    let rows: Vec<Vec<String>> = list
                        .iter()
                        .map(|p| {
                            vec![
                                p.id.clone(),
                                or_dash(&p.label),
                                p.created_at.clone(),
                                optional(p.revoked_at.as_ref()),
                            ]
                        })
                        .collect();
                    out(
                        args,
                        &list,
                        table(&["ИДЕНТИФИКАТОР", "ПОДПИСЬ", "ВЫПУЩЕН", "ОТОЗВАН"], &rows),
                    )
                }
                AppPasswordAction::Revoke { account, id } => {
                    let account = account_id(client, &org, &account).await?;
                    client
                        .call_empty(
                            APP_PASSWORD_REVOKE,
                            &[org.as_str(), account.as_str(), id.as_str()],
                            &[],
                            NO_BODY,
                        )
                        .await?;
                    out(args, &(), format!("Пароль {id} отозван.\n"))
                }
            }
        }
        ApiAction::Domain { action } => {
            let org = org(client, args).await?;
            match action {
                DomainAction::List => {
                    let list = domains(client, &org).await?;
                    let rows: Vec<Vec<String>> = list.domains.iter().map(domain_row).collect();
                    let text = format!(
                        "{}Версия снимка: {}\n",
                        table(DOMAIN_COLUMNS, &rows),
                        list.version
                    );
                    out(args, &list, text)
                }
                DomainAction::Add { name, relay } => {
                    let version = domains(client, &org).await?.version;
                    let kind = match relay {
                        Some(next_hop) => DomainKind::InternalRelay { next_hop },
                        None => DomainKind::Authoritative,
                    };
                    let body = wire::DomainBody::from(tp_admin::transport::DomainBody {
                        version,
                        name,
                        kind,
                    });
                    let w: wire::DomainWritten = client
                        .call(DOMAIN_ADD, &[org.as_str()], &[], Some(&body))
                        .await?;
                    let fields: Vec<(&str, String)> = DOMAIN_COLUMNS
                        .iter()
                        .copied()
                        .zip(domain_row(&w.domain))
                        .collect();
                    let text = format!("{}Версия снимка: {}\n", record(&fields), w.version);
                    out(args, &w, text)
                }
                DomainAction::Remove { id } => {
                    let version = domains(client, &org).await?.version;
                    let query = wire::version_query(tp_admin::transport::VersionQuery { version });
                    let w: wire::Written = client
                        .call(DOMAIN_REMOVE, &[org.as_str(), id.as_str()], &query, NO_BODY)
                        .await?;
                    out(
                        args,
                        &w,
                        format!("Домен {id} удалён. Версия снимка: {}\n", w.version),
                    )
                }
            }
        }
    }
}

const ACCOUNT_COLUMNS: &[&str] = &["ИДЕНТИФИКАТОР", "ИМЯ ВХОДА", "ИМЯ", "ВХОД"];

const DOMAIN_COLUMNS: &[&str] = &["ИДЕНТИФИКАТОР", "ДОМЕН", "ВИД"];

/// Текущий администратор — строки «подпись: значение».
fn whoami_record(w: &wire::WhoAmI) -> String {
    record(&[
        ("Администратор", w.login.clone()),
        ("Организация", w.tenant.clone()),
        ("Учётная запись", w.account.clone()),
        ("Роли", roles(&w.roles)),
    ])
}

/// Учётная запись — строки «подпись: значение».
fn account_record(a: &wire::Account) -> String {
    record(&[
        ("Идентификатор", a.id.clone()),
        ("Имя входа", a.login.clone()),
        ("Имя", or_dash(&a.display_name)),
        ("Вход разрешён", a.enabled.to_string()),
        ("Домен", a.domain.clone()),
        ("Занято, байт", a.quota.used_bytes.to_string()),
        ("Писем", a.quota.messages.to_string()),
        ("Квота, байт", optional(a.quota.limit_bytes.as_ref())),
    ])
}

/// Учётная запись — строка таблицы [`ACCOUNT_COLUMNS`].
fn account_row(a: &wire::Account) -> Vec<String> {
    vec![
        a.id.clone(),
        a.login.clone(),
        or_dash(&a.display_name),
        a.enabled.to_string(),
    ]
}

/// Принятый домен — строка таблицы [`DOMAIN_COLUMNS`].
fn domain_row(d: &AcceptedDomain) -> Vec<String> {
    let kind = match &d.kind {
        DomainKind::Authoritative => "authoritative".to_owned(),
        DomainKind::InternalRelay { next_hop } => format!("internal_relay → {next_hop}"),
    };
    vec![d.id.to_string(), d.name.clone(), kind]
}

/// Роли через запятую (имена — как в API).
fn roles(roles: &[AdminRole]) -> String {
    roles
        .iter()
        .map(|r| match serde_json::to_value(r) {
            Ok(serde_json::Value::String(s)) => s,
            _ => format!("{r:?}"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Строка; пустая — «—».
fn or_dash(s: &str) -> String {
    if s.is_empty() {
        "—".to_owned()
    } else {
        s.to_owned()
    }
}

/// Необязательное значение; нет — «—».
fn optional<T: ToString>(v: Option<&T>) -> String {
    v.map_or_else(|| "—".to_owned(), ToString::to_string)
}

/// Организация: `--org`, `THEPOST_ADMIN_ORG` или организация администратора (`whoami`).
async fn org(client: &Client, args: &ApiArgs) -> CliResult<String> {
    let explicit = args
        .org
        .clone()
        .or_else(|| std::env::var(ENV_ORG).ok())
        .map(|o| o.trim().to_owned())
        .filter(|o| !o.is_empty());
    let org = match explicit {
        Some(o) => o,
        None => {
            let w: wire::WhoAmI = client.call(WHOAMI, &[], &[], NO_BODY).await?;
            w.tenant
        }
    };
    org.parse::<TenantId>()
        .map_err(|e| format!("организация {org:?}: {e}"))?;
    Ok(org)
}

/// Учётные записи с именем входа `login`.
async fn find_accounts(client: &Client, org: &str, login: String) -> CliResult<Vec<wire::Account>> {
    let query = wire::account_query(tp_admin::accounts::AccountQuery { login });
    client.call(ACCOUNT_FIND, &[org], &query, NO_BODY).await
}

/// Идентификатор учётной записи: UUID как есть, иначе поиск по имени входа.
async fn account_id(client: &Client, org: &str, account: &str) -> CliResult<String> {
    if account.parse::<AccountId>().is_ok() {
        return Ok(account.to_owned());
    }
    find_accounts(client, org, account.to_owned())
        .await?
        .into_iter()
        .next()
        .map(|a| a.id)
        .ok_or_else(|| format!("учётная запись {account} не найдена"))
}

/// Принятые домены организации и версия снимка кластера (для записи — из последнего чтения).
async fn domains(client: &Client, org: &str) -> CliResult<wire::DomainList> {
    client.call(DOMAIN_LIST, &[org], &[], NO_BODY).await
}

/// Строки «подпись: значение».
fn record(fields: &[(&str, String)]) -> String {
    let width = fields
        .iter()
        .map(|(t, _)| t.chars().count())
        .max()
        .unwrap_or(0);
    fields
        .iter()
        .map(|(title, value)| {
            format!(
                "{}:{} {}\n",
                title,
                pad("", width - title.chars().count()),
                value
            )
        })
        .collect()
}

/// Таблица с заголовком `cols`; строк нет — «нет записей».
fn table(cols: &[&str], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return "нет записей\n".to_owned();
    }
    let widths: Vec<usize> = cols
        .iter()
        .enumerate()
        .map(|(i, t)| {
            rows.iter()
                .map(|r| r.get(i).map_or(0, |c| c.chars().count()))
                .chain([t.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: Vec<&str>| {
        let mut s = cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| pad(c, *w))
            .collect::<Vec<_>>()
            .join("  ");
        s.truncate(s.trim_end().len());
        s.push('\n');
        s
    };
    let mut out = line(cols.to_vec());
    for r in rows {
        out.push_str(&line(r.iter().map(String::as_str).collect()));
    }
    out
}

fn pad(s: &str, w: usize) -> String {
    format!("{s}{}", " ".repeat(w.saturating_sub(s.chars().count())))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::Value;
    use sqlx::types::time::OffsetDateTime;
    use tp_admin::accounts::PolicyBody;
    use tp_admin::{AdminConfig, AdminRole, Deps, InitRequest, RoleBinding};
    use tp_contracts::auth::Principal;
    use tp_contracts::ext::DecisionHooks;
    use tp_contracts::meta::Policy;
    use tp_contracts::{DomainId, Result};
    use tp_testkit::{
        Clock, FakeAuthenticator, MemAppPasswords, MemBlobIndex, MemBlobStore, MemDeliveryLog,
        MemEventBus, MemGroups, MemLocalAdminTokens, MemMetadata, MemModeration, MemPolicies,
        MemQuota, MemTransport, MemTrash, SystemClock,
    };

    use super::*;

    /// Каждая операция клиента есть в спецификации OpenAPI tp-admin (метод и шаблон пути): расхождение — ошибка.
    #[test]
    fn operations_match_openapi() {
        let spec = serde_json::to_value(tp_admin::server::openapi_spec()).expect("spec");
        let paths = spec["paths"].as_object().expect("paths");
        for op in OPERATIONS {
            let item = paths
                .get(op.path)
                .unwrap_or_else(|| panic!("{}: нет в спецификации Admin API", op.path));
            assert!(
                item.get(op.method.to_ascii_lowercase()).is_some(),
                "{} {}: нет в спецификации Admin API",
                op.method,
                op.path
            );
        }
    }

    #[test]
    fn parses_admin_commands() {
        use clap::Parser;

        use crate::cli::{AdminAction, Cli, Command};

        let cli = Cli::try_parse_from([
            "thepost", "admin", "account", "create", "a@b.test", "--name", "А", "--json",
        ])
        .expect("account create");
        let Command::Admin { api, action } = cli.command else {
            panic!("{:?}", cli.command)
        };
        assert!(api.json);
        assert!(matches!(
            action,
            AdminAction::Api(ApiAction::Account {
                action: AccountAction::Create { ref login, name: Some(_) }
            }) if login == "a@b.test"
        ));
        let cli = Cli::try_parse_from([
            "thepost",
            "admin",
            "--url",
            "https://127.0.0.1:8443",
            "domain",
            "add",
            "b.test",
            "--org",
            "00000000-0000-0000-0000-000000000001",
        ])
        .expect("domain add");
        let Command::Admin { api, action } = cli.command else {
            panic!("{:?}", cli.command)
        };
        assert_eq!(api.url.as_deref(), Some("https://127.0.0.1:8443"));
        assert!(api.org.is_some());
        assert!(matches!(
            action,
            AdminAction::Api(ApiAction::Domain {
                action: DomainAction::Add { relay: None, .. }
            })
        ));
        for args in [
            &["thepost", "admin", "login"][..],
            &["thepost", "admin", "whoami"],
            &[
                "thepost",
                "admin",
                "app-password",
                "create",
                "a@b.test",
                "--label",
                "x",
            ],
            &[
                "thepost",
                "admin",
                "app-password",
                "revoke",
                "a@b.test",
                "id1",
            ],
            &["thepost", "admin", "domain", "list"],
            &["thepost", "admin", "account", "list", "--login", "a@b.test"],
            &["thepost", "admin", "init", "--email", "a@b.test"],
        ] {
            Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
        assert!(Cli::try_parse_from(["thepost", "admin", "account", "list"]).is_err());
    }

    #[test]
    fn token_kinds() {
        let args = |t: &str| ApiArgs {
            token: Some(t.to_owned()),
            ..ApiArgs::default()
        };
        assert!(token(&args("tpa_x"), true).is_ok());
        assert!(token(&args("tps_x"), true).is_err());
        let err = token(&args("tpa_x"), false).expect_err("local token");
        assert!(err.contains("thepost admin login"), "{err}");
        assert_eq!(token(&args(" tps_x "), false).expect("session"), "tps_x");
    }

    #[test]
    fn url_from_config_yaml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.yaml");
        let base = "domain: example.test\nprofile: dev\nroles: [admin]\nservice_http:\n  allow_plaintext: true\ndatabase:\n  url: postgres://thepost@db/thepost?sslmode=disable\n  tls: { allow_plaintext: true }\nblob:\n  url: file:///var/lib/thepost/blob\n";
        std::fs::write(
            &path,
            format!(
                "{base}http:\n  admin: {{ listen: \"127.0.0.1:9443\", allow_plaintext: true }}\n"
            ),
        )
        .expect("write");
        assert_eq!(
            url_from_config(&path).expect("url"),
            "http://127.0.0.1:9443"
        );
        std::fs::write(
            &path,
            format!("{base}http:\n  admin: {{ listen: \"[::1]:8443\" }}\n"),
        )
        .expect("write");
        assert_eq!(url_from_config(&path).expect("url"), "https://[::1]:8443");
        let err = url_from_config(&dir.path().join("nope.yaml")).expect_err("no config");
        assert!(err.contains(ENV_URL), "{err}");
    }

    #[test]
    fn renders_table_and_record() {
        let rows = vec![
            vec!["1".into(), "a@b.test".into(), "Анна".into(), "true".into()],
            vec!["22".into(), "bb@b.test".into(), or_dash(""), "false".into()],
        ];
        let t = table(ACCOUNT_COLUMNS, &rows);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines.len(), 3, "{t}");
        assert!(lines[0].starts_with("ИДЕНТИФИКАТОР  ИМЯ ВХОДА"), "{t}");
        assert!(
            lines[2].contains("bb@b.test") && lines[2].contains('—'),
            "{t}"
        );
        assert_eq!(
            table(ACCOUNT_COLUMNS, &[]),
            "нет записей
"
        );
        let w = wire::WhoAmI {
            tenant: TenantId::DEFAULT.to_string(),
            account: AccountId::new().to_string(),
            login: "a".into(),
            roles: vec![AdminRole::InstallationAdmin, AdminRole::Operator],
        };
        let r = whoami_record(&w);
        assert!(
            r.contains("Роли:") && r.contains("installation_admin, operator"),
            "{r}"
        );
    }

    /// Тело запроса отражения читает структура запроса tp-admin (`deny_unknown_fields`) — тот же запрос.
    fn request<W: Serialize, R: DeserializeOwned + std::fmt::Debug>(wire: W, original: String) {
        let json = serde_json::to_value(&wire).expect("wire");
        let parsed: R =
            serde_json::from_value(json.clone()).unwrap_or_else(|e| panic!("{json}: {e}"));
        assert_eq!(format!("{parsed:?}"), original, "{json}");
    }

    /// Ответ, сериализованный tp-admin, отражение читает и сериализует обратно без изменений.
    fn response<S: Serialize, M: Serialize + DeserializeOwned>(server: S) {
        let json = serde_json::to_value(&server).expect("server");
        let mirror: M =
            serde_json::from_value(json.clone()).unwrap_or_else(|e| panic!("{json}: {e}"));
        assert_eq!(serde_json::to_value(&mirror).expect("mirror"), json);
    }

    /// Отражения [`wire`] совпадают со структурами tp-admin по JSON (поля при сборке — `From` и `Mirror`).
    #[test]
    fn wire_matches_tp_admin() {
        use tp_admin::{accounts, app_passwords, init, server, transport};

        for name in [Some("Анна"), None] {
            let r = || accounts::NewAccount {
                login: "a@b.test".into(),
                display_name: name.map(str::to_owned),
            };
            request::<_, accounts::NewAccount>(wire::NewAccount::from(r()), format!("{:?}", r()));
        }
        let r = || app_passwords::NewAppPassword {
            label: "Телефон".into(),
        };
        request::<_, app_passwords::NewAppPassword>(
            wire::NewAppPassword::from(r()),
            format!("{:?}", r()),
        );
        for kind in [
            DomainKind::Authoritative,
            DomainKind::InternalRelay {
                next_hop: "mx.old.test:25".into(),
            },
        ] {
            let r = || transport::DomainBody {
                version: 7,
                name: "b.test".into(),
                kind: kind.clone(),
            };
            request::<_, transport::DomainBody>(wire::DomainBody::from(r()), format!("{:?}", r()));
        }
        assert_eq!(
            wire::account_query(accounts::AccountQuery {
                login: "a@b.test".into()
            }),
            vec![("login", "a@b.test".to_owned())]
        );
        assert_eq!(
            wire::version_query(transport::VersionQuery { version: 3 }),
            vec![("version", "3".to_owned())]
        );

        let now = OffsetDateTime::now_utc();
        let roles = vec![AdminRole::InstallationAdmin];
        response::<_, wire::Session>(init::AdminSession {
            session: "tps_x".into(),
            expires_at: now,
            tenant: TenantId::DEFAULT.to_string(),
            account: AccountId::new().to_string(),
            login: "admin@b.test".into(),
            roles: roles.clone(),
        });
        response::<_, wire::WhoAmI>(server::WhoAmI {
            tenant: TenantId::DEFAULT.to_string(),
            account: AccountId::new().to_string(),
            login: "admin@b.test".into(),
            roles,
        });
        let account = || accounts::AccountInfo {
            id: AccountId::new().to_string(),
            domain: "b.test".into(),
            login: "a@b.test".into(),
            display_name: "Анна".into(),
            external_id: Some("cn=a".into()),
            enabled: true,
            quota: accounts::QuotaInfo {
                used_bytes: 10,
                messages: 1,
                limit_bytes: Some(100),
            },
            policies: accounts::Policies {
                policy: PolicyBody {
                    quota_bytes: Some(100),
                    ..PolicyBody::default()
                },
                effective: PolicyBody::default(),
            },
        };
        response::<_, wire::Account>(account());
        response::<_, Vec<wire::Account>>(vec![account(), account()]);
        let info = |revoked_at| app_passwords::AppPasswordInfo {
            id: "p1".into(),
            label: "Телефон".into(),
            created_at: now,
            revoked_at,
        };
        response::<_, Vec<wire::AppPassword>>(vec![info(None), info(Some(now))]);
        response::<_, wire::IssuedAppPassword>(app_passwords::IssuedAppPassword {
            info: info(None),
            password: "secret".into(),
        });
        let domain = || AcceptedDomain {
            id: DomainId::new(),
            tenant: TenantId::DEFAULT,
            name: "b.test".into(),
            kind: DomainKind::InternalRelay {
                next_hop: "mx.old.test".into(),
            },
        };
        response::<_, wire::DomainList>(transport::DomainList {
            version: 2,
            domains: vec![domain()],
        });
        response::<_, wire::DomainWritten>(transport::DomainWritten {
            version: 3,
            domain: domain(),
        });
        response::<_, wire::Written>(transport::Written { version: 4 });
        for detail in [None, Some("плохое имя".to_owned())] {
            response::<_, wire::Problem>(server::Problem {
                status: 400,
                title: "Bad Request".into(),
                detail,
            });
        }
    }

    /// Политики уровней: в тестах не задаются.
    struct NoPolicies;

    #[async_trait]
    impl tp_admin::PolicyStore for NoPolicies {
        async fn policy(&self, _: TenantId, _: tp_admin::PolicyTarget) -> Result<Policy> {
            Ok(Policy::default())
        }

        async fn set_policy(
            &self,
            _: TenantId,
            _: tp_admin::PolicyTarget,
            _: &Policy,
        ) -> Result<()> {
            Ok(())
        }
    }

    /// Домены организации (не транспорт): в тестах не используются.
    struct NoDomains;

    #[async_trait]
    impl tp_admin::DomainStore for NoDomains {
        async fn list(&self, _: TenantId) -> Result<Vec<tp_admin::Domain>> {
            Ok(Vec::new())
        }

        async fn create(&self, _: TenantId, _: &str) -> Result<tp_admin::Domain> {
            Err(tp_contracts::Error::UNAVAILABLE)
        }

        async fn delete(&self, _: TenantId, _: DomainId) -> Result<()> {
            Err(tp_contracts::Error::UNAVAILABLE)
        }
    }

    /// Admin API tp-admin на хранилищах testkit на loopback: локальный вход подключён, `oidc` — токен OIDC
    /// администратора инсталляции.
    async fn admin_api() -> (Arc<tp_admin::Admin>, Url) {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let index = MemBlobIndex::new(clock.clone());
        let meta = MemMetadata::new(clock.clone()).with_blob_index(index.clone());
        let quota = MemQuota::new(&meta);
        let policies = MemPolicies::new(&meta);
        let trash = MemTrash::new(&meta).with_policies(policies.clone());
        let store = tp_mailstore::Store::new(tp_mailstore::Deps {
            blobs: Arc::new(MemBlobStore::new(clock)),
            index: Arc::new(index),
            messages: Arc::new(meta.clone()),
            changes: Arc::new(meta.clone()),
            trash: Arc::new(trash.clone()),
            quota: Arc::new(quota.clone()),
            bus: Arc::new(MemEventBus::default()),
            hooks: DecisionHooks::default(),
        });
        let app_passwords = tp_auth::AppPasswords::new(
            Arc::new(MemAppPasswords::new(&meta)),
            tp_auth::PasswordConfig::default(),
        )
        .expect("app passwords");
        let auth = FakeAuthenticator::default();
        auth.add_token(
            "oidc",
            Principal {
                tenant: TenantId::DEFAULT,
                account: AccountId::new(),
                login: "root@example.test".into(),
                groups: BTreeSet::from(["mail-root".to_owned()]),
                scopes: None,
            },
        );
        let deps = Deps {
            auth: Arc::new(auth),
            audit: Arc::new(tp_admin::LogAudit),
            audit_log: None,
            delivery: Arc::new(MemDeliveryLog::new()),
            groups: Arc::new(MemGroups::new()),
            moderation: Arc::new(MemModeration::default()),
            modules: Vec::new(),
            accounts: Arc::new(meta.clone()),
            quota: Arc::new(quota),
            policies: Arc::new(NoPolicies),
            resolver: Arc::new(policies),
            domains: Arc::new(NoDomains),
            trash: Arc::new(trash),
            store: Arc::new(store),
            app_passwords,
        };
        let cfg = AdminConfig {
            roles: vec![RoleBinding {
                role: AdminRole::InstallationAdmin,
                logins: vec![],
                groups: vec!["mail-root".into()],
            }],
            ..AdminConfig::default()
        };
        let admin = Arc::new(
            tp_admin::Admin::new(deps, cfg)
                .expect("admin")
                .with_transport(Arc::new(MemTransport::new()))
                .with_local_tokens(Arc::new(MemLocalAdminTokens::new())),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let router = admin.clone().router();
        tokio::spawn(async move { axum::serve(listener, router).await });
        let url = Url::parse(&format!("http://{addr}")).expect("url");
        (admin, url)
    }

    fn client(url: &Url, token: &str) -> Client {
        Client::new(url.clone(), token.to_owned(), &ApiArgs::default()).expect("client")
    }

    async fn exec(c: &Client, json: bool, action: ApiAction) -> CliResult<String> {
        let args = ApiArgs {
            json,
            ..ApiArgs::default()
        };
        execute(c, &args, action).await
    }

    /// Сценарий пилота без curl: admin init → login → домены, учётная запись, пароль приложения, whoami;
    /// вход по токену OIDC.
    #[tokio::test]
    async fn pilot_flow_against_admin_api() {
        let (admin, url) = admin_api().await;
        let granted = admin
            .init(&InitRequest {
                email: "admin@example.test".into(),
                display_name: None,
                force: false,
            })
            .await
            .expect("init");

        // Обмен одноразового токена: сессия один раз, повтор — 401 с подсказкой.
        let local = client(&url, &granted.token);
        let session: Value =
            serde_json::from_str(&exec(&local, true, ApiAction::Login).await.expect("login"))
                .expect("json");
        let session = session["session"].as_str().expect("session").to_owned();
        assert!(session.starts_with("tps_"), "{session}");
        let err = exec(&local, false, ApiAction::Login)
            .await
            .expect_err("повтор");
        assert!(err.contains("401") && err.contains("одноразовый"), "{err}");

        let s = client(&url, &session);
        let who = exec(&s, false, ApiAction::Whoami).await.expect("whoami");
        assert!(
            who.contains("admin@example.test") && who.contains("installation_admin"),
            "{who}"
        );

        // Домен admin init уже принят; добавление второго.
        let domains = exec(
            &s,
            false,
            ApiAction::Domain {
                action: DomainAction::List,
            },
        )
        .await
        .expect("domains");
        assert!(domains.contains("example.test"), "{domains}");
        let added = exec(
            &s,
            true,
            ApiAction::Domain {
                action: DomainAction::Add {
                    name: "Second.Test".into(),
                    relay: None,
                },
            },
        )
        .await
        .expect("domain add");
        let added: Value = serde_json::from_str(&added).expect("json");
        assert_eq!(added["domain"]["name"], "second.test");

        // Учётная запись: создание, поиск, пароль приложения по имени входа.
        let created = exec(
            &s,
            false,
            ApiAction::Account {
                action: AccountAction::Create {
                    login: "a@second.test".into(),
                    name: Some("Анна".into()),
                },
            },
        )
        .await
        .expect("account create");
        assert!(
            created.contains("a@second.test") && created.contains("Анна"),
            "{created}"
        );
        let dup = exec(
            &s,
            false,
            ApiAction::Account {
                action: AccountAction::Create {
                    login: "a@second.test".into(),
                    name: None,
                },
            },
        )
        .await
        .expect_err("занято");
        assert!(dup.contains("409"), "{dup}");
        let listed = exec(
            &s,
            false,
            ApiAction::Account {
                action: AccountAction::List {
                    login: "A@second.test".into(),
                },
            },
        )
        .await
        .expect("account list");
        assert_eq!(listed.lines().count(), 2, "{listed}");
        let issued = exec(
            &s,
            true,
            ApiAction::AppPassword {
                action: AppPasswordAction::Create {
                    account: "a@second.test".into(),
                    label: "swaks".into(),
                },
            },
        )
        .await
        .expect("app-password create");
        let issued: Value = serde_json::from_str(&issued).expect("json");
        assert!(!issued["password"].as_str().unwrap_or_default().is_empty());
        let id = issued["id"].as_str().expect("id").to_owned();
        exec(
            &s,
            false,
            ApiAction::AppPassword {
                action: AppPasswordAction::Revoke {
                    account: "a@second.test".into(),
                    id: id.clone(),
                },
            },
        )
        .await
        .expect("revoke");
        let list = exec(
            &s,
            true,
            ApiAction::AppPassword {
                action: AppPasswordAction::List {
                    account: "a@second.test".into(),
                },
            },
        )
        .await
        .expect("app-password list");
        let list: Value = serde_json::from_str(&list).expect("json");
        assert_eq!(list[0]["id"], id.as_str());
        assert!(list[0]["revoked_at"].is_string(), "{list}");

        let missing = exec(
            &s,
            false,
            ApiAction::Account {
                action: AccountAction::Get {
                    account: "nobody@second.test".into(),
                },
            },
        )
        .await
        .expect_err("нет записи");
        assert!(missing.contains("не найдена"), "{missing}");

        // Токен OIDC — тот же вход Bearer; неверный — 401 с подсказкой.
        let oidc = exec(&client(&url, "oidc"), false, ApiAction::Whoami)
            .await
            .expect("oidc");
        assert!(oidc.contains("root@example.test"), "{oidc}");
        let bad = exec(&client(&url, "bogus"), false, ApiAction::Whoami)
            .await
            .expect_err("bogus");
        assert!(bad.contains("401") && bad.contains("admin login"), "{bad}");

        // Внутренняя пересылка и удаление домена по версии снимка последнего чтения.
        let third = exec(
            &s,
            true,
            ApiAction::Domain {
                action: DomainAction::Add {
                    name: "third.test".into(),
                    relay: Some("mx.third.test:25".into()),
                },
            },
        )
        .await
        .expect("domain add relay");
        let third: Value = serde_json::from_str(&third).expect("json");
        assert!(
            third["domain"]["kind"]["internal_relay"].is_object(),
            "{third}"
        );
        let id = third["domain"]["id"]
            .as_str()
            .expect("domain id")
            .to_owned();
        let removed = exec(
            &s,
            false,
            ApiAction::Domain {
                action: DomainAction::Remove { id },
            },
        )
        .await
        .expect("domain remove");
        assert!(removed.contains("удалён"), "{removed}");
        let domains = exec(
            &s,
            false,
            ApiAction::Domain {
                action: DomainAction::List,
            },
        )
        .await
        .expect("domains");
        assert!(!domains.contains("third.test"), "{domains}");
    }
}
