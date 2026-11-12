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
//! Тела запросов, параметры и ответы — прямо структуры tp-admin, из которых строится спецификация OpenAPI
//! (APP-21, APP-22; у них обе стороны serde — ADM-13).
//! Операции клиента — [`OPERATIONS`]: тест сверяет каждую со спецификацией OpenAPI tp-admin
//! (`tp_admin::server::openapi_spec`), команды проверяются против роутера tp-admin. Генератор клиента (progenitor)
//! не используется: спецификация — OpenAPI 3.1, progenitor читает только 3.0.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand};
use reqwest::header::{CONTENT_TYPE, HeaderMap, LINK};
use reqwest::{Method, StatusCode, Url};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tp_admin::AdminRole;
use tp_admin::accounts::{AccountInfo, AccountPatch, AccountQuery, NewAccount};
use tp_admin::app_passwords::{AppPasswordInfo, IssuedAppPassword, NewAppPassword};
use tp_admin::init::AdminSession;
use tp_admin::server::{Problem, WhoAmI};
use tp_admin::sieve::{Script, ScriptBody, ScriptProblem, ScriptText, VacationBody};
use tp_admin::transport::{DomainBody, DomainList, DomainWritten, VersionQuery, Written};
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
    /// Правила Sieve учётной записи: просмотр, правка, включение и отключение за пользователя.
    Sieve {
        /// Действие.
        #[command(subcommand)]
        action: SieveAction,
    },
    /// Автоответ учётной записи: просмотр, включение и выключение за пользователя.
    Vacation {
        /// Действие.
        #[command(subcommand)]
        action: Box<VacationAction>,
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
    /// Учётные записи по возрастанию имени входа (страницы Admin API — по ссылке Link rel="next"); --login — поиск одной.
    List {
        /// Поиск по имени входа (без учёта регистра); несовместимо с --prefix и --limit.
        #[arg(long, conflicts_with_all = ["prefix", "limit"])]
        login: Option<String>,
        /// Имя входа начинается с префикса (без учёта регистра).
        #[arg(long)]
        prefix: Option<String>,
        /// Не больше N записей (по умолчанию — все).
        #[arg(long, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
        limit: Option<usize>,
    },
    /// Учётная запись по идентификатору или имени входа.
    Get {
        /// Идентификатор (UUID) или имя входа.
        account: String,
    },
    /// Блокировка входа локальной учётной записи: новый вход запрещён, открытые сеансы не разрываются.
    /// Запись каталога блокируется в каталоге (ответ 409).
    Block {
        /// Идентификатор (UUID) или имя входа.
        account: String,
    },
    /// Снятие блокировки входа локальной учётной записи.
    Unblock {
        /// Идентификатор (UUID) или имя входа.
        account: String,
    },
    /// Смена отображаемого имени локальной учётной записи (у записи каталога — 409).
    Rename {
        /// Идентификатор (UUID) или имя входа.
        account: String,
        /// Новое отображаемое имя (непустое, одна строка).
        name: String,
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

/// Действия с правилами Sieve учётной записи (решение 62, ADM-17); каждое изменение — в журнале аудита Admin API.
#[derive(Debug, Clone, Subcommand)]
pub enum SieveAction {
    /// Скрипты учётной записи: имя, размер, какой активен.
    List {
        /// Учётная запись: идентификатор (UUID) или имя входа.
        account: String,
    },
    /// Текст скрипта (в stdout — как записан).
    Get {
        /// Учётная запись: идентификатор (UUID) или имя входа.
        account: String,
        /// Имя скрипта (из `sieve list`).
        name: String,
    },
    /// Создание или замена текста скрипта: текст проверяется, активность сохраняется.
    Put {
        /// Учётная запись: идентификатор (UUID) или имя входа.
        account: String,
        /// Имя скрипта.
        name: String,
        /// Файл с текстом скрипта (UTF-8); `-` — stdin.
        #[arg(long, value_name = "ПУТЬ")]
        file: PathBuf,
    },
    /// Включение скрипта: он выполняется при доставке, прежний активный отключается.
    Activate {
        /// Учётная запись: идентификатор (UUID) или имя входа.
        account: String,
        /// Имя скрипта (из `sieve list`).
        name: String,
    },
    /// Отключение правил: активного скрипта нет, тексты остаются.
    Deactivate {
        /// Учётная запись: идентификатор (UUID) или имя входа.
        account: String,
    },
    /// Удаление скрипта; активный не удаляется (409) — сначала `sieve deactivate`.
    Delete {
        /// Учётная запись: идентификатор (UUID) или имя входа.
        account: String,
        /// Имя скрипта (из `sieve list`).
        name: String,
    },
    /// Скрипт организации: выполняется при доставке каждого письма до скрипта пользователя.
    Org {
        /// Действие.
        #[command(subcommand)]
        action: OrgSieveAction,
    },
}

/// Действия со скриптом организации (решение 76); каждое изменение — в журнале аудита Admin API.
#[derive(Debug, Clone, Subcommand)]
pub enum OrgSieveAction {
    /// Текст скрипта организации (в stdout — как записан); не задан — 404.
    Get,
    /// Запись скрипта организации: текст проверяется до записи.
    ///
    /// Скрипты организации и пользователя выполняются подряд, их действия копятся. Запрещающее правило
    /// организации пишите как `discard; stop;` — после одного `discard;` скрипт пользователя всё равно
    /// выполнится, и его `keep` отменит запрет.
    Put {
        /// Файл с текстом скрипта (UTF-8); `-` — stdin.
        #[arg(long, value_name = "ПУТЬ")]
        file: PathBuf,
    },
    /// Удаление скрипта организации; повторное удаление — не ошибка.
    Delete,
}

/// Действия с автоответом учётной записи (модель JMAP VacationResponse); каждое изменение — в журнале аудита.
#[derive(Debug, Clone, Subcommand)]
pub enum VacationAction {
    /// Автоответ учётной записи; не задан — выключен.
    Get {
        /// Учётная запись: идентификатор (UUID) или имя входа.
        account: String,
    },
    /// Включение автоответа (`--off` — выключение). Незаданные параметры остаются прежними.
    Set {
        /// Учётная запись: идентификатор (UUID) или имя входа.
        account: String,
        /// Выключить автоответ; текст, тема и срок сохраняются.
        #[arg(long)]
        off: bool,
        /// Начало действия, RFC 3339 (например, 2026-12-01T00:00:00Z); `-` — сразу.
        #[arg(long, value_name = "ВРЕМЯ")]
        from: Option<String>,
        /// Конец действия (не включая), RFC 3339; `-` — бессрочно.
        #[arg(long, value_name = "ВРЕМЯ")]
        to: Option<String>,
        /// Тема ответа; `-` — по умолчанию («Автоответ: <тема письма>»).
        #[arg(long)]
        subject: Option<String>,
        /// Текст ответа.
        #[arg(long, conflicts_with = "text_file")]
        text: Option<String>,
        /// Файл с текстом ответа (UTF-8); `-` — stdin.
        #[arg(long, value_name = "ПУТЬ")]
        text_file: Option<PathBuf>,
        /// Файл с текстом ответа HTML (UTF-8).
        #[arg(long, value_name = "ПУТЬ")]
        html_file: Option<PathBuf>,
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
/// Поиск учётной записи по имени входа и страница списка.
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
/// Изменение учётной записи: отображаемое имя, блокировка входа.
pub const ACCOUNT_PATCH: Op = Op {
    method: "PATCH",
    path: "/api/v1/orgs/{org}/accounts/{account}",
};
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
/// Скрипты Sieve учётной записи.
pub const SIEVE_LIST: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts",
};
/// Скрипт с текстом.
pub const SIEVE_GET: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts/{name}",
};
/// Создание или замена текста скрипта.
pub const SIEVE_PUT: Op = Op {
    method: "PUT",
    path: "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts/{name}",
};
/// Включение скрипта.
pub const SIEVE_ACTIVATE: Op = Op {
    method: "POST",
    path: "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts/{name}/activate",
};
/// Отключение правил учётной записи.
pub const SIEVE_DEACTIVATE: Op = Op {
    method: "POST",
    path: "/api/v1/orgs/{org}/accounts/{account}/sieve/deactivate",
};
/// Удаление скрипта (активный — 409).
pub const SIEVE_DELETE: Op = Op {
    method: "DELETE",
    path: "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts/{name}",
};
/// Автоответ учётной записи.
pub const VACATION_GET: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/accounts/{account}/vacation",
};
/// Замена автоответа учётной записи целиком.
pub const VACATION_PUT: Op = Op {
    method: "PUT",
    path: "/api/v1/orgs/{org}/accounts/{account}/vacation",
};
/// Скрипт организации.
pub const ORG_SIEVE_GET: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/sieve",
};
/// Запись скрипта организации.
pub const ORG_SIEVE_PUT: Op = Op {
    method: "PUT",
    path: "/api/v1/orgs/{org}/sieve",
};
/// Удаление скрипта организации.
pub const ORG_SIEVE_DELETE: Op = Op {
    method: "DELETE",
    path: "/api/v1/orgs/{org}/sieve",
};

/// Все операции клиента (сверка со спецификацией OpenAPI — тест).
pub const OPERATIONS: &[Op] = &[
    SESSION,
    WHOAMI,
    ACCOUNT_FIND,
    ACCOUNT_CREATE,
    ACCOUNT_GET,
    ACCOUNT_PATCH,
    APP_PASSWORD_LIST,
    APP_PASSWORD_CREATE,
    APP_PASSWORD_REVOKE,
    DOMAIN_LIST,
    DOMAIN_ADD,
    DOMAIN_REMOVE,
    SIEVE_LIST,
    SIEVE_GET,
    SIEVE_PUT,
    SIEVE_ACTIVATE,
    SIEVE_DEACTIVATE,
    SIEVE_DELETE,
    VACATION_GET,
    VACATION_PUT,
    ORG_SIEVE_GET,
    ORG_SIEVE_PUT,
    ORG_SIEVE_DELETE,
];

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
    fn url(&self, op: Op, params: &[&str], query: &[(String, String)]) -> CliResult<Url> {
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

    /// Запрос `op` с телом `body` (JSON); ответ 2xx — заголовки и тело как есть, иначе — ошибка с кодом и причиной
    /// из problem details.
    async fn send<B: Serialize + ?Sized>(
        &self,
        op: Op,
        params: &[&str],
        query: &[(String, String)],
        body: Option<&B>,
    ) -> CliResult<(HeaderMap, Vec<u8>)> {
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
        let headers = resp.headers().clone();
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| format!("Admin API: ответ: {e}"))?;
        if status.is_success() {
            Ok((headers, bytes.to_vec()))
        } else {
            Err(problem(op, status, &bytes))
        }
    }

    /// Запрос `op`; ответ — структура ответа tp-admin `R`.
    pub async fn call<B: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        op: Op,
        params: &[&str],
        query: &[(String, String)],
        body: Option<&B>,
    ) -> CliResult<R> {
        self.call_page(op, params, query, body)
            .await
            .map(|(r, _)| r)
    }

    /// Запрос `op`; ответ — структура ответа tp-admin `R` и цель ссылки `Link: <…>; rel="next"` (RFC 8288), если есть.
    pub async fn call_page<B: Serialize + ?Sized, R: DeserializeOwned>(
        &self,
        op: Op,
        params: &[&str],
        query: &[(String, String)],
        body: Option<&B>,
    ) -> CliResult<(R, Option<String>)> {
        let (headers, bytes) = self.send(op, params, query, body).await?;
        let r = serde_json::from_slice(&bytes).map_err(|e| {
            format!(
                "{} {}: ответ не по спецификации Admin API: {e}",
                op.method, op.path
            )
        })?;
        Ok((r, next_link(&headers)))
    }

    /// Параметры запроса из ссылки Admin API (`/api/v1/…?…`, путь от корня Admin API).
    fn link_query(&self, link: &str) -> CliResult<Vec<(String, String)>> {
        let url = self
            .base
            .join(link)
            .map_err(|e| format!("ссылка Link {link:?}: {e}"))?;
        Ok(url.query_pairs().into_owned().collect())
    }

    /// Запрос `op` без тела ответа (`204`).
    pub async fn call_empty<B: Serialize + ?Sized>(
        &self,
        op: Op,
        params: &[&str],
        query: &[(String, String)],
        body: Option<&B>,
    ) -> CliResult<()> {
        self.send(op, params, query, body).await.map(drop)
    }
}

/// Цель первой ссылки `rel="next"` заголовков `Link` (RFC 8288).
fn next_link(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(LINK)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .find_map(|link| {
            let (target, params) = link.trim().strip_prefix('<')?.split_once('>')?;
            params
                .split(';')
                .filter_map(|p| p.trim().strip_prefix("rel="))
                .any(|rel| {
                    rel.trim_matches('"')
                        .split_ascii_whitespace()
                        .any(|r| r.eq_ignore_ascii_case("next"))
                })
                .then(|| target.to_owned())
        })
}

/// Параметры запроса из структуры запроса tp-admin: поля объекта JSON, `null` пропускается.
fn query<Q: Serialize>(q: &Q) -> CliResult<Vec<(String, String)>> {
    match serde_json::to_value(q).map_err(|e| e.to_string())? {
        serde_json::Value::Object(fields) => Ok(fields
            .into_iter()
            .filter_map(|(k, v)| match v {
                serde_json::Value::Null => None,
                serde_json::Value::String(s) => Some((k, s)),
                other => Some((k, other.to_string())),
            })
            .collect()),
        other => Err(format!("параметры запроса — не объект: {other}")),
    }
}

/// Поле ответа `v` как в JSON Admin API (даты — RFC 3339, форматом tp-admin); `null` или нет поля — «—».
fn json_field<T: Serialize>(v: &T, field: &str) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::Object(mut fields)) => match fields.remove(field) {
            Some(serde_json::Value::String(s)) => s,
            None | Some(serde_json::Value::Null) => "—".to_owned(),
            Some(other) => other.to_string(),
        },
        _ => "—".to_owned(),
    }
}

/// Сообщение об отказе: код, описание и подробность ответа (RFC 9457, [`Problem`]), подсказка для 401.
fn problem(op: Op, status: StatusCode, body: &[u8]) -> String {
    let p = serde_json::from_slice::<Problem>(body).ok();
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
    if let Ok(script) = serde_json::from_slice::<ScriptProblem>(body) {
        for e in &script.errors {
            msg.push_str(&format!(
                "\n  строка {}, столбец {}: {}",
                e.line, e.column, e.message
            ));
        }
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
            let s: AdminSession = client.call(SESSION, &[], &[], NO_BODY).await?;
            let text = format!(
                "{}Значение сессии показывается один раз. Дальше: export {ENV_TOKEN}=<сессия>\n",
                record(&[
                    ("Сессия", s.session.clone()),
                    ("Действует до", json_field(&s, "expires_at")),
                    ("Администратор", s.login.clone()),
                    ("Организация", s.tenant.clone()),
                    ("Роли", roles(&s.roles)),
                ])
            );
            out(args, &s, text)
        }
        ApiAction::Whoami => {
            let w: WhoAmI = client.call(WHOAMI, &[], &[], NO_BODY).await?;
            out(args, &w, whoami_record(&w))
        }
        ApiAction::Account { action } => {
            let org = org(client, args).await?;
            match action {
                AccountAction::Create { login, name } => {
                    let body = NewAccount {
                        login,
                        display_name: name,
                    };
                    let a: AccountInfo = client
                        .call(ACCOUNT_CREATE, &[org.as_str()], &[], Some(&body))
                        .await?;
                    out(args, &a, account_record(&a))
                }
                AccountAction::List {
                    login: Some(login), ..
                } => {
                    let found = find_accounts(client, &org, login).await?;
                    let rows: Vec<Vec<String>> = found.iter().map(account_row).collect();
                    out(args, &found, table(ACCOUNT_COLUMNS, &rows))
                }
                AccountAction::List {
                    login: None,
                    prefix,
                    limit,
                } => {
                    let (found, more) = list_accounts(client, &org, prefix, limit).await?;
                    let rows: Vec<Vec<String>> = found.iter().map(account_row).collect();
                    let mut text = table(ACCOUNT_COLUMNS, &rows);
                    if more {
                        text.push_str(&format!(
                            "Показаны первые {}; есть ещё — уточните --prefix или увеличьте --limit.\n",
                            found.len()
                        ));
                    }
                    out(args, &found, text)
                }
                AccountAction::Get { account } => {
                    let id = account_id(client, &org, &account).await?;
                    let a: AccountInfo = client
                        .call(ACCOUNT_GET, &[org.as_str(), id.as_str()], &[], NO_BODY)
                        .await?;
                    out(args, &a, account_record(&a))
                }
                AccountAction::Block { account } => {
                    let patch = AccountPatch {
                        enabled: Some(false),
                        ..AccountPatch::default()
                    };
                    let a = patch_account(client, &org, &account, &patch).await?;
                    out(args, &a, account_record(&a))
                }
                AccountAction::Unblock { account } => {
                    let patch = AccountPatch {
                        enabled: Some(true),
                        ..AccountPatch::default()
                    };
                    let a = patch_account(client, &org, &account, &patch).await?;
                    out(args, &a, account_record(&a))
                }
                AccountAction::Rename { account, name } => {
                    let patch = AccountPatch {
                        display_name: Some(name),
                        ..AccountPatch::default()
                    };
                    let a = patch_account(client, &org, &account, &patch).await?;
                    out(args, &a, account_record(&a))
                }
            }
        }
        ApiAction::AppPassword { action } => {
            let org = org(client, args).await?;
            match action {
                AppPasswordAction::Create { account, label } => {
                    let id = account_id(client, &org, &account).await?;
                    let body = NewAppPassword { label };
                    let p: IssuedAppPassword = client
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
                            ("Выпущен", json_field(&p.info, "created_at")),
                        ])
                    );
                    out(args, &p, text)
                }
                AppPasswordAction::List { account } => {
                    let id = account_id(client, &org, &account).await?;
                    let list: Vec<AppPasswordInfo> = client
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
                                json_field(p, "created_at"),
                                json_field(p, "revoked_at"),
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
                    let body = DomainBody {
                        version,
                        name,
                        kind,
                    };
                    let w: DomainWritten = client
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
                    let query = query(&VersionQuery { version })?;
                    let w: Written = client
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
        ApiAction::Sieve { action } => {
            let org = org(client, args).await?;
            match action {
                SieveAction::List { account } => {
                    let id = account_id(client, &org, &account).await?;
                    let list: Vec<Script> = client
                        .call(SIEVE_LIST, &[org.as_str(), id.as_str()], &[], NO_BODY)
                        .await?;
                    let rows: Vec<Vec<String>> = list
                        .iter()
                        .map(|s| {
                            let active = if s.active { "да" } else { "нет" };
                            vec![s.name.clone(), s.size.to_string(), active.to_owned()]
                        })
                        .collect();
                    out(args, &list, table(&["ИМЯ", "РАЗМЕР", "АКТИВЕН"], &rows))
                }
                SieveAction::Get { account, name } => {
                    let id = account_id(client, &org, &account).await?;
                    let script: ScriptBody = client
                        .call(
                            SIEVE_GET,
                            &[org.as_str(), id.as_str(), name.as_str()],
                            &[],
                            NO_BODY,
                        )
                        .await?;
                    let text = script.text.clone();
                    out(args, &script, text)
                }
                SieveAction::Put {
                    account,
                    name,
                    file,
                } => {
                    let body = ScriptText {
                        text: script_text(file).await?,
                    };
                    let id = account_id(client, &org, &account).await?;
                    client
                        .call_empty(
                            SIEVE_PUT,
                            &[org.as_str(), id.as_str(), name.as_str()],
                            &[],
                            Some(&body),
                        )
                        .await?;
                    out(args, &(), format!("Скрипт {name} записан.\n"))
                }
                SieveAction::Activate { account, name } => {
                    let id = account_id(client, &org, &account).await?;
                    client
                        .call_empty(
                            SIEVE_ACTIVATE,
                            &[org.as_str(), id.as_str(), name.as_str()],
                            &[],
                            NO_BODY,
                        )
                        .await?;
                    out(args, &(), format!("Скрипт {name} включён.\n"))
                }
                SieveAction::Deactivate { account } => {
                    let id = account_id(client, &org, &account).await?;
                    client
                        .call_empty(SIEVE_DEACTIVATE, &[org.as_str(), id.as_str()], &[], NO_BODY)
                        .await?;
                    out(args, &(), "Правила отключены.\n".to_owned())
                }
                SieveAction::Delete { account, name } => {
                    let id = account_id(client, &org, &account).await?;
                    client
                        .call_empty(
                            SIEVE_DELETE,
                            &[org.as_str(), id.as_str(), name.as_str()],
                            &[],
                            NO_BODY,
                        )
                        .await?;
                    out(args, &(), format!("Скрипт {name} удалён.\n"))
                }
                SieveAction::Org { action } => match action {
                    OrgSieveAction::Get => {
                        let script: ScriptText = client
                            .call(ORG_SIEVE_GET, &[org.as_str()], &[], NO_BODY)
                            .await?;
                        let text = script.text.clone();
                        out(args, &script, text)
                    }
                    OrgSieveAction::Put { file } => {
                        let body = ScriptText {
                            text: script_text(file).await?,
                        };
                        client
                            .call_empty(ORG_SIEVE_PUT, &[org.as_str()], &[], Some(&body))
                            .await?;
                        out(args, &(), "Скрипт организации записан.\n".to_owned())
                    }
                    OrgSieveAction::Delete => {
                        client
                            .call_empty(ORG_SIEVE_DELETE, &[org.as_str()], &[], NO_BODY)
                            .await?;
                        out(args, &(), "Скрипт организации удалён.\n".to_owned())
                    }
                },
            }
        }
        ApiAction::Vacation { action } => {
            let org = org(client, args).await?;
            match *action {
                VacationAction::Get { account } => {
                    let id = account_id(client, &org, &account).await?;
                    let v: VacationBody = client
                        .call(VACATION_GET, &[org.as_str(), id.as_str()], &[], NO_BODY)
                        .await?;
                    out(args, &v, vacation_record(&v))
                }
                VacationAction::Set {
                    account,
                    off,
                    from,
                    to,
                    subject,
                    text,
                    text_file,
                    html_file,
                } => {
                    let text = match text_file {
                        Some(file) => Some(script_text(file).await?),
                        None => text,
                    };
                    let html = match html_file {
                        Some(file) => Some(script_text(file).await?),
                        None => None,
                    };
                    let id = account_id(client, &org, &account).await?;
                    let params = [org.as_str(), id.as_str()];
                    let current: VacationBody =
                        client.call(VACATION_GET, &params, &[], NO_BODY).await?;
                    let change = VacationChange {
                        enabled: !off,
                        from,
                        to,
                        subject,
                        text,
                        html,
                    };
                    let body = change.apply(&current)?;
                    let v: VacationBody =
                        client.call(VACATION_PUT, &params, &[], Some(&body)).await?;
                    out(args, &v, vacation_record(&v))
                }
            }
        }
    }
}

/// Параметры `vacation set`: `None` — поле без изменений, `-` — сброс (`from`, `to`, `subject`).
struct VacationChange {
    enabled: bool,
    from: Option<String>,
    to: Option<String>,
    subject: Option<String>,
    text: Option<String>,
    html: Option<String>,
}

impl VacationChange {
    /// Автоответ `current` с изменениями; время разбирает сама структура tp-admin (RFC 3339).
    fn apply(self, current: &VacationBody) -> CliResult<VacationBody> {
        let mut v = serde_json::to_value(current).map_err(|e| e.to_string())?;
        v["enabled"] = self.enabled.into();
        let reset = |s: String| {
            if s == "-" {
                serde_json::Value::Null
            } else {
                s.into()
            }
        };
        for (field, value) in [
            ("from", self.from.map(reset)),
            ("to", self.to.map(reset)),
            ("subject", self.subject.map(reset)),
            ("text_body", self.text.map(Into::into)),
            ("html_body", self.html.map(Into::into)),
        ] {
            if let Some(value) = value {
                v[field] = value;
            }
        }
        serde_json::from_value(v).map_err(|e| {
            format!("автоответ: {e} (время — RFC 3339, например 2026-12-01T00:00:00Z)")
        })
    }
}

/// Автоответ — строки «подпись: значение».
fn vacation_record(v: &VacationBody) -> String {
    let field = |name: &str| or_dash(&json_field(v, name));
    record(&[
        ("Включён", if v.enabled { "да" } else { "нет" }.to_owned()),
        ("С", field("from")),
        ("По", field("to")),
        ("Тема", field("subject")),
        ("Текст", field("text_body")),
        ("Текст HTML", field("html_body")),
    ])
}

/// `PATCH` учётной записи `account` (идентификатор или имя входа).
async fn patch_account(
    client: &Client,
    org: &str,
    account: &str,
    patch: &AccountPatch,
) -> CliResult<AccountInfo> {
    let id = account_id(client, org, account).await?;
    client
        .call(ACCOUNT_PATCH, &[org, id.as_str()], &[], Some(patch))
        .await
}

/// Текст скрипта из файла `file` (`-` — stdin); чтение — вне потоков async.
async fn script_text(file: PathBuf) -> CliResult<String> {
    tokio::task::spawn_blocking(move || {
        if file.as_os_str() == "-" {
            std::io::read_to_string(std::io::stdin()).map_err(|e| format!("stdin: {e}"))
        } else {
            std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

const ACCOUNT_COLUMNS: &[&str] = &["ИДЕНТИФИКАТОР", "ИМЯ ВХОДА", "ИМЯ", "ВХОД"];

const DOMAIN_COLUMNS: &[&str] = &["ИДЕНТИФИКАТОР", "ДОМЕН", "ВИД"];

/// Текущий администратор — строки «подпись: значение».
fn whoami_record(w: &WhoAmI) -> String {
    record(&[
        ("Администратор", w.login.clone()),
        ("Организация", w.tenant.clone()),
        ("Учётная запись", w.account.clone()),
        ("Роли", roles(&w.roles)),
    ])
}

/// Учётная запись — строки «подпись: значение».
fn account_record(a: &AccountInfo) -> String {
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
fn account_row(a: &AccountInfo) -> Vec<String> {
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
            let w: WhoAmI = client.call(WHOAMI, &[], &[], NO_BODY).await?;
            w.tenant
        }
    };
    org.parse::<TenantId>()
        .map_err(|e| format!("организация {org:?}: {e}"))?;
    Ok(org)
}

/// Учётные записи с именем входа `login`.
async fn find_accounts(client: &Client, org: &str, login: String) -> CliResult<Vec<AccountInfo>> {
    let query = query(&AccountQuery {
        login: Some(login),
        ..AccountQuery::default()
    })?;
    client.call(ACCOUNT_FIND, &[org], &query, NO_BODY).await
}

/// Список учётных записей: страницы Admin API по ссылке `rel="next"`, не больше `limit` записей (`None` — все);
/// второе значение — остались записи сверх `limit`.
async fn list_accounts(
    client: &Client,
    org: &str,
    prefix: Option<String>,
    limit: Option<usize>,
) -> CliResult<(Vec<AccountInfo>, bool)> {
    let mut query = query(&AccountQuery {
        prefix,
        limit,
        ..AccountQuery::default()
    })?;
    let mut out = Vec::new();
    loop {
        let (page, next): (Vec<AccountInfo>, _) = client
            .call_page(ACCOUNT_FIND, &[org], &query, NO_BODY)
            .await?;
        let empty = page.is_empty();
        out.extend(page);
        if let Some(limit) = limit.filter(|l| out.len() >= *l) {
            let more = out.len() > limit || next.is_some();
            out.truncate(limit);
            return Ok((out, more));
        }
        match next {
            // Пустая страница со ссылкой дальше — ошибка узла; без проверки — бесконечный цикл.
            Some(_) if empty => {
                return Err(format!(
                    "{} {}: пустая страница со ссылкой rel=\"next\"",
                    ACCOUNT_FIND.method, ACCOUNT_FIND.path
                ));
            }
            Some(link) => query = client.link_query(&link)?,
            None => return Ok((out, false)),
        }
    }
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
async fn domains(client: &Client, org: &str) -> CliResult<DomainList> {
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

    use serde_json::{Value, json};
    use tp_admin::{AdminConfig, AdminRole, Deps, InitRequest, RoleBinding};
    use tp_contracts::auth::Principal;
    use tp_contracts::ext::DecisionHooks;
    use tp_testkit::{
        Clock, FakeAuthenticator, MemAppPasswords, MemAuditLog, MemBlobIndex, MemBlobStore,
        MemDeliveryLog, MemEventBus, MemGroups, MemLocalAdminTokens, MemMetadata, MemModeration,
        MemPolicies, MemQuota, MemSieve, MemTransport, MemTrash, SystemClock,
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
            &["thepost", "admin", "sieve", "list", "a@b.test"],
            &["thepost", "admin", "sieve", "get", "a@b.test", "main"],
            &[
                "thepost", "admin", "sieve", "put", "a@b.test", "main", "--file", "-",
            ],
            &["thepost", "admin", "sieve", "activate", "a@b.test", "main"],
            &["thepost", "admin", "sieve", "deactivate", "a@b.test"],
            &["thepost", "admin", "sieve", "delete", "a@b.test", "main"],
            &["thepost", "admin", "sieve", "org", "get"],
            &["thepost", "admin", "sieve", "org", "put", "--file", "-"],
            &["thepost", "admin", "sieve", "org", "delete", "--json"],
            &["thepost", "admin", "vacation", "get", "a@b.test"],
            &["thepost", "admin", "vacation", "set", "a@b.test", "--off"],
            &[
                "thepost",
                "admin",
                "vacation",
                "set",
                "a@b.test",
                "--from",
                "2026-12-01T00:00:00Z",
                "--to",
                "-",
                "--subject",
                "Отпуск",
                "--text-file",
                "-",
                "--html-file",
                "a.html",
            ],
            &["thepost", "admin", "account", "block", "a@b.test"],
            &["thepost", "admin", "account", "unblock", "a@b.test"],
            &["thepost", "admin", "account", "rename", "a@b.test", "Анна"],
            &["thepost", "admin", "account", "list", "--login", "a@b.test"],
            &["thepost", "admin", "account", "list"],
            &[
                "thepost", "admin", "account", "list", "--prefix", "a", "--limit", "10",
            ],
            &["thepost", "admin", "init", "--email", "a@b.test"],
        ] {
            Cli::try_parse_from(args).unwrap_or_else(|e| panic!("{args:?}: {e}"));
        }
        for args in [
            &[
                "thepost", "admin", "account", "list", "--login", "a@b.test", "--prefix", "a",
            ][..],
            &[
                "thepost", "admin", "account", "list", "--login", "a@b.test", "--limit", "1",
            ],
            &["thepost", "admin", "account", "list", "--limit", "0"],
            &["thepost", "admin", "sieve", "put", "a@b.test", "main"],
            &["thepost", "admin", "sieve", "activate", "a@b.test"],
            &["thepost", "admin", "sieve", "delete", "a@b.test"],
            &["thepost", "admin", "sieve", "org", "put"],
            &["thepost", "admin", "vacation", "set"],
            &[
                "thepost",
                "admin",
                "vacation",
                "set",
                "a@b.test",
                "--text",
                "а",
                "--text-file",
                "-",
            ],
            &["thepost", "admin", "account", "rename", "a@b.test"],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
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
        let w = WhoAmI {
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

    /// Параметры запроса — из структур запроса tp-admin; `null` пропускается.
    #[test]
    fn query_from_tp_admin_structs() {
        let sorted = |mut v: Vec<(String, String)>| {
            v.sort();
            v
        };
        let pair = |k: &str, v: &str| (k.to_owned(), v.to_owned());
        assert_eq!(
            query(&AccountQuery {
                login: Some("a@b.test".into()),
                ..AccountQuery::default()
            }),
            Ok(vec![pair("login", "a@b.test")])
        );
        assert_eq!(
            query(&AccountQuery {
                login: None,
                prefix: Some("a".into()),
                after: Some("a@b.test".into()),
                limit: Some(10),
            })
            .map(sorted),
            Ok(vec![
                pair("after", "a@b.test"),
                pair("limit", "10"),
                pair("prefix", "a")
            ])
        );
        assert_eq!(
            query(&VersionQuery { version: 3 }),
            Ok(vec![pair("version", "3")])
        );
    }

    /// Ссылка `rel="next"` (RFC 8288): среди нескольких ссылок, `rel` с несколькими значениями, без кавычек.
    #[test]
    fn next_link_header() {
        let headers = |values: &[&str]| {
            let mut h = HeaderMap::new();
            for v in values {
                h.append(LINK, v.parse().expect("header"));
            }
            h
        };
        let next = "/api/v1/orgs/x/accounts?limit=2&after=b@b.test";
        assert_eq!(
            next_link(&headers(&[format!("<{next}>; rel=\"next\"").as_str()])),
            Some(next.to_owned())
        );
        assert_eq!(
            next_link(&headers(&[
                "</first>; rel=\"first\"",
                format!("<{next}>; title=\"x\"; rel=\"last next\"").as_str()
            ])),
            Some(next.to_owned())
        );
        assert_eq!(
            next_link(&headers(&["</a>; rel=prev, </b>; rel=next"])),
            Some("/b".to_owned())
        );
        assert_eq!(next_link(&headers(&["</a>; rel=\"prev\""])), None);
        assert_eq!(next_link(&HeaderMap::new()), None);
    }

    /// Admin API tp-admin на хранилищах testkit на loopback: локальный вход подключён, `oidc` — токен OIDC
    /// администратора инсталляции; страница списков — 2 записи (переход по `Link` в каждом списке длиннее).
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
            audit_log: Arc::new(MemAuditLog::new()),
            delivery: Arc::new(MemDeliveryLog::new()),
            groups: Arc::new(MemGroups::new()),
            moderation: Arc::new(MemModeration::default()),
            modules: Vec::new(),
            accounts: Arc::new(meta.clone()),
            quota: Arc::new(quota),
            policies: Arc::new(policies.clone()),
            resolver: Arc::new(policies),
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
            page_size: 2,
            max_page_size: 2,
            ..AdminConfig::default()
        };
        let admin = Arc::new(
            tp_admin::Admin::new(deps, cfg)
                .expect("admin")
                .with_transport(Arc::new(MemTransport::new()))
                .with_sieve(Arc::new(MemSieve::new()), check_script)
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

    /// Проверка скрипта, как у узла: разбор tp-sieve.
    fn check_script(text: &str) -> Vec<tp_admin::ScriptError> {
        tp_sieve::check(text)
            .err()
            .unwrap_or_default()
            .into_iter()
            .map(|d| tp_admin::ScriptError {
                line: d.line,
                column: d.column,
                message: d.message,
            })
            .collect()
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

    /// Имена входа `account list --json` без `--login`.
    async fn list_logins(c: &Client, prefix: Option<&str>, limit: Option<usize>) -> Vec<String> {
        let action = AccountAction::List {
            login: None,
            prefix: prefix.map(str::to_owned),
            limit,
        };
        let json = exec(c, true, ApiAction::Account { action })
            .await
            .expect("account list");
        let v: Value = serde_json::from_str(&json).expect("json");
        v.as_array()
            .expect("array")
            .iter()
            .map(|a| a["login"].as_str().expect("login").to_owned())
            .collect()
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
                    login: Some("A@second.test".into()),
                    prefix: None,
                    limit: None,
                },
            },
        )
        .await
        .expect("account list");
        assert_eq!(listed.lines().count(), 2, "{listed}");

        // Список без --login: страницы по 2 записи по ссылке Link, префикс, предел.
        for login in ["c@second.test", "B@second.test"] {
            let action = AccountAction::Create {
                login: login.into(),
                name: None,
            };
            exec(&s, false, ApiAction::Account { action })
                .await
                .expect("account create");
        }
        assert_eq!(
            list_logins(&s, None, None).await,
            [
                "a@second.test",
                "admin@example.test",
                "b@second.test",
                "c@second.test"
            ]
        );
        assert_eq!(
            list_logins(&s, Some("A"), None).await,
            ["a@second.test", "admin@example.test"]
        );
        assert_eq!(
            list_logins(&s, None, Some(3)).await,
            ["a@second.test", "admin@example.test", "b@second.test"]
        );
        let text = exec(
            &s,
            false,
            ApiAction::Account {
                action: AccountAction::List {
                    login: None,
                    prefix: None,
                    limit: Some(3),
                },
            },
        )
        .await
        .expect("limit text");
        assert_eq!(text.lines().count(), 5, "{text}");
        assert!(text.contains("есть ещё"), "{text}");
        let text = exec(
            &s,
            false,
            ApiAction::Account {
                action: AccountAction::List {
                    login: None,
                    prefix: Some("c".into()),
                    limit: None,
                },
            },
        )
        .await
        .expect("prefix text");
        assert!(
            text.contains("c@second.test") && !text.contains("есть ещё"),
            "{text}"
        );
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

    /// ADM-17 (решение 62): администратор из CLI записывает пользователю правило пересылки, включает, отключает;
    /// неверный текст — отказ с позицией ошибки, в хранилище не попадает.
    #[tokio::test]
    async fn sieve_commands_against_admin_api() {
        let (_admin, url) = admin_api().await;
        let c = client(&url, "oidc");
        let sieve = |action| ApiAction::Sieve { action };
        let account = || "a@rules.test".to_owned();
        let name = || "пересылка".to_owned();
        for action in [
            ApiAction::Domain {
                action: DomainAction::Add {
                    name: "rules.test".into(),
                    relay: None,
                },
            },
            ApiAction::Account {
                action: AccountAction::Create {
                    login: account(),
                    name: None,
                },
            },
        ] {
            exec(&c, false, action).await.expect("подготовка");
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("rule.sieve");
        let text = "redirect \"outside@example.net\";\n";
        std::fs::write(&file, text).expect("write");
        let put = |file: &Path| {
            sieve(SieveAction::Put {
                account: account(),
                name: name(),
                file: file.to_owned(),
            })
        };
        let done = exec(&c, false, put(&file)).await.expect("put");
        assert_eq!(done, "Скрипт пересылка записан.\n");
        let on = sieve(SieveAction::Activate {
            account: account(),
            name: name(),
        });
        exec(&c, false, on).await.expect("activate");

        let list = || sieve(SieveAction::List { account: account() });
        let listed = exec(&c, false, list()).await.expect("list");
        let row = listed.lines().nth(1).expect("строка");
        assert!(row.contains("пересылка") && row.ends_with("да"), "{listed}");
        let get = || {
            sieve(SieveAction::Get {
                account: account(),
                name: name(),
            })
        };
        assert_eq!(exec(&c, false, get()).await.as_deref(), Ok(text));

        // Неверный текст: 422 с позицией, прежний текст на месте.
        let bad = dir.path().join("bad.sieve");
        std::fs::write(&bad, "keep;\nнеизвестно;\n").expect("write");
        let err = exec(&c, false, put(&bad)).await.expect_err("422");
        assert!(err.contains("422") && err.contains("строка 2"), "{err}");
        let missing = dir.path().join("нет.sieve");
        let err = exec(&c, false, put(&missing)).await.expect_err("нет файла");
        assert!(err.contains("нет.sieve"), "{err}");
        assert_eq!(exec(&c, false, get()).await.as_deref(), Ok(text));

        // Отключение: скрипт остаётся, активного нет.
        let off = sieve(SieveAction::Deactivate { account: account() });
        let done = exec(&c, false, off).await.expect("deactivate");
        assert_eq!(done, "Правила отключены.\n");
        let json = exec(&c, true, list()).await.expect("list");
        let v: Value = serde_json::from_str(&json).expect("json");
        assert_eq!(v[0]["name"], "пересылка");
        assert_eq!(v[0]["active"], false);
        let on = sieve(SieveAction::Activate {
            account: account(),
            name: "другой".into(),
        });
        let err = exec(&c, false, on).await.expect_err("404");
        assert!(err.contains("404"), "{err}");

        let bad_file = bad.clone();
        // Удаление: активный скрипт — 409, после отключения удаляется; повторно — 404.
        let on = sieve(SieveAction::Activate {
            account: account(),
            name: name(),
        });
        exec(&c, false, on).await.expect("activate");
        let delete = || {
            sieve(SieveAction::Delete {
                account: account(),
                name: name(),
            })
        };
        let err = exec(&c, false, delete()).await.expect_err("409");
        assert!(err.contains("409"), "{err}");
        let off = sieve(SieveAction::Deactivate { account: account() });
        exec(&c, false, off).await.expect("deactivate");
        let done = exec(&c, false, delete()).await.expect("delete");
        assert_eq!(done, "Скрипт пересылка удалён.\n");
        let json = exec(&c, true, list()).await.expect("list");
        assert_eq!(serde_json::from_str::<Value>(&json).ok(), Some(json!([])));
        let err = exec(&c, false, delete()).await.expect_err("404");
        assert!(err.contains("404"), "{err}");

        // Автоответ: не задан — выключен; включение, выключение с сохранением текста, сброс срока.
        let vacation = |action| ApiAction::Vacation {
            action: Box::new(action),
        };
        let get = || vacation(VacationAction::Get { account: account() });
        let set = || VacationChange {
            enabled: true,
            from: None,
            to: None,
            subject: None,
            text: None,
            html: None,
        };
        let apply = |off: bool, ch: VacationChange| {
            vacation(VacationAction::Set {
                account: account(),
                off,
                from: ch.from,
                to: ch.to,
                subject: ch.subject,
                text: ch.text,
                text_file: None,
                html_file: None,
            })
        };
        let shown = exec(&c, false, get()).await.expect("vacation get");
        assert!(shown.starts_with("Включён"), "{shown}");
        assert!(shown.lines().next().is_some_and(|l| l.ends_with("нет")));
        let on = apply(
            false,
            VacationChange {
                from: Some("2026-12-01T00:00:00Z".into()),
                to: Some("2026-12-15T00:00:00+03:00".into()),
                subject: Some("В отпуске".into()),
                text: Some("Вернусь 15 декабря.".into()),
                ..set()
            },
        );
        let v: Value = serde_json::from_str(&exec(&c, true, on).await.expect("set")).expect("json");
        assert_eq!(v["enabled"], true);
        assert_eq!(v["subject"], "В отпуске");
        assert_eq!(v["text_body"], "Вернусь 15 декабря.");
        assert_eq!(v["from"], "2026-12-01T00:00:00Z");
        let shown = exec(&c, false, get()).await.expect("vacation get");
        assert!(
            shown.contains("В отпуске") && shown.contains("2026-12-01"),
            "{shown}"
        );
        let off = exec(&c, true, apply(true, set())).await.expect("off");
        let v: Value = serde_json::from_str(&off).expect("json");
        assert_eq!(v["enabled"], false);
        assert_eq!(v["text_body"], "Вернусь 15 декабря.");
        assert_eq!(v["from"], "2026-12-01T00:00:00Z");
        let reset = apply(
            false,
            VacationChange {
                from: Some("-".into()),
                to: Some("-".into()),
                subject: Some("-".into()),
                ..set()
            },
        );
        let v: Value =
            serde_json::from_str(&exec(&c, true, reset).await.expect("reset")).expect("json");
        assert_eq!(v["enabled"], true);
        assert_eq!(
            (&v["from"], &v["to"], &v["subject"]),
            (&json!(null), &json!(null), &json!(null))
        );
        let bad = apply(
            false,
            VacationChange {
                from: Some("завтра".into()),
                ..set()
            },
        );
        let err = exec(&c, false, bad).await.expect_err("время");
        assert!(err.contains("RFC 3339"), "{err}");
        // Текст из файла.
        let reply = dir.path().join("reply.txt");
        std::fs::write(&reply, "Из файла.\n").expect("write");
        let from_file = vacation(VacationAction::Set {
            account: account(),
            off: false,
            from: None,
            to: None,
            subject: None,
            text: None,
            text_file: Some(reply),
            html_file: None,
        });
        let v: Value =
            serde_json::from_str(&exec(&c, true, from_file).await.expect("file")).expect("json");
        assert_eq!(v["text_body"], "Из файла.\n");

        // Скрипт организации: не задан — 404; запись, чтение, 422 с позицией, удаление (повторное — не ошибка).
        let org = |action| sieve(SieveAction::Org { action });
        let err = exec(&c, false, org(OrgSieveAction::Get))
            .await
            .expect_err("404");
        assert!(err.contains("404"), "{err}");
        let rule = dir.path().join("org.sieve");
        let org_text = "if address :is \"from\" \"spam@example.net\" { discard; stop; }\n";
        std::fs::write(&rule, org_text).expect("write");
        let put = |file: &Path| {
            org(OrgSieveAction::Put {
                file: file.to_owned(),
            })
        };
        let done = exec(&c, false, put(&rule)).await.expect("org put");
        assert_eq!(done, "Скрипт организации записан.\n");
        assert_eq!(
            exec(&c, false, org(OrgSieveAction::Get)).await.as_deref(),
            Ok(org_text)
        );
        let err = exec(&c, false, put(&bad_file)).await.expect_err("422");
        assert!(err.contains("422") && err.contains("строка 2"), "{err}");
        assert_eq!(
            exec(&c, false, org(OrgSieveAction::Get)).await.as_deref(),
            Ok(org_text)
        );
        for _ in 0..2 {
            let done = exec(&c, false, org(OrgSieveAction::Delete))
                .await
                .expect("org delete");
            assert_eq!(done, "Скрипт организации удалён.\n");
        }
        let err = exec(&c, false, org(OrgSieveAction::Get))
            .await
            .expect_err("404");
        assert!(err.contains("404"), "{err}");
    }

    #[test]
    fn org_put_help_explains_order() {
        use clap::CommandFactory;

        let mut cmd = crate::cli::Cli::command();
        let mut cmd = &mut cmd;
        for name in ["admin", "sieve", "org", "put"] {
            cmd = cmd.find_subcommand_mut(name).expect(name);
        }
        let help = cmd.render_long_help().to_string();
        assert!(
            help.contains("discard; stop;") && help.contains("подряд"),
            "{help}"
        );
    }

    #[tokio::test]
    async fn account_block_against_admin_api() {
        let (_admin, url) = admin_api().await;
        let c = client(&url, "oidc");
        let login = || "a@block.test".to_owned();
        let account = |action| ApiAction::Account { action };
        for action in [
            ApiAction::Domain {
                action: DomainAction::Add {
                    name: "block.test".into(),
                    relay: None,
                },
            },
            account(AccountAction::Create {
                login: login(),
                name: None,
            }),
        ] {
            exec(&c, false, action).await.expect("подготовка");
        }
        let enabled = |out: CliResult<String>| {
            let v: Value = serde_json::from_str(&out.expect("ответ")).expect("json");
            (v["enabled"].clone(), v["display_name"].clone())
        };
        let block = account(AccountAction::Block { account: login() });
        assert_eq!(enabled(exec(&c, true, block).await).0, false);
        let get = || account(AccountAction::Get { account: login() });
        let shown = exec(&c, false, get()).await.expect("get");
        assert!(shown.contains("false"), "{shown}");
        let unblock = account(AccountAction::Unblock { account: login() });
        assert_eq!(enabled(exec(&c, true, unblock).await).0, true);
        let rename = account(AccountAction::Rename {
            account: login(),
            name: "Анна".into(),
        });
        assert_eq!(
            enabled(exec(&c, true, rename).await),
            (json!(true), json!("Анна"))
        );
        let rename = account(AccountAction::Rename {
            account: login(),
            name: String::new(),
        });
        let err = exec(&c, false, rename).await.expect_err("400");
        assert!(err.contains("400"), "{err}");
        let missing = account(AccountAction::Block {
            account: "нет@block.test".into(),
        });
        assert!(exec(&c, false, missing).await.is_err());
    }
}
