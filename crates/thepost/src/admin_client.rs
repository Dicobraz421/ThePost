//! `thepost admin <ресурс> <действие>` — клиент Admin API (APP-11, SRS 5.2): интерфейс без собственной логики.
//!
//! - Адрес — `--url`, иначе `THEPOST_ADMIN_URL`, иначе `http.admin.listen` из config.yaml (`allow_plaintext` — `http`,
//!   иначе `https`; сертификат на другое имя — `--ca-file` или `--insecure`).
//! - Вход — `Authorization: Bearer`: сессия Admin API (`tps_…`) или токен OIDC; значение — `THEPOST_ADMIN_TOKEN`
//!   (или `--token`, но ключ виден в списке процессов). Одноразовый токен `admin init` (`tpa_…`) обменивает на сессию
//!   [`ApiAction::Login`] (`POST /api/v1/session`); для остальных запросов он не принимается и узлом.
//! - Организация — `--org`, иначе `THEPOST_ADMIN_ORG`, иначе организация администратора (`GET /api/v1/whoami`).
//! - Вывод — таблица или ответ API как есть (`--json`).
//!
//! Операции клиента — [`OPERATIONS`]: тест сверяет каждую со спецификацией OpenAPI tp-admin
//! (`tp_admin::server::openapi_spec`), команды проверяются против роутера tp-admin. Генератор клиента (progenitor)
//! не используется: спецификация — OpenAPI 3.1, progenitor читает только 3.0.

use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand};
use reqwest::header::CONTENT_TYPE;
use reqwest::{Method, StatusCode, Url};
use serde_json::{Value, json};
use tp_contracts::admin_token::is_local_admin_token;
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

/// Результат команды: текст для stdout или сообщение об ошибке.
type CliResult<T> = std::result::Result<T, String>;

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
    fn url(&self, op: Op, params: &[&str], query: &[(&str, &str)]) -> CliResult<Url> {
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

    /// Запрос `op`; ответ 2xx — тело JSON (`null` — пустое), иначе — ошибка с кодом и причиной из problem details.
    pub async fn call(
        &self,
        op: Op,
        params: &[&str],
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> CliResult<Value> {
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
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        if status.is_success() {
            Ok(value)
        } else {
            Err(problem(op, status, &value))
        }
    }
}

/// Сообщение об отказе: код, описание и подробность ответа (RFC 9457), подсказка для 401.
fn problem(op: Op, status: StatusCode, body: &Value) -> String {
    let title = body["title"]
        .as_str()
        .or_else(|| status.canonical_reason())
        .unwrap_or_default();
    let mut msg = format!("{} {}: {} {title}", op.method, op.path, status.as_u16());
    if let Some(detail) = body["detail"].as_str() {
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

/// Выполняет команду через `client`; текст для stdout.
pub async fn execute(client: &Client, args: &ApiArgs, action: ApiAction) -> CliResult<String> {
    let out = |v: &Value, text: String| -> CliResult<String> {
        if args.json {
            serde_json::to_string_pretty(v)
                .map(|j| format!("{j}\n"))
                .map_err(|e| e.to_string())
        } else {
            Ok(text)
        }
    };
    match action {
        ApiAction::Login => {
            let v = client.call(SESSION, &[], &[], None).await?;
            let text = format!(
                "{}Значение сессии показывается один раз. Дальше: export {ENV_TOKEN}=<сессия>\n",
                record(
                    &v,
                    &[
                        ("Сессия", "session"),
                        ("Действует до", "expires_at"),
                        ("Администратор", "login"),
                        ("Организация", "tenant"),
                        ("Роли", "roles"),
                    ],
                )
            );
            out(&v, text)
        }
        ApiAction::Whoami => {
            let v = client.call(WHOAMI, &[], &[], None).await?;
            out(&v, record(&v, WHOAMI_FIELDS))
        }
        ApiAction::Account { action } => {
            let org = org(client, args).await?;
            match action {
                AccountAction::Create { login, name } => {
                    let mut body = json!({ "login": login });
                    if let Some(name) = name {
                        body["display_name"] = Value::String(name);
                    }
                    let v = client
                        .call(ACCOUNT_CREATE, &[org.as_str()], &[], Some(&body))
                        .await?;
                    out(&v, record(&v, ACCOUNT_FIELDS))
                }
                AccountAction::List { login } => {
                    let v = client
                        .call(
                            ACCOUNT_FIND,
                            &[org.as_str()],
                            &[("login", login.as_str())],
                            None,
                        )
                        .await?;
                    out(&v, table(&v, ACCOUNT_COLUMNS))
                }
                AccountAction::Get { account } => {
                    let id = account_id(client, &org, &account).await?;
                    let v = client
                        .call(ACCOUNT_GET, &[org.as_str(), id.as_str()], &[], None)
                        .await?;
                    out(&v, record(&v, ACCOUNT_FIELDS))
                }
            }
        }
        ApiAction::AppPassword { action } => {
            let org = org(client, args).await?;
            match action {
                AppPasswordAction::Create { account, label } => {
                    let id = account_id(client, &org, &account).await?;
                    let body = json!({ "label": label });
                    let v = client
                        .call(
                            APP_PASSWORD_CREATE,
                            &[org.as_str(), id.as_str()],
                            &[],
                            Some(&body),
                        )
                        .await?;
                    let text = format!(
                        "{}Пароль показывается один раз.\n",
                        record(
                            &v,
                            &[
                                ("Пароль", "password"),
                                ("Идентификатор", "id"),
                                ("Подпись", "label"),
                                ("Выпущен", "created_at"),
                            ],
                        )
                    );
                    out(&v, text)
                }
                AppPasswordAction::List { account } => {
                    let id = account_id(client, &org, &account).await?;
                    let v = client
                        .call(APP_PASSWORD_LIST, &[org.as_str(), id.as_str()], &[], None)
                        .await?;
                    out(
                        &v,
                        table(
                            &v,
                            &[
                                ("ИДЕНТИФИКАТОР", "id"),
                                ("ПОДПИСЬ", "label"),
                                ("ВЫПУЩЕН", "created_at"),
                                ("ОТОЗВАН", "revoked_at"),
                            ],
                        ),
                    )
                }
                AppPasswordAction::Revoke { account, id } => {
                    let account = account_id(client, &org, &account).await?;
                    let v = client
                        .call(
                            APP_PASSWORD_REVOKE,
                            &[org.as_str(), account.as_str(), id.as_str()],
                            &[],
                            None,
                        )
                        .await?;
                    out(&v, format!("Пароль {id} отозван.\n"))
                }
            }
        }
        ApiAction::Domain { action } => {
            let org = org(client, args).await?;
            match action {
                DomainAction::List => {
                    let v = client.call(DOMAIN_LIST, &[org.as_str()], &[], None).await?;
                    let text = format!(
                        "{}Версия снимка: {}\n",
                        table(&v["domains"], DOMAIN_COLUMNS),
                        field(&v, "version")
                    );
                    out(&v, text)
                }
                DomainAction::Add { name, relay } => {
                    let version = domain_version(client, &org).await?;
                    let kind = match relay {
                        Some(next_hop) => json!({ "internal_relay": { "next_hop": next_hop } }),
                        None => json!("authoritative"),
                    };
                    let body = json!({ "version": version, "name": name, "kind": kind });
                    let v = client
                        .call(DOMAIN_ADD, &[org.as_str()], &[], Some(&body))
                        .await?;
                    let text = format!(
                        "{}Версия снимка: {}\n",
                        record(&v["domain"], DOMAIN_COLUMNS),
                        field(&v, "version")
                    );
                    out(&v, text)
                }
                DomainAction::Remove { id } => {
                    let version = domain_version(client, &org).await?.to_string();
                    let v = client
                        .call(
                            DOMAIN_REMOVE,
                            &[org.as_str(), id.as_str()],
                            &[("version", version.as_str())],
                            None,
                        )
                        .await?;
                    out(
                        &v,
                        format!(
                            "Домен {id} удалён. Версия снимка: {}\n",
                            field(&v, "version")
                        ),
                    )
                }
            }
        }
    }
}

const WHOAMI_FIELDS: &[(&str, &str)] = &[
    ("Администратор", "login"),
    ("Организация", "tenant"),
    ("Учётная запись", "account"),
    ("Роли", "roles"),
];

const ACCOUNT_FIELDS: &[(&str, &str)] = &[
    ("Идентификатор", "id"),
    ("Имя входа", "login"),
    ("Имя", "display_name"),
    ("Вход разрешён", "enabled"),
    ("Домен", "domain"),
    ("Занято, байт", "quota.used_bytes"),
    ("Писем", "quota.messages"),
    ("Квота, байт", "quota.limit_bytes"),
];

const ACCOUNT_COLUMNS: &[(&str, &str)] = &[
    ("ИДЕНТИФИКАТОР", "id"),
    ("ИМЯ ВХОДА", "login"),
    ("ИМЯ", "display_name"),
    ("ВХОД", "enabled"),
];

const DOMAIN_COLUMNS: &[(&str, &str)] =
    &[("ИДЕНТИФИКАТОР", "id"), ("ДОМЕН", "name"), ("ВИД", "kind")];

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
        None => client.call(WHOAMI, &[], &[], None).await?["tenant"]
            .as_str()
            .ok_or("whoami: в ответе нет организации")?
            .to_owned(),
    };
    org.parse::<TenantId>()
        .map_err(|e| format!("организация {org:?}: {e}"))?;
    Ok(org)
}

/// Идентификатор учётной записи: UUID как есть, иначе поиск по имени входа.
async fn account_id(client: &Client, org: &str, account: &str) -> CliResult<String> {
    if account.parse::<AccountId>().is_ok() {
        return Ok(account.to_owned());
    }
    let found = client
        .call(ACCOUNT_FIND, &[org], &[("login", account)], None)
        .await?;
    found
        .as_array()
        .and_then(|a| a.first())
        .and_then(|a| a["id"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| format!("учётная запись {account} не найдена"))
}

/// Версия снимка кластера для записи доменов (из последнего чтения).
async fn domain_version(client: &Client, org: &str) -> CliResult<u64> {
    client.call(DOMAIN_LIST, &[org], &[], None).await?["version"]
        .as_u64()
        .ok_or_else(|| "список доменов: в ответе нет версии снимка".to_owned())
}

/// Значение поля по пути через точку: строка как есть, `null` и нет поля — «—», массив строк — через запятую.
fn field(v: &Value, path: &str) -> String {
    let mut cur = v;
    for key in path.split('.') {
        cur = &cur[key];
    }
    match cur {
        Value::Null => "—".to_owned(),
        Value::String(s) => s.clone(),
        Value::Array(items) if items.iter().all(Value::is_string) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string(),
    }
}

/// Объект — строки «подпись: значение».
fn record(v: &Value, fields: &[(&str, &str)]) -> String {
    let width = fields
        .iter()
        .map(|(t, _)| t.chars().count())
        .max()
        .unwrap_or(0);
    fields
        .iter()
        .map(|(title, path)| {
            format!(
                "{}:{} {}\n",
                title,
                pad("", width - title.chars().count()),
                field(v, path)
            )
        })
        .collect()
}

/// Массив объектов — таблица с заголовком; пустой — «нет записей».
fn table(v: &Value, cols: &[(&str, &str)]) -> String {
    let rows: Vec<Vec<String>> = v
        .as_array()
        .map(|a| {
            a.iter()
                .map(|r| cols.iter().map(|(_, p)| field(r, p)).collect())
                .collect()
        })
        .unwrap_or_default();
    if rows.is_empty() {
        return "нет записей\n".to_owned();
    }
    let widths: Vec<usize> = cols
        .iter()
        .enumerate()
        .map(|(i, (t, _))| {
            rows.iter()
                .map(|r| r[i].chars().count())
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
    let mut out = line(cols.iter().map(|(t, _)| *t).collect());
    for r in &rows {
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
        let rows = json!([
            {"id": "1", "login": "a@b.test", "display_name": "Анна", "enabled": true},
            {"id": "22", "login": "bb@b.test", "display_name": null, "enabled": false},
        ]);
        let t = table(&rows, ACCOUNT_COLUMNS);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines.len(), 3, "{t}");
        assert!(lines[0].starts_with("ИДЕНТИФИКАТОР  ИМЯ ВХОДА"), "{t}");
        assert!(
            lines[2].contains("bb@b.test") && lines[2].contains('—'),
            "{t}"
        );
        assert_eq!(table(&json!([]), ACCOUNT_COLUMNS), "нет записей\n");
        let r = record(&json!({"login": "a", "roles": ["x", "y"]}), WHOAMI_FIELDS);
        assert!(r.contains("Роли:") && r.contains("x, y"), "{r}");
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
