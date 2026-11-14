//! `thepost admin <ресурс> <действие>` — CLI Admin API (APP-11, SRS 5.2): интерфейс без собственной логики поверх
//! общего клиента [`tp_admin_client`] (решение 118, ADM-44).
//!
//! - Адрес — `--url`, иначе `THEPOST_ADMIN_URL`, иначе `http.admin.listen` из config.yaml (`allow_plaintext` — `http`,
//!   иначе `https`; сертификат на другое имя — `--ca-file` или `--insecure`).
//! - Вход — `Authorization: Bearer`: сессия Admin API (`tps_…`) или токен OIDC; значение — `THEPOST_ADMIN_TOKEN`
//!   (или `--token`, но ключ виден в списке процессов). Сессию выдаёт [`ApiAction::Login`] — вход по паролю
//!   администратора (`POST /api/v1/session`, решение 117): пароль — из терминала без эха или `--password-stdin`,
//!   не из аргумента; `--oidc` — вход у поставщика `auth.oidc` с устройства (RFC 8628, клиент — `cli.oidc`).
//!   Одноразовый токен `admin init` (`tpa_…`) клиент не принимает.
//! - Организация — `--org`, иначе `THEPOST_ADMIN_ORG`, иначе организация администратора (`GET /api/v1/whoami`).
//! - Вывод — таблица или ответ API (`--json`); и то и другое — из типизированного ответа tp-admin.
//!
//! Команды проверяются против роутера tp-admin.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::{Args, Subcommand};
use reqwest::Url;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tp_admin::AdminRole;
use tp_admin::accounts::{
    AccountInfo, AccountPatch, AccountQuery, NewAccount, Organization, Policies, PolicyBody,
};
use tp_admin::app_passwords::{AppPasswordInfo, IssuedAppPassword, NewAppPassword};
use tp_admin::audit::{AuditPage, AuditParams};
use tp_admin::delivery::{DeliveryParams, DeliveryResults};
use tp_admin::groups::{GroupInfo, GroupPatch, NewGroup};
use tp_admin::retention::{PageQuery, RestoredMessage, TrashPage};
use tp_admin::server::WhoAmI;
use tp_admin::sieve::{Script, ScriptBody, ScriptText, VacationBody};
use tp_admin::transport::{
    DomainBody, DomainList, DomainWritten, RouteBody, RouteList, SourceBody, SourceList,
    VersionQuery, Written,
};
use tp_admin_client::oidc::{DeviceLogin, DeviceSettings, prompt_text};
use tp_admin_client::ops::*;
use tp_admin_client::{
    Client, DEFAULT_TIMEOUT_SECS, ENV_TOKEN, ENV_URL, NO_BODY, Options, query, setting,
};
use tp_contracts::groups::{GroupMember, SenderRestriction};
use tp_contracts::transport::{
    AcceptedDomain, DestinationPolicy, DomainKind, SourceKind, SourceMatch, TlsRequirement,
    TrustedSource,
};
use tp_contracts::{AccountId, DomainId, GroupId};

use crate::sections::{self, AUTH, AuthSection, CLI, CliOidc, CliSection, HTTP, HttpSection};

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

impl ApiArgs {
    /// Канал к Admin API по `--ca-file`, `--insecure` (с предупреждением в stderr) и `--timeout`.
    fn options(&self) -> Options {
        if self.insecure {
            eprintln!("thepost: admin: сертификат TLS Admin API не проверяется (--insecure)");
        }
        Options {
            ca_file: self.ca_file.clone(),
            insecure: self.insecure,
            timeout: Duration::from_secs(self.timeout),
        }
    }
}

/// Команды Admin API.
#[derive(Debug, Clone, Subcommand)]
pub enum ApiAction {
    /// Вход администратора по паролю — сессия Admin API (tps_…); пароль — из терминала без эха или --password-stdin.
    /// --oidc — вход у поставщика OIDC с устройства.
    Login {
        /// Имя входа администратора (адрес; пароль — каталога или выданный thepost admin init).
        #[arg(long, required_unless_present = "oidc", conflicts_with = "oidc")]
        user: Option<String>,
        /// Пароль — первой строкой stdin (сценарии), а не из терминала.
        #[arg(long, conflicts_with = "oidc")]
        password_stdin: bool,
        /// Вход у поставщика auth.oidc с устройства (RFC 8628, клиент — cli.oidc): адрес и код — в stderr, токен
        /// OIDC — строкой в stdout, без записи в историю оболочки:
        /// export THEPOST_ADMIN_TOKEN="$(thepost admin login --oidc)".
        #[arg(long)]
        oidc: bool,
    },
    /// Текущий администратор и его роли.
    Whoami,
    /// Учётные записи.
    Account {
        /// Действие.
        #[command(subcommand)]
        action: Box<AccountAction>,
    },
    /// Пароли приложений учётной записи (IMAP, SMTP, JMAP).
    AppPassword {
        /// Действие.
        #[command(subcommand)]
        action: AppPasswordAction,
    },
    /// Политика уровня (организация, домен, ящик): квота, сроки хранения, лимиты отправки.
    Policy {
        /// Действие.
        #[command(subcommand)]
        action: Box<PolicyAction>,
    },
    /// Группы рассылки организации.
    Group {
        /// Действие.
        #[command(subcommand)]
        action: Box<GroupAction>,
    },
    /// Корзина учётной записи: просмотр и восстановление писем.
    Trash {
        /// Действие.
        #[command(subcommand)]
        action: TrashAction,
    },
    /// Журнал аудита Admin API.
    Audit {
        /// Действие.
        #[command(subcommand)]
        action: Box<AuditAction>,
    },
    /// Журнал доставки: поиск писем.
    Delivery {
        /// Действие.
        #[command(subcommand)]
        action: Box<DeliveryAction>,
    },
    /// Транспорт: принятые домены, доверенные источники, политики доменов назначения.
    Transport {
        /// Ресурс.
        #[command(subcommand)]
        action: Box<TransportAction>,
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
        /// Поиск по имени входа (без учёта регистра); несовместимо с --prefix, --after и --limit.
        #[arg(long, conflicts_with_all = ["prefix", "after", "limit"])]
        login: Option<String>,
        /// Имя входа начинается с префикса (без учёта регистра).
        #[arg(long)]
        prefix: Option<String>,
        /// Продолжение после этого имени входа (из подсказки предыдущего вывода с --limit).
        #[arg(long, value_name = "ИМЯ_ВХОДА")]
        after: Option<String>,
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
    /// Псевдонимы и скрытие из книги «Организация» локальной учётной записи (у записи каталога — 409, их ведёт
    /// каталог). Нужен хотя бы один ключ.
    Set {
        /// Идентификатор (UUID) или имя входа.
        account: String,
        /// Псевдоним — адрес в принятом домене организации, не занятый учётной записью, группой или контактом (иначе
        /// 409). Ключ повторяется; заданные псевдонимы заменяют прежние.
        #[arg(long = "alias", value_name = "АДРЕС", conflicts_with = "no_aliases")]
        aliases: Vec<String>,
        /// Снять все псевдонимы.
        #[arg(long)]
        no_aliases: bool,
        /// Скрыть из книги «Организация» (`true`) или показать в ней (`false`); книга меняется не позже чем через
        /// `maintenance.gal_interval`.
        #[arg(long, value_name = "true|false")]
        hidden_from_gal: Option<bool>,
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

/// Действия с политикой уровня; уровень — организация, `--domain` или `--account`.
#[derive(Debug, Clone, Subcommand)]
pub enum PolicyAction {
    /// Политика: заданное на уровне и действующее с учётом вышестоящих (организация → домен → ящик).
    Get {
        /// Уровень.
        #[command(flatten)]
        level: PolicyLevel,
    },
    /// Замена политики уровня целиком. Файл — JSON с полями политики (quota_bytes, trash_retention, retention,
    /// send_per_hour, rcpts_per_hour, max_recipients, dkim_selector, milter_profile, trash_purge, archive_after,
    /// archive_quota_bytes); поля нет или null — не задано на уровне.
    Set {
        /// Уровень.
        #[command(flatten)]
        level: PolicyLevel,
        /// Файл с политикой (JSON, UTF-8); `-` — stdin.
        #[arg(long, value_name = "ПУТЬ")]
        file: PathBuf,
    },
}

/// Уровень политики: без ключей — организация.
#[derive(Debug, Clone, Args)]
pub struct PolicyLevel {
    /// Принятый домен: идентификатор (UUID) или имя.
    #[arg(long, conflicts_with = "account")]
    pub domain: Option<String>,
    /// Учётная запись: идентификатор (UUID) или имя входа.
    #[arg(long)]
    pub account: Option<String>,
}

/// Действия с группами рассылки; группа — идентификатор (UUID), имя или адрес.
#[derive(Debug, Clone, Subcommand)]
pub enum GroupAction {
    /// Группы организации.
    List,
    /// Группа: адрес, участники, ограничение отправителей, модераторы.
    Get {
        /// Группа: идентификатор (UUID), имя или адрес.
        group: String,
    },
    /// Новая группа (не из каталога).
    Create {
        /// Отображаемое имя.
        name: String,
        /// Адрес рассылки (без адреса — группа только для вложения).
        #[arg(long)]
        address: Option<String>,
        /// Участник — учётная запись: идентификатор (UUID) или имя входа (можно повторять).
        #[arg(long = "account", value_name = "УЧЁТНАЯ_ЗАПИСЬ")]
        accounts: Vec<String>,
        /// Участник — вложенная группа: идентификатор (UUID), имя или адрес (можно повторять).
        #[arg(long = "group", value_name = "ГРУППА")]
        groups: Vec<String>,
    },
    /// Удаление группы (не из каталога); письма её очереди модерации отклоняются с уведомлением.
    Delete {
        /// Группа: идентификатор (UUID), имя или адрес.
        group: String,
    },
    /// Участники группы; --set — замена участников перечисленными (без --account и --group — пусто).
    Members {
        /// Группа: идентификатор (UUID), имя или адрес.
        group: String,
        /// Заменить участников (у группы каталога участников ведёт каталог — 400).
        #[arg(long)]
        set: bool,
        /// Участник — учётная запись: идентификатор (UUID) или имя входа (можно повторять).
        #[arg(long = "account", value_name = "УЧЁТНАЯ_ЗАПИСЬ", requires = "set")]
        accounts: Vec<String>,
        /// Участник — вложенная группа: идентификатор (UUID), имя или адрес (можно повторять).
        #[arg(long = "group", value_name = "ГРУППА", requires = "set")]
        groups: Vec<String>,
    },
}

/// Действия с корзиной учётной записи.
#[derive(Debug, Clone, Subcommand)]
pub enum TrashAction {
    /// Письма корзины, новые первыми, и действующий срок корзины.
    List {
        /// Учётная запись: идентификатор (UUID) или имя входа.
        account: String,
        /// Скрытые письма («очистить корзину» с hide): пользователь их не видит.
        #[arg(long)]
        hidden: bool,
        /// Тема и отправитель писем: только администратору, запрос пишется в журнал аудита.
        #[arg(long)]
        subject: bool,
        /// Смещение от начала списка (из подсказки предыдущей страницы).
        #[arg(long, default_value_t = 0)]
        offset: usize,
        /// Размер страницы (по умолчанию и наибольший — из конфигурации узла).
        #[arg(long, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
        limit: Option<usize>,
    },
    /// Восстановление письма в исходную папку (нет — во «Входящие») с тем же идентификатором.
    Restore {
        /// Учётная запись: идентификатор (UUID) или имя входа.
        account: String,
        /// Письмо (UUID, из `trash list`).
        message: String,
        /// Скрытое письмо (из `trash list --hidden`).
        #[arg(long)]
        hidden: bool,
    },
}

/// Действия с журналом аудита.
#[derive(Debug, Clone, Subcommand)]
pub enum AuditAction {
    /// Записи по возрастанию номера; условия — по «и», хотя бы одно обязательно.
    Search {
        /// Начало интервала, RFC 3339 (например, 2026-12-01T00:00:00Z), включительно.
        #[arg(long, value_name = "ВРЕМЯ")]
        since: Option<String>,
        /// Конец интервала, RFC 3339, не включительно.
        #[arg(long, value_name = "ВРЕМЯ")]
        until: Option<String>,
        /// Администратор: имя входа или UUID учётной записи.
        #[arg(long)]
        actor: Option<String>,
        /// Метод HTTP (PUT, POST, PATCH, DELETE).
        #[arg(long)]
        method: Option<String>,
        /// Начало шаблона маршрута: /api/v1/orgs/{org}/transport — все маршруты транспорта.
        #[arg(long)]
        route: Option<String>,
        /// Размер страницы (по умолчанию и наибольший — из конфигурации узла).
        #[arg(long, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
        limit: Option<usize>,
        /// Продолжение: номер из подсказки предыдущей страницы.
        #[arg(long)]
        after: Option<u64>,
    },
}

/// Действия с журналом доставки.
#[derive(Debug, Clone, Subcommand)]
pub enum DeliveryAction {
    /// События по возрастанию времени; условия — по «и».
    Search {
        /// Отправитель конверта (точно, без учёта регистра).
        #[arg(long)]
        sender: Option<String>,
        /// Один из получателей (точно, без учёта регистра).
        #[arg(long)]
        recipient: Option<String>,
        /// Message-ID без угловых скобок.
        #[arg(long)]
        message_id: Option<String>,
        /// Начало интервала, RFC 3339 (например, 2026-12-01T00:00:00Z), включительно.
        #[arg(long, value_name = "ВРЕМЯ")]
        since: Option<String>,
        /// Конец интервала, RFC 3339, не включительно.
        #[arg(long, value_name = "ВРЕМЯ")]
        until: Option<String>,
        /// Размер страницы (по умолчанию и наибольший — из конфигурации узла).
        #[arg(long, value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
        limit: Option<usize>,
        /// Продолжение: позиция из подсказки предыдущей страницы (с теми же условиями).
        #[arg(long)]
        after: Option<String>,
    },
}

/// Ресурсы транспорта; каждая запись — по версии снимка последнего чтения (устарела — 409, повторите команду).
#[derive(Debug, Clone, Subcommand)]
pub enum TransportAction {
    /// Принятые домены организации.
    Domain {
        /// Действие.
        #[command(subcommand)]
        action: DomainAction,
    },
    /// Доверенные источники организации: шлюзы и приложения, отправляющие почту без входа.
    Source {
        /// Действие.
        #[command(subcommand)]
        action: SourceAction,
    },
    /// Политики доменов назначения: TLS, smart host, исходящий адрес.
    Route {
        /// Действие.
        #[command(subcommand)]
        action: RouteAction,
    },
}

/// Действия с принятыми доменами.
#[derive(Debug, Clone, Subcommand)]
pub enum DomainAction {
    /// Принятые домены организации.
    List,
    /// Новый принятый домен.
    Create {
        /// Имя домена.
        name: String,
        /// Внутренняя пересылка на узел host[:port] вместо собственного домена (authoritative).
        #[arg(long)]
        relay: Option<String>,
    },
    /// Удаление принятого домена.
    Delete {
        /// Домен: идентификатор (UUID) или имя.
        domain: String,
    },
}

/// Действия с доверенными источниками организации.
#[derive(Debug, Clone, Subcommand)]
pub enum SourceAction {
    /// Доверенные источники организации.
    List,
    /// Создание или замена источника: откуда (сети или сертификат клиента) и кто (шлюз или приложение).
    Set {
        /// Имя источника ([a-z0-9._-], до 64 символов).
        name: String,
        /// Сеть источника, CIDR (можно повторять).
        #[arg(
            long = "network",
            value_name = "CIDR",
            required_unless_present = "client_cert"
        )]
        networks: Vec<String>,
        /// Имя (dNSName) в сертификате клиента (mTLS) вместо сетей.
        #[arg(long, conflicts_with = "networks")]
        client_cert: Option<String>,
        /// Приложение (отправляет от перечисленных адресов) вместо шлюза.
        #[arg(long)]
        application: bool,
        /// Адрес отправителя приложения: user@domain или @domain (можно повторять).
        #[arg(long = "sender", value_name = "ШАБЛОН", requires = "application")]
        senders: Vec<String>,
        /// Приложению разрешены внешние получатели.
        #[arg(long, requires = "application")]
        external: bool,
        /// Шлюз: принимать адрес исходного клиента из XCLIENT/XFORWARD.
        #[arg(long, conflicts_with = "application")]
        xclient: bool,
        /// Шлюз: доверять Authentication-Results и ARC с этим authserv-id.
        #[arg(long, conflicts_with = "application")]
        authserv_id: Option<String>,
    },
    /// Удаление источника организации.
    Delete {
        /// Имя источника.
        name: String,
    },
}

/// Требование TLS политики домена назначения.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum TlsMode {
    /// TLS, если сервер предлагает.
    #[default]
    Opportunistic,
    /// Обязательный TLS с проверкой сертификата; иначе — временный отказ доставки.
    Required,
}

/// Действия с политиками доменов назначения.
#[derive(Debug, Clone, Subcommand)]
pub enum RouteAction {
    /// Политики доменов назначения организации.
    List,
    /// Создание или замена политики домена назначения.
    Set {
        /// Шаблон: example.org, *.example.org (поддомены) или * (все внешние).
        pattern: String,
        /// Требование TLS.
        #[arg(long, value_enum, default_value_t = TlsMode::Opportunistic)]
        tls: TlsMode,
        /// Закреплённый ключ сервера: SHA-256 SubjectPublicKeyInfo, hex (можно повторять); вместо --tls.
        #[arg(long = "pin", value_name = "SHA256", conflicts_with = "tls")]
        pins: Vec<String>,
        /// Отправка через узел host[:port] вместо MX.
        #[arg(long)]
        smart_host: Option<String>,
        /// Исходящий IP-адрес.
        #[arg(long, value_name = "IP")]
        egress_ip: Option<String>,
    },
    /// Удаление политики домена назначения.
    Delete {
        /// Шаблон (как в `transport route list`).
        pattern: String,
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

/// Результат команды: текст для stdout или сообщение об ошибке.
type CliResult<T> = std::result::Result<T, String>;

/// Поле ответа `v` как в JSON Admin API (даты — RFC 3339, форматом tp-admin); `null` или нет поля — «—».
fn json_field<T: Serialize>(v: &T, field: &str) -> String {
    match serde_json::to_value(v) {
        Ok(serde_json::Value::Object(mut fields)) => fields
            .remove(field)
            .map_or_else(|| "—".to_owned(), |v| value_text(&v)),
        _ => "—".to_owned(),
    }
}

/// Значение JSON для вывода: строка — без кавычек, `null` — «—».
fn value_text(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => "—".to_owned(),
        other => other.to_string(),
    }
}

/// `thepost admin <ресурс> <действие>`: код возврата (0 — успех, 1 — отказ или ошибка).
pub async fn command(config: &Path, args: ApiArgs, action: ApiAction) -> i32 {
    if matches!(action, ApiAction::Login { oidc: true, .. }) {
        return oidc_command(config).await;
    }
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
    let options = args.options();
    if let ApiAction::Login {
        user: Some(user),
        password_stdin,
        ..
    } = &action
    {
        let password = read_password(*password_stdin).await?;
        let client = Client::anonymous(base_url(config, args)?, &options)?;
        return login(&client, args, user, password).await;
    }
    let token = tp_admin_client::token(args.token.clone())?;
    let client = Client::new(base_url(config, args)?, token, &options)?;
    execute(&client, args, action).await
}

/// `login --oidc` — вход с устройства по config.yaml `config`: приглашение — в stderr, токен доступа — строкой
/// в stdout, чтобы он не попал в историю оболочки: `export THEPOST_ADMIN_TOKEN="$(…)"`.
async fn oidc_command(config: &Path) -> i32 {
    match oidc_login(config).await {
        Ok(token) => {
            println!("{token}");
            eprintln!(
                "thepost: admin: токен OIDC получен; без записи в историю оболочки: \
                 export {ENV_TOKEN}=\"$(thepost admin login --oidc)\""
            );
            0
        }
        Err(msg) => {
            eprintln!("thepost: admin: {msg}");
            1
        }
    }
}

/// Токен OIDC: поставщик — `auth.oidc`, клиент — `cli.oidc`, ключ канала — из хранилища секретов узла.
async fn oidc_login(config: &Path) -> CliResult<String> {
    let registry = sections::registry().map_err(|e| e.to_string())?;
    let path = config.to_owned();
    let cfg = tokio::task::spawn_blocking(move || registry.load(&path))
        .await
        .map_err(|e| format!("чтение конфигурации прервано: {e}"))?
        .map_err(|e| format!("{}: {e}", config.display()))?;
    let auth = cfg
        .section::<AuthSection>(AUTH)
        .map_err(|e| format!("{}: секция auth: {e}", config.display()))?;
    let oidc = auth.oidc.as_ref().ok_or_else(|| {
        format!(
            "{}: нет auth.oidc — поставщик OIDC не настроен",
            config.display()
        )
    })?;
    let cli = cfg
        .section::<CliSection>(CLI)
        .map_err(|e| format!("{}: секция cli: {e}", config.display()))?;
    let settings = device_settings(&cli.oidc)?;
    let secrets = crate::secrets::store(&cfg, None)
        .await
        .map_err(|e| format!("{}: хранилище секретов: {e}", config.display()))?;
    let login = DeviceLogin::new(oidc, settings, cfg.profile(), secrets.as_ref()).await?;
    Ok(login.login(|code| eprint!("{}", prompt_text(code))).await?)
}

/// Настройки входа с устройства из `cli.oidc`; без `client_id` вход недоступен.
fn device_settings(cli: &CliOidc) -> CliResult<DeviceSettings> {
    cli.check().map_err(|e| e.to_string())?;
    let client_id = cli.client_id.clone().ok_or_else(|| {
        "не задан cli.oidc.client_id — идентификатор открытого клиента (public client) с разрешённым \
         Device Authorization Grant у поставщика OIDC"
            .to_owned()
    })?;
    Ok(DeviceSettings {
        client_id,
        scope: cli.scope.clone(),
        poll_interval: cli.poll_interval,
        slow_down_step: cli.slow_down_step,
        max_wait: cli.max_wait,
    })
}

/// Пароль входа: первая строка stdin (`--password-stdin`) или ввод в терминале без эха; чтение — вне потоков async.
async fn read_password(stdin: bool) -> CliResult<String> {
    let read = move || -> std::io::Result<String> {
        if stdin {
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            Ok(line.trim_end_matches(['\r', '\n']).to_owned())
        } else {
            rpassword::prompt_password("Пароль: ")
        }
    };
    let password = tokio::task::spawn_blocking(read)
        .await
        .map_err(|e| format!("чтение пароля: {e}"))?
        .map_err(|e| {
            if stdin {
                format!("пароль из stdin: {e}")
            } else {
                format!("пароль из терминала: {e} (без терминала — --password-stdin)")
            }
        })?;
    if password.is_empty() {
        return Err("пустой пароль".to_owned());
    }
    Ok(password)
}

/// Вход по паролю: сессия Admin API (`POST /api/v1/session`), значение — один раз в stdout.
pub async fn login(
    client: &Client,
    args: &ApiArgs,
    user: &str,
    password: String,
) -> CliResult<String> {
    let s = client.login(user, password).await?;
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

/// Адрес Admin API: `--url`, `THEPOST_ADMIN_URL` или `http.admin` из config.yaml.
fn base_url(config: &Path, args: &ApiArgs) -> CliResult<Url> {
    let raw = match setting(args.url.clone(), ENV_URL) {
        Some(u) => u,
        None => url_from_config(config)?,
    };
    Ok(tp_admin_client::base_url(&raw)?)
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
        ApiAction::Login { .. } => Err(
            "login — вход по паролю или у поставщика OIDC (thepost admin login), не запрос с сессией".to_owned(),
        ),
        ApiAction::Whoami => {
            let w = client.whoami().await?;
            out(args, &w, whoami_record(&w))
        }
        ApiAction::Account { action } => {
            let org = client.org(args.org.clone()).await?;
            match *action {
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
                    after,
                    limit,
                } => {
                    let (found, more) =
                        list_accounts(client, &org, prefix.clone(), after, limit).await?;
                    let rows: Vec<Vec<String>> = found.iter().map(account_row).collect();
                    let mut text = table(ACCOUNT_COLUMNS, &rows);
                    if let Some(last) = found.last().filter(|_| more) {
                        // `after` следующей страницы — как в ссылке rel="next": последнее показанное имя входа.
                        let prefix = prefix.map(|p| format!(" --prefix {p}")).unwrap_or_default();
                        text.push_str(&format!(
                            "Показано {}; есть ещё. Следующая страница: thepost admin account list{prefix} \
                             --after {} --limit {}\n",
                            found.len(),
                            last.login.to_lowercase(),
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
                AccountAction::Set {
                    account,
                    aliases,
                    no_aliases,
                    hidden_from_gal,
                } => {
                    let aliases = (no_aliases || !aliases.is_empty()).then_some(aliases);
                    if aliases.is_none() && hidden_from_gal.is_none() {
                        return Err(
                            "нужен хотя бы один ключ: --alias, --no-aliases, --hidden-from-gal"
                                .to_owned(),
                        );
                    }
                    let patch = AccountPatch {
                        aliases,
                        hidden_from_gal,
                        ..AccountPatch::default()
                    };
                    let a = patch_account(client, &org, &account, &patch).await?;
                    out(args, &a, account_record(&a))
                }
            }
        }
        ApiAction::AppPassword { action } => {
            let org = client.org(args.org.clone()).await?;
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
        ApiAction::Policy { action } => policy_command(client, args, *action).await,
        ApiAction::Group { action } => group_command(client, args, *action).await,
        ApiAction::Trash { action } => trash_command(client, args, action).await,
        ApiAction::Audit { action } => audit_command(client, args, *action).await,
        ApiAction::Delivery { action } => delivery_command(client, args, *action).await,
        ApiAction::Transport { action } => {
            let org = client.org(args.org.clone()).await?;
            match *action {
                TransportAction::Domain { action } => {
                    domain_command(client, args, &org, action).await
                }
                TransportAction::Source { action } => {
                    source_command(client, args, &org, action).await
                }
                TransportAction::Route { action } => {
                    route_command(client, args, &org, action).await
                }
            }
        }
        ApiAction::Sieve { action } => {
            let org = client.org(args.org.clone()).await?;
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
                        text: input_text(file).await?,
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
                            text: input_text(file).await?,
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
            let org = client.org(args.org.clone()).await?;
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
                        Some(file) => Some(input_text(file).await?),
                        None => text,
                    };
                    let html = match html_file {
                        Some(file) => Some(input_text(file).await?),
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

/// Уровень политики с идентификатором из пути операции.
enum Level {
    Org,
    Domain(String),
    Account(String),
}

/// Уровень `level`: домен и учётная запись — по идентификатору или имени.
async fn level(client: &Client, org: &str, level: PolicyLevel) -> CliResult<Level> {
    Ok(match (level.domain, level.account) {
        (Some(domain), _) => Level::Domain(domain_id(client, org, &domain).await?),
        (None, Some(account)) => Level::Account(account_id(client, org, &account).await?),
        (None, None) => Level::Org,
    })
}

/// `thepost admin policy …`.
async fn policy_command(
    client: &Client,
    args: &ApiArgs,
    action: PolicyAction,
) -> CliResult<String> {
    let org = client.org(args.org.clone()).await?;
    match action {
        PolicyAction::Get { level: l } => {
            let p: Policies = match level(client, &org, l).await? {
                Level::Org => {
                    let o: Organization = client.call(ORG_GET, &[&org], &[], NO_BODY).await?;
                    o.policies
                }
                Level::Domain(id) => {
                    client
                        .call(DOMAIN_POLICY_GET, &[&org, &id], &[], NO_BODY)
                        .await?
                }
                Level::Account(id) => {
                    let a: AccountInfo =
                        client.call(ACCOUNT_GET, &[&org, &id], &[], NO_BODY).await?;
                    a.policies
                }
            };
            out(args, &p, policy_table(&p.policy, Some(&p.effective)))
        }
        PolicyAction::Set { level: l, file } => {
            let text = input_text(file).await?;
            let body: PolicyBody =
                serde_json::from_str(&text).map_err(|e| format!("политика (JSON): {e}"))?;
            let (op, id) = match level(client, &org, l).await? {
                Level::Org => (ORG_POLICY_PUT, None),
                Level::Domain(id) => (DOMAIN_POLICY_PUT, Some(id)),
                Level::Account(id) => (ACCOUNT_POLICY_PUT, Some(id)),
            };
            let params: Vec<&str> = [Some(org.as_str()), id.as_deref()]
                .into_iter()
                .flatten()
                .collect();
            let p: PolicyBody = client.call(op, &params, &[], Some(&body)).await?;
            out(args, &p, policy_table(&p, None))
        }
    }
}

/// Поля политики: заданное на уровне и действующее (`effective`); не задано — «—».
fn policy_table(policy: &PolicyBody, effective: Option<&PolicyBody>) -> String {
    let fields = |p: &PolicyBody| match serde_json::to_value(p) {
        Ok(serde_json::Value::Object(fields)) => fields,
        _ => serde_json::Map::new(),
    };
    let effective = effective.map(fields);
    let rows: Vec<Vec<String>> = fields(policy)
        .iter()
        .map(|(name, v)| {
            let mut row = vec![name.clone(), value_text(v)];
            if let Some(e) = &effective {
                row.push(e.get(name).map_or_else(|| "—".to_owned(), value_text));
            }
            row
        })
        .collect();
    let cols: &[&str] = if effective.is_some() {
        &["ПОЛЕ", "НА УРОВНЕ", "ДЕЙСТВУЕТ"]
    } else {
        &["ПОЛЕ", "НА УРОВНЕ"]
    };
    table(cols, &rows)
}

/// `thepost admin group …`.
async fn group_command(client: &Client, args: &ApiArgs, action: GroupAction) -> CliResult<String> {
    let org = client.org(args.org.clone()).await?;
    match action {
        GroupAction::List => {
            let list = groups(client, &org).await?;
            let rows: Vec<Vec<String>> = list.iter().map(group_row).collect();
            out(args, &list, table(GROUP_COLUMNS, &rows))
        }
        GroupAction::Get { group } => {
            let id = group_id(client, &org, &group).await?;
            let g: GroupInfo = client.call(GROUP_GET, &[&org, &id], &[], NO_BODY).await?;
            out(args, &g, group_record(&g))
        }
        GroupAction::Create {
            name,
            address,
            accounts,
            groups,
        } => {
            let body = NewGroup {
                name,
                address,
                members: members(client, &org, accounts, groups).await?,
                senders: SenderRestriction::default(),
                moderators: Vec::new(),
                hidden_from_gal: false,
            };
            let g: GroupInfo = client.call(GROUP_CREATE, &[&org], &[], Some(&body)).await?;
            out(args, &g, group_record(&g))
        }
        GroupAction::Delete { group } => {
            let id = group_id(client, &org, &group).await?;
            client
                .call_empty(GROUP_DELETE, &[&org, &id], &[], NO_BODY)
                .await?;
            out(args, &(), format!("Группа {group} удалена.\n"))
        }
        GroupAction::Members {
            group,
            set,
            accounts,
            groups,
        } => {
            let id = group_id(client, &org, &group).await?;
            let params = [org.as_str(), id.as_str()];
            let g: GroupInfo = if set {
                let patch = GroupPatch {
                    members: Some(members(client, &org, accounts, groups).await?),
                    ..GroupPatch::default()
                };
                client.call(GROUP_PATCH, &params, &[], Some(&patch)).await?
            } else {
                client.call(GROUP_GET, &params, &[], NO_BODY).await?
            };
            let rows: Vec<Vec<String>> = g
                .members
                .iter()
                .map(|m| {
                    let (kind, id) = member(m);
                    vec![kind.to_owned(), id.clone(), name_of(&g, id)]
                })
                .collect();
            out(
                args,
                &g.members,
                table(&["ВИД", "ИДЕНТИФИКАТОР", "ИМЯ"], &rows),
            )
        }
    }
}

const GROUP_COLUMNS: &[&str] = &["ИДЕНТИФИКАТОР", "ИМЯ", "АДРЕС", "УЧАСТНИКОВ"];

/// Группы организации.
async fn groups(client: &Client, org: &str) -> CliResult<Vec<GroupInfo>> {
    Ok(client.call(GROUP_LIST, &[org], &[], NO_BODY).await?)
}

/// Идентификатор группы: UUID как есть, иначе единственная группа с таким именем или адресом (без учёта регистра).
async fn group_id(client: &Client, org: &str, group: &str) -> CliResult<String> {
    if group.parse::<GroupId>().is_ok() {
        return Ok(group.to_owned());
    }
    let want = group.to_lowercase();
    let list = groups(client, org).await?;
    let found: Vec<&GroupInfo> = list
        .iter()
        .filter(|g| {
            g.name.to_lowercase() == want
                || g.address
                    .as_deref()
                    .is_some_and(|a| a.to_lowercase() == want)
        })
        .collect();
    match found.as_slice() {
        [g] => Ok(g.id.clone()),
        [] => Err(format!("группа {group} не найдена")),
        _ => Err(format!(
            "групп с именем {group} несколько — укажите идентификатор (из group list)"
        )),
    }
}

/// Участники: учётные записи (идентификатор или имя входа) и группы (идентификатор, имя или адрес).
async fn members(
    client: &Client,
    org: &str,
    accounts: Vec<String>,
    groups: Vec<String>,
) -> CliResult<Vec<GroupMember>> {
    let mut out = Vec::with_capacity(accounts.len() + groups.len());
    for a in accounts {
        let id = account_id(client, org, &a).await?;
        let id = id
            .parse::<AccountId>()
            .map_err(|e| format!("учётная запись {id}: {e}"))?;
        out.push(GroupMember::Account(id));
    }
    for g in groups {
        let id = group_id(client, org, &g).await?;
        let id = id
            .parse::<GroupId>()
            .map_err(|e| format!("группа {id}: {e}"))?;
        out.push(GroupMember::Group(id));
    }
    Ok(out)
}

/// Вид и идентификатор участника.
fn member(m: &GroupMember) -> (&'static str, String) {
    match m {
        GroupMember::Account(id) => ("учётная запись", id.to_string()),
        GroupMember::Group(id) => ("группа", id.to_string()),
    }
}

/// Имя по идентификатору из словаря `names` ответа; нет в словаре — «—» (записи уже нет).
fn name_of(g: &GroupInfo, id: String) -> String {
    g.names.get(&id).cloned().unwrap_or_else(|| "—".to_owned())
}

/// Группа — строка таблицы [`GROUP_COLUMNS`].
fn group_row(g: &GroupInfo) -> Vec<String> {
    vec![
        g.id.clone(),
        g.name.clone(),
        optional(g.address.as_ref()),
        g.members.len().to_string(),
    ]
}

/// Группа — строки «подпись: значение»; участники и модераторы — по именам.
fn group_record(g: &GroupInfo) -> String {
    let names = |ids: Vec<String>| {
        let list: Vec<String> = ids.into_iter().map(|id| name_of(g, id)).collect();
        or_dash(&list.join(", "))
    };
    let senders =
        serde_json::to_value(&g.senders).map_or_else(|_| "—".to_owned(), |v| value_text(&v));
    record(&[
        ("Идентификатор", g.id.clone()),
        ("Имя", g.name.clone()),
        ("Адрес", optional(g.address.as_ref())),
        ("Из каталога", optional(g.external_id.as_ref())),
        (
            "Участники",
            names(g.members.iter().map(|m| member(m).1).collect()),
        ),
        ("Отправители", senders),
        (
            "Модераторы",
            names(g.moderators.iter().map(ToString::to_string).collect()),
        ),
        (
            "Скрыта из GAL",
            if g.hidden_from_gal { "да" } else { "нет" }.to_owned(),
        ),
    ])
}

/// `thepost admin trash …`.
async fn trash_command(client: &Client, args: &ApiArgs, action: TrashAction) -> CliResult<String> {
    let org = client.org(args.org.clone()).await?;
    match action {
        TrashAction::List {
            account,
            hidden,
            subject,
            offset,
            limit,
        } => {
            let id = account_id(client, &org, &account).await?;
            let op = if hidden {
                TRASH_HIDDEN_LIST
            } else {
                TRASH_LIST
            };
            let query = query(&PageQuery {
                offset,
                limit,
                include: subject.then(|| "subject".to_owned()),
            })?;
            let page: TrashPage = client.call(op, &[&org, &id], &query, NO_BODY).await?;
            let mut cols = vec!["ПИСЬМО", "ПАПКА", "РАЗМЕР", "УДАЛЕНО", "УДАЛЯЕТСЯ"];
            if subject {
                cols.extend(["ТЕМА", "ОТПРАВИТЕЛЬ"]);
            }
            let rows: Vec<Vec<String>> = page
                .items
                .iter()
                .map(|e| {
                    let mut row = vec![
                        e.message.clone(),
                        e.origin.clone(),
                        e.size.to_string(),
                        json_field(e, "deleted_at"),
                        json_field(e, "expires_at"),
                    ];
                    if subject {
                        for v in [&e.subject, &e.from] {
                            row.push(optional(v.clone().flatten().as_ref()));
                        }
                    }
                    row
                })
                .collect();
            let retention = match page.trash_retention {
                Some(_) => json_field(&page, "trash_retention"),
                None => "по умолчанию узла".to_owned(),
            };
            let mut text = format!("{}Срок корзины: {retention}\n", table(&cols, &rows));
            if let Some(next) = page.next_offset {
                text.push_str(&format!("Есть ещё. Следующая страница: --offset {next}\n"));
            }
            out(args, &page, text)
        }
        TrashAction::Restore {
            account,
            message,
            hidden,
        } => {
            let id = account_id(client, &org, &account).await?;
            let op = if hidden {
                TRASH_HIDDEN_RESTORE
            } else {
                TRASH_RESTORE
            };
            let r: RestoredMessage = client
                .call(op, &[&org, &id, &message], &[], NO_BODY)
                .await?;
            let text = format!("Письмо {} восстановлено, UID {}.\n", r.message, r.uid);
            out(args, &r, text)
        }
    }
}

/// `thepost admin audit search`: одна страница; продолжение — подсказкой `--after`.
async fn audit_command(client: &Client, args: &ApiArgs, action: AuditAction) -> CliResult<String> {
    let AuditAction::Search {
        since,
        until,
        actor,
        method,
        route,
        limit,
        after,
    } = action;
    let org = client.org(args.org.clone()).await?;
    let params: AuditParams = from_json(
        serde_json::json!({
            "since": since, "until": until, "actor": actor, "method": method, "route": route,
            "limit": limit, "after": after,
        }),
        "журнал аудита",
    )?;
    let page: AuditPage = client
        .call(AUDIT_SEARCH, &[&org], &query(&params)?, NO_BODY)
        .await?;
    let rows: Vec<Vec<String>> = page
        .entries
        .iter()
        .map(|e| {
            let r = &e.record;
            vec![
                e.seq.to_string(),
                json_field(r, "at"),
                r.actor_login.clone(),
                r.method.clone(),
                r.route.clone(),
                r.status.to_string(),
            ]
        })
        .collect();
    let mut text = table(
        &[
            "НОМЕР",
            "МОМЕНТ",
            "АДМИНИСТРАТОР",
            "МЕТОД",
            "МАРШРУТ",
            "КОД",
        ],
        &rows,
    );
    if let Some(next) = page.next {
        text.push_str(&format!(
            "Есть ещё. Следующая страница: те же условия и --after {next}\n"
        ));
    }
    out(args, &page, text)
}

/// `thepost admin delivery search`: одна страница; продолжение — подсказкой `--after`.
async fn delivery_command(
    client: &Client,
    args: &ApiArgs,
    action: DeliveryAction,
) -> CliResult<String> {
    let DeliveryAction::Search {
        sender,
        recipient,
        message_id,
        since,
        until,
        limit,
        after,
    } = action;
    let org = client.org(args.org.clone()).await?;
    let params: DeliveryParams = from_json(
        serde_json::json!({
            "sender": sender, "recipient": recipient, "message_id": message_id, "since": since,
            "until": until, "limit": limit, "after": after,
        }),
        "журнал доставки",
    )?;
    let page: DeliveryResults = client
        .call(DELIVERY_SEARCH, &[&org], &query(&params)?, NO_BODY)
        .await?;
    let rows: Vec<Vec<String>> = page
        .events
        .iter()
        .map(|e| {
            vec![
                json_field(e, "at"),
                json_field(e, "kind"),
                or_dash(&e.sender),
                e.recipients.join(", "),
                json_field(e, "decision"),
                optional(e.reason.as_ref()),
            ]
        })
        .collect();
    let mut text = table(
        &[
            "МОМЕНТ",
            "СОБЫТИЕ",
            "ОТПРАВИТЕЛЬ",
            "ПОЛУЧАТЕЛИ",
            "РЕШЕНИЕ",
            "ПРИЧИНА",
        ],
        &rows,
    );
    if let Some(next) = &page.next {
        text.push_str(&format!(
            "Есть ещё. Следующая страница: те же условия и --after {next}\n"
        ));
    }
    out(args, &page, text)
}

/// Структура запроса tp-admin из значений ключей CLI: разбор и проверка — её serde (время — RFC 3339).
fn from_json<T: DeserializeOwned>(v: serde_json::Value, what: &str) -> CliResult<T> {
    serde_json::from_value(v)
        .map_err(|e| format!("{what}: {e} (время — RFC 3339, например 2026-12-01T00:00:00Z)"))
}

/// `thepost admin transport domain …`.
async fn domain_command(
    client: &Client,
    args: &ApiArgs,
    org: &str,
    action: DomainAction,
) -> CliResult<String> {
    match action {
        DomainAction::List => {
            let list = domains(client, org).await?;
            let rows: Vec<Vec<String>> = list.domains.iter().map(domain_row).collect();
            let text = format!(
                "{}Версия снимка: {}\n",
                table(DOMAIN_COLUMNS, &rows),
                list.version
            );
            out(args, &list, text)
        }
        DomainAction::Create { name, relay } => {
            let version = domains(client, org).await?.version;
            let kind = match relay {
                Some(next_hop) => DomainKind::InternalRelay { next_hop },
                None => DomainKind::Authoritative,
            };
            let body = DomainBody {
                version,
                name,
                kind,
            };
            let w: DomainWritten = client.call(DOMAIN_CREATE, &[org], &[], Some(&body)).await?;
            let fields: Vec<(&str, String)> = DOMAIN_COLUMNS
                .iter()
                .copied()
                .zip(domain_row(&w.domain))
                .collect();
            let text = format!("{}Версия снимка: {}\n", record(&fields), w.version);
            out(args, &w, text)
        }
        DomainAction::Delete { domain } => {
            let list = domains(client, org).await?;
            let id = find_domain(&list, &domain)?;
            let query = query(&VersionQuery {
                version: list.version,
            })?;
            let w: Written = client
                .call(DOMAIN_DELETE, &[org, &id], &query, NO_BODY)
                .await?;
            let text = format!("Домен {domain} удалён. Версия снимка: {}\n", w.version);
            out(args, &w, text)
        }
    }
}

/// Идентификатор принятого домена в `list`: UUID как есть, иначе по имени (без учёта регистра).
fn find_domain(list: &DomainList, domain: &str) -> CliResult<String> {
    if domain.parse::<DomainId>().is_ok() {
        return Ok(domain.to_owned());
    }
    let name = domain.trim_end_matches('.');
    list.domains
        .iter()
        .find(|d| d.name.eq_ignore_ascii_case(name))
        .map(|d| d.id.to_string())
        .ok_or_else(|| format!("принятый домен {domain} не найден"))
}

/// Идентификатор принятого домена организации по идентификатору или имени.
async fn domain_id(client: &Client, org: &str, domain: &str) -> CliResult<String> {
    if domain.parse::<DomainId>().is_ok() {
        return Ok(domain.to_owned());
    }
    find_domain(&domains(client, org).await?, domain)
}

const SOURCE_COLUMNS: &[&str] = &["ИМЯ", "ОТКУДА", "ВИД"];

/// `thepost admin transport source …`.
async fn source_command(
    client: &Client,
    args: &ApiArgs,
    org: &str,
    action: SourceAction,
) -> CliResult<String> {
    let sources = move || async move {
        client
            .call::<(), SourceList>(SOURCE_LIST, &[org], &[], NO_BODY)
            .await
    };
    match action {
        SourceAction::List => {
            let list = sources().await?;
            let rows: Vec<Vec<String>> = list.sources.iter().map(source_row).collect();
            let text = format!(
                "{}Версия снимка: {}\n",
                table(SOURCE_COLUMNS, &rows),
                list.version
            );
            out(args, &list, text)
        }
        SourceAction::Set {
            name,
            networks,
            client_cert,
            application,
            senders,
            external,
            xclient,
            authserv_id,
        } => {
            let matcher = match client_cert {
                Some(subject) => serde_json::json!({ "client_cert": { "subject": subject } }),
                None => serde_json::json!({ "networks": networks }),
            };
            let kind = if application {
                serde_json::json!({ "application": { "senders": senders, "external": external } })
            } else {
                serde_json::json!({ "gateway": { "xclient": xclient, "authserv_id": authserv_id } })
            };
            let version = sources().await?.version;
            let body: SourceBody = from_json(
                serde_json::json!({ "version": version, "matcher": matcher, "kind": kind }),
                "источник",
            )?;
            let w: Written = client
                .call(SOURCE_PUT, &[org, &name], &[], Some(&body))
                .await?;
            let text = format!("Источник {name} записан. Версия снимка: {}\n", w.version);
            out(args, &w, text)
        }
        SourceAction::Delete { name } => {
            let query = query(&VersionQuery {
                version: sources().await?.version,
            })?;
            let w: Written = client
                .call(SOURCE_DELETE, &[org, &name], &query, NO_BODY)
                .await?;
            let text = format!("Источник {name} удалён. Версия снимка: {}\n", w.version);
            out(args, &w, text)
        }
    }
}

/// Доверенный источник — строка таблицы [`SOURCE_COLUMNS`].
fn source_row(s: &TrustedSource) -> Vec<String> {
    let join = |items: Vec<String>| items.join(", ");
    let matcher = match &s.matcher {
        SourceMatch::Networks(nets) => join(nets.iter().map(ToString::to_string).collect()),
        SourceMatch::ClientCert { subject } => format!("сертификат {subject}"),
    };
    let kind = match &s.kind {
        SourceKind::Gateway {
            xclient,
            authserv_id,
        } => {
            let mut k = "шлюз".to_owned();
            if *xclient {
                k.push_str(", XCLIENT");
            }
            if let Some(id) = authserv_id {
                k.push_str(&format!(", authserv-id {id}"));
            }
            k
        }
        SourceKind::Application { senders, external } => format!(
            "приложение: {}{}",
            join(senders.iter().map(ToString::to_string).collect()),
            if *external {
                ", внешние получатели"
            } else {
                ""
            }
        ),
    };
    vec![s.name.clone(), matcher, kind]
}

const ROUTE_COLUMNS: &[&str] = &["ШАБЛОН", "TLS", "SMART HOST", "ИСХОДЯЩИЙ IP"];

/// `thepost admin transport route …`.
async fn route_command(
    client: &Client,
    args: &ApiArgs,
    org: &str,
    action: RouteAction,
) -> CliResult<String> {
    let routes = move || async move {
        client
            .call::<(), RouteList>(ROUTE_LIST, &[org], &[], NO_BODY)
            .await
    };
    match action {
        RouteAction::List => {
            let list = routes().await?;
            let rows: Vec<Vec<String>> = list.routes.iter().map(route_row).collect();
            let text = format!(
                "{}Версия снимка: {}\n",
                table(ROUTE_COLUMNS, &rows),
                list.version
            );
            out(args, &list, text)
        }
        RouteAction::Set {
            pattern,
            tls,
            pins,
            smart_host,
            egress_ip,
        } => {
            let tls = match (tls, pins.is_empty()) {
                (_, false) => TlsRequirement::Pinned { spki_sha256: pins },
                (TlsMode::Opportunistic, true) => TlsRequirement::Opportunistic,
                (TlsMode::Required, true) => TlsRequirement::Required,
            };
            let egress_ip = egress_ip
                .map(|ip| {
                    ip.parse::<IpAddr>()
                        .map_err(|e| format!("--egress-ip {ip}: {e}"))
                })
                .transpose()?;
            let body = RouteBody {
                version: routes().await?.version,
                tls,
                smart_host,
                egress_ip,
            };
            let w: Written = client
                .call(ROUTE_PUT, &[org, &pattern], &[], Some(&body))
                .await?;
            let text = format!(
                "Политика {pattern} записана. Версия снимка: {}\n",
                w.version
            );
            out(args, &w, text)
        }
        RouteAction::Delete { pattern } => {
            let query = query(&VersionQuery {
                version: routes().await?.version,
            })?;
            let w: Written = client
                .call(ROUTE_DELETE, &[org, &pattern], &query, NO_BODY)
                .await?;
            let text = format!("Политика {pattern} удалена. Версия снимка: {}\n", w.version);
            out(args, &w, text)
        }
    }
}

/// Политика домена назначения — строка таблицы [`ROUTE_COLUMNS`].
fn route_row(r: &DestinationPolicy) -> Vec<String> {
    let tls = match &r.tls {
        TlsRequirement::Opportunistic => "opportunistic".to_owned(),
        TlsRequirement::Required => "required".to_owned(),
        TlsRequirement::Pinned { spki_sha256 } => format!("pinned: {}", spki_sha256.join(", ")),
    };
    vec![
        r.pattern.clone(),
        tls,
        optional(r.smart_host.as_ref()),
        optional(r.egress_ip.as_ref()),
    ]
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
    Ok(client
        .call(ACCOUNT_PATCH, &[org, id.as_str()], &[], Some(patch))
        .await?)
}

/// Текст из файла `file` (`-` — stdin); чтение — вне потоков async.
async fn input_text(file: PathBuf) -> CliResult<String> {
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
        ("Псевдонимы", or_dash(&a.aliases.join(", "))),
        (
            "Скрыта из книги «Организация»",
            a.hidden_from_gal.to_string(),
        ),
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

/// Учётные записи с именем входа `login`.
async fn find_accounts(client: &Client, org: &str, login: String) -> CliResult<Vec<AccountInfo>> {
    let query = query(&AccountQuery {
        login: Some(login),
        ..AccountQuery::default()
    })?;
    Ok(client.call(ACCOUNT_FIND, &[org], &query, NO_BODY).await?)
}

/// Список учётных записей: страницы Admin API по ссылке `rel="next"`, не больше `limit` записей (`None` — все);
/// второе значение — остались записи сверх `limit`.
async fn list_accounts(
    client: &Client,
    org: &str,
    prefix: Option<String>,
    after: Option<String>,
    limit: Option<usize>,
) -> CliResult<(Vec<AccountInfo>, bool)> {
    let query = query(&AccountQuery {
        prefix,
        after,
        limit,
        login: None,
    })?;
    Ok(client.pages(ACCOUNT_FIND, &[org], query, limit).await?)
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
    Ok(client.call(DOMAIN_LIST, &[org], &[], NO_BODY).await?)
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
    use tp_contracts::TenantId;
    use tp_contracts::auth::Principal;
    use tp_contracts::ext::DecisionHooks;
    use tp_testkit::{
        Clock, FakeAuthenticator, MemAbuseBlocks, MemAppPasswords, MemAuditLog, MemBlobIndex,
        MemBlobStore, MemDeliveryLog, MemEventBus, MemGroups, MemLocalAdminTokens, MemMetadata,
        MemModeration, MemPolicies, MemQuota, MemSieve, MemTransport, MemTrash, SystemClock,
    };
    use tp_testkit::{MemAddressBook, MemMailboxAudit, MemSenderLists, MemSyncGate};

    use super::*;

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
        let AdminAction::Api(ApiAction::Account { action }) = action else {
            panic!("{action:?}")
        };
        assert!(matches!(
            *action,
            AccountAction::Create { ref login, name: Some(_) } if login == "a@b.test"
        ));
        let cli = Cli::try_parse_from([
            "thepost",
            "admin",
            "--url",
            "https://127.0.0.1:8443",
            "transport",
            "domain",
            "create",
            "b.test",
            "--org",
            "00000000-0000-0000-0000-000000000001",
        ])
        .expect("transport domain create");
        let Command::Admin { api, action } = cli.command else {
            panic!("{:?}", cli.command)
        };
        assert_eq!(api.url.as_deref(), Some("https://127.0.0.1:8443"));
        assert!(api.org.is_some());
        let AdminAction::Api(ApiAction::Transport { action }) = action else {
            panic!("{action:?}")
        };
        assert!(matches!(
            *action,
            TransportAction::Domain {
                action: DomainAction::Create { relay: None, .. }
            }
        ));
        let cli =
            Cli::try_parse_from(["thepost", "admin", "login", "--oidc"]).expect("login --oidc");
        assert!(matches!(
            cli.command,
            Command::Admin {
                action: AdminAction::Api(ApiAction::Login {
                    oidc: true,
                    user: None,
                    ..
                }),
                ..
            }
        ));
        // Пароль — не аргумент: только --user и --password-stdin; без --user (и без --oidc) — ошибка разбора.
        let cli = Cli::try_parse_from([
            "thepost",
            "admin",
            "login",
            "--user",
            "a@b.test",
            "--password-stdin",
        ])
        .expect("login --user");
        assert!(matches!(
            cli.command,
            Command::Admin {
                action: AdminAction::Api(ApiAction::Login {
                    user: Some(_),
                    password_stdin: true,
                    oidc: false
                }),
                ..
            }
        ));
        for bad in [
            &["thepost", "admin", "login"][..],
            &[
                "thepost",
                "admin",
                "login",
                "--user",
                "a@b.test",
                "--password",
                "x",
            ],
            &["thepost", "admin", "login", "--user", "a@b.test", "--oidc"],
        ] {
            assert!(Cli::try_parse_from(bad).is_err(), "{bad:?}");
        }
        for args in [
            &["thepost", "admin", "login", "--user", "a@b.test"][..],
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
            &["thepost", "admin", "transport", "domain", "list"],
            &[
                "thepost",
                "admin",
                "transport",
                "domain",
                "delete",
                "b.test",
            ],
            &["thepost", "admin", "transport", "source", "list"],
            &[
                "thepost",
                "admin",
                "transport",
                "source",
                "set",
                "gw",
                "--network",
                "192.0.2.0/24",
                "--network",
                "2001:db8::/32",
                "--xclient",
                "--authserv-id",
                "mx.b.test",
            ],
            &[
                "thepost",
                "admin",
                "transport",
                "source",
                "set",
                "mfu",
                "--client-cert",
                "mfu.b.test",
                "--application",
                "--sender",
                "@b.test",
                "--external",
            ],
            &["thepost", "admin", "transport", "source", "delete", "gw"],
            &["thepost", "admin", "transport", "route", "list"],
            &[
                "thepost",
                "admin",
                "transport",
                "route",
                "set",
                "*.b.test",
                "--tls",
                "required",
                "--smart-host",
                "relay.b.test:587",
                "--egress-ip",
                "192.0.2.1",
            ],
            &[
                "thepost",
                "admin",
                "transport",
                "route",
                "set",
                "*",
                "--pin",
                "ab",
                "--pin",
                "cd",
            ],
            &["thepost", "admin", "transport", "route", "delete", "*"],
            &["thepost", "admin", "policy", "get"],
            &["thepost", "admin", "policy", "get", "--domain", "b.test"],
            &[
                "thepost",
                "admin",
                "policy",
                "set",
                "--account",
                "a@b.test",
                "--file",
                "-",
            ],
            &["thepost", "admin", "group", "list"],
            &["thepost", "admin", "group", "get", "Отдел"],
            &[
                "thepost",
                "admin",
                "group",
                "create",
                "Отдел",
                "--address",
                "o@b.test",
                "--account",
                "a@b.test",
                "--group",
                "Вложенная",
            ],
            &["thepost", "admin", "group", "delete", "Отдел"],
            &["thepost", "admin", "group", "members", "Отдел"],
            &["thepost", "admin", "group", "members", "Отдел", "--set"],
            &[
                "thepost",
                "admin",
                "group",
                "members",
                "Отдел",
                "--set",
                "--account",
                "a@b.test",
            ],
            &["thepost", "admin", "trash", "list", "a@b.test"],
            &[
                "thepost",
                "admin",
                "trash",
                "list",
                "a@b.test",
                "--hidden",
                "--subject",
                "--offset",
                "2",
                "--limit",
                "2",
            ],
            &[
                "thepost", "admin", "trash", "restore", "a@b.test", "m1", "--hidden",
            ],
            &[
                "thepost",
                "admin",
                "audit",
                "search",
                "--since",
                "2026-12-01T00:00:00Z",
                "--actor",
                "a@b.test",
                "--method",
                "put",
                "--route",
                "/api/v1/orgs/{org}/transport",
                "--after",
                "3",
            ],
            &[
                "thepost",
                "admin",
                "delivery",
                "search",
                "--sender",
                "a@b.test",
                "--recipient",
                "c@d.test",
                "--message-id",
                "x@b.test",
                "--until",
                "2026-12-01T00:00:00Z",
                "--after",
                "p",
                "--limit",
                "5",
            ],
            &[
                "thepost", "admin", "account", "list", "--after", "a@b.test", "--limit", "2",
            ],
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
            &[
                "thepost", "admin", "account", "list", "--login", "a@b.test", "--after", "a@b.test",
            ],
            &["thepost", "admin", "domain", "list"],
            &["thepost", "admin", "transport", "domain", "add", "b.test"],
            &["thepost", "admin", "transport", "source", "set", "gw"],
            &[
                "thepost",
                "admin",
                "transport",
                "source",
                "set",
                "gw",
                "--network",
                "192.0.2.0/24",
                "--client-cert",
                "mfu.b.test",
            ],
            &[
                "thepost",
                "admin",
                "transport",
                "source",
                "set",
                "gw",
                "--network",
                "192.0.2.0/24",
                "--sender",
                "@b.test",
            ],
            &[
                "thepost",
                "admin",
                "transport",
                "source",
                "set",
                "gw",
                "--network",
                "192.0.2.0/24",
                "--application",
                "--xclient",
            ],
            &[
                "thepost",
                "admin",
                "transport",
                "route",
                "set",
                "*",
                "--tls",
                "required",
                "--pin",
                "ab",
            ],
            &[
                "thepost",
                "admin",
                "transport",
                "route",
                "set",
                "*",
                "--tls",
                "never",
            ],
            &[
                "thepost",
                "admin",
                "policy",
                "get",
                "--domain",
                "b.test",
                "--account",
                "a@b.test",
            ],
            &["thepost", "admin", "policy", "set"],
            &[
                "thepost",
                "admin",
                "group",
                "members",
                "Отдел",
                "--account",
                "a@b.test",
            ],
            &["thepost", "admin", "trash", "restore", "a@b.test"],
            &["thepost", "admin", "audit", "search", "--after", "x"],
        ] {
            assert!(Cli::try_parse_from(args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn url_from_config_yaml() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.yaml");
        let base = DEV_CONFIG;
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

    /// Admin API tp-admin на хранилищах testkit на loopback: локальный вход подключён, `oidc` — токен OIDC
    /// администратора инсталляции; страница списков — 2 записи (переход по `Link` в каждом списке длиннее).
    async fn admin_api() -> (Arc<tp_admin::Admin>, Url) {
        admin_api_with(tp_auth::AbuseConfig::default()).await
    }

    /// То же с защитой от перебора `abuse`. Учётные данные проверяет путь входа узла ([`crate::services::Guarded`]):
    /// пароль — `PasswordAuthenticator` с паролями приложений, Bearer — `oidc`; адрес клиента — из соединения.
    async fn admin_api_with(abuse: tp_auth::AbuseConfig) -> (Arc<tp_admin::Admin>, Url) {
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
        let bearer = FakeAuthenticator::default();
        bearer.add_token(
            "oidc",
            Principal {
                tenant: TenantId::DEFAULT,
                account: AccountId::new(),
                login: "root@example.test".into(),
                groups: BTreeSet::from(["mail-root".to_owned()]),
                scopes: None,
            },
        );
        let password = tp_auth::PasswordAuthenticator::new(tp_auth::Accounts::new(
            TenantId::DEFAULT,
            Arc::new(meta.clone()),
        ))
        .with_app_passwords(app_passwords.clone());
        let router = tp_auth::CredentialsRouter::default()
            .password(Arc::new(password))
            .bearer(Arc::new(bearer));
        let auth = crate::services::Guarded::new(
            Arc::new(router),
            tp_auth::AbuseGuard::new(abuse, Arc::new(MemAbuseBlocks::default())),
            Arc::new(meta.clone()),
            TenantId::DEFAULT,
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
            contacts: Arc::new(MemAddressBook::new()),
            org: Arc::new(MemAddressBook::new()),
            dynamic: Arc::new(MemAddressBook::new()),
            senders: Arc::new(MemSenderLists::new()),
            mailbox_audit: Arc::new(MemMailboxAudit::new()),
            sync: Arc::new(MemSyncGate::new()),
            oidc_issuer: None,
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
        let router = admin
            .clone()
            .router()
            .into_make_service_with_connect_info::<std::net::SocketAddr>();
        tokio::spawn(async move { axum::serve(listener, router).await });
        let url = Url::parse(&format!("http://{addr}")).expect("url");
        (admin, url)
    }

    /// Приёмка ADM-41: перебор пароля с одного адреса — блокировка `AbuseGuard` по адресу клиента, после неё и
    /// верный пароль не принимается (401).
    #[tokio::test]
    async fn password_login_bruteforce_is_blocked() {
        let (admin, url) = admin_api_with(tp_auth::AbuseConfig {
            ip_failures: 3,
            ..tp_auth::AbuseConfig::default()
        })
        .await;
        let granted = admin
            .init(&InitRequest {
                email: "admin@example.test".into(),
                display_name: None,
                force: false,
            })
            .await
            .expect("init");
        let anon = Client::anonymous(url, &Options::default()).expect("client");
        let args = ApiArgs::default();
        for _ in 0..3 {
            let err = login(&anon, &args, &granted.login, "wrong".into())
                .await
                .expect_err("неверный пароль");
            assert!(err.contains("401"), "{err}");
        }
        let err = login(&anon, &args, &granted.login, granted.password.clone())
            .await
            .expect_err("адрес заблокирован");
        assert!(err.contains("401"), "{err}");
    }

    /// Проверка скрипта, как у узла: `tp_sieve::compile` с пределами секции `sieve`; у тестовой секции глубина
    /// вложенности меньше умолчания, чтобы отказ по пределу секции был виден.
    fn check_script(text: &str) -> Vec<tp_admin::ScriptError> {
        let section = tp_sieve::SieveConfig {
            max_nesting: 2,
            ..tp_sieve::SieveConfig::default()
        };
        tp_sieve::compile(text.as_bytes(), &section.limits())
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

    /// `account …`.
    fn account_action(action: AccountAction) -> ApiAction {
        ApiAction::Account {
            action: Box::new(action),
        }
    }

    /// `group …`.
    fn group_action(action: GroupAction) -> ApiAction {
        ApiAction::Group {
            action: Box::new(action),
        }
    }

    /// `transport domain …`.
    fn domain_action(action: DomainAction) -> ApiAction {
        transport(TransportAction::Domain { action })
    }

    fn transport(action: TransportAction) -> ApiAction {
        ApiAction::Transport {
            action: Box::new(action),
        }
    }

    fn client(url: &Url, token: &str) -> Client {
        Client::new(url.clone(), token.to_owned(), &Options::default()).expect("client")
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
            after: None,
            limit,
        };
        let json = exec(c, true, account_action(action))
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

        // Вход по паролю admin init: сессия; неверный пароль — 401 с подсказкой.
        let anon = Client::anonymous(url.clone(), &Options::default()).expect("client");
        let json_args = ApiArgs {
            json: true,
            ..ApiArgs::default()
        };
        let session: Value = serde_json::from_str(
            &login(&anon, &json_args, &granted.login, granted.password.clone())
                .await
                .expect("login"),
        )
        .expect("json");
        let session = session["session"].as_str().expect("session").to_owned();
        assert!(session.starts_with("tps_"), "{session}");
        let err = login(&anon, &ApiArgs::default(), &granted.login, "wrong".into())
            .await
            .expect_err("неверный пароль");
        assert!(err.contains("401") && err.contains("пароль"), "{err}");
        let err = exec(&anon, false, ApiAction::Whoami)
            .await
            .expect_err("без сессии");
        assert!(err.contains("401"), "{err}");

        let s = client(&url, &session);
        let who = exec(&s, false, ApiAction::Whoami).await.expect("whoami");
        assert!(
            who.contains("admin@example.test") && who.contains("installation_admin"),
            "{who}"
        );

        // Домен admin init уже принят; добавление второго.
        let domains = exec(&s, false, domain_action(DomainAction::List))
            .await
            .expect("domains");
        assert!(domains.contains("example.test"), "{domains}");
        let added = exec(
            &s,
            true,
            domain_action(DomainAction::Create {
                name: "Second.Test".into(),
                relay: None,
            }),
        )
        .await
        .expect("transport domain create");
        let added: Value = serde_json::from_str(&added).expect("json");
        assert_eq!(added["domain"]["name"], "second.test");

        // Учётная запись: создание, поиск, пароль приложения по имени входа.
        let created = exec(
            &s,
            false,
            account_action(AccountAction::Create {
                login: "a@second.test".into(),
                name: Some("Анна".into()),
            }),
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
            account_action(AccountAction::Create {
                login: "a@second.test".into(),
                name: None,
            }),
        )
        .await
        .expect_err("занято");
        assert!(dup.contains("409"), "{dup}");
        let listed = exec(
            &s,
            false,
            account_action(AccountAction::List {
                login: Some("A@second.test".into()),
                prefix: None,
                after: None,
                limit: None,
            }),
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
            exec(&s, false, account_action(action))
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
            account_action(AccountAction::List {
                login: None,
                prefix: None,
                after: None,
                limit: Some(3),
            }),
        )
        .await
        .expect("limit text");
        assert_eq!(text.lines().count(), 5, "{text}");
        assert!(text.contains("есть ещё"), "{text}");
        let text = exec(
            &s,
            false,
            account_action(AccountAction::List {
                login: None,
                prefix: Some("c".into()),
                after: None,
                limit: None,
            }),
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
            account_action(AccountAction::Get {
                account: "nobody@second.test".into(),
            }),
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
            domain_action(DomainAction::Create {
                name: "third.test".into(),
                relay: Some("mx.third.test:25".into()),
            }),
        )
        .await
        .expect("transport domain create relay");
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
            domain_action(DomainAction::Delete { domain: id }),
        )
        .await
        .expect("transport domain delete");
        assert!(removed.contains("удалён"), "{removed}");
        let domains = exec(&s, false, domain_action(DomainAction::List))
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
            domain_action(DomainAction::Create {
                name: "rules.test".into(),
                relay: None,
            }),
            account_action(AccountAction::Create {
                login: account(),
                name: None,
            }),
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
        // Текст верен, но вложенность больше предела секции `sieve`: отказ узла с позицией, как при доставке.
        let deep = dir.path().join("deep.sieve");
        let deep_text = "if true { if true { if true { if true { keep; } } } }
";
        assert!(
            tp_sieve::compile(deep_text.as_bytes(), &tp_sieve::Limits::default()).is_ok(),
            "с пределами по умолчанию скрипт верен"
        );
        std::fs::write(&deep, deep_text).expect("write");
        let err = exec(&c, false, put(&deep)).await.expect_err("422");
        assert!(err.contains("422") && err.contains("строка 1"), "{err}");
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
        let account = account_action;
        for action in [
            domain_action(DomainAction::Create {
                name: "block.test".into(),
                relay: None,
            }),
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

    /// ADM-42: `account set` — псевдонимы (замена и снятие) и скрытие из книги «Организация»; домен не принят — 409;
    /// без ключей — ошибка без запроса.
    #[tokio::test]
    async fn account_set_against_admin_api() {
        let (_admin, url) = admin_api().await;
        let c = client(&url, "oidc");
        let login = || "a@alias.test".to_owned();
        let account = account_action;
        for action in [
            domain_action(DomainAction::Create {
                name: "alias.test".into(),
                relay: None,
            }),
            account(AccountAction::Create {
                login: login(),
                name: None,
            }),
        ] {
            exec(&c, false, action).await.expect("подготовка");
        }
        let set = |aliases: &[&str], no_aliases: bool, hidden_from_gal: Option<bool>| {
            account(AccountAction::Set {
                account: login(),
                aliases: aliases.iter().map(|a| (*a).to_owned()).collect(),
                no_aliases,
                hidden_from_gal,
            })
        };
        let fields = |out: CliResult<String>| {
            let v: Value = serde_json::from_str(&out.expect("ответ")).expect("json");
            (v["aliases"].clone(), v["hidden_from_gal"].clone())
        };
        let out = exec(
            &c,
            true,
            set(&["B@alias.test", "c@alias.test"], false, None),
        )
        .await;
        assert_eq!(
            fields(out),
            (json!(["b@alias.test", "c@alias.test"]), json!(false))
        );
        let out = exec(&c, true, set(&[], false, Some(true))).await;
        assert_eq!(
            fields(out),
            (json!(["b@alias.test", "c@alias.test"]), json!(true))
        );
        let shown = exec(&c, false, set(&[], true, Some(false)))
            .await
            .expect("set");
        assert!(shown.contains("Псевдонимы"), "{shown}");
        let get = account(AccountAction::Get { account: login() });
        assert_eq!(fields(exec(&c, true, get).await), (json!([]), json!(false)));
        let err = exec(&c, false, set(&["x@other.test"], false, None))
            .await
            .expect_err("409");
        assert!(err.contains("409"), "{err}");
        let err = exec(&c, false, set(&[], false, None))
            .await
            .expect_err("без ключей");
        assert!(err.contains("--alias"), "{err}");
    }

    /// `--after` из подсказки вывода (`… --after <имя входа>`); нет подсказки — `None`.
    fn after_hint(text: &str) -> Option<String> {
        text.split("--after ")
            .nth(1)
            .and_then(|rest| rest.split_whitespace().next())
            .map(str::to_owned)
    }

    /// ADM-37: `account list --after` с подсказки предыдущей страницы — все записи по порядку без пропусков и
    /// повторов; подсказка сохраняет `--prefix`.
    #[tokio::test]
    async fn account_list_after_pages() {
        let (_admin, url) = admin_api().await;
        let c = client(&url, "oidc");
        let domain = DomainAction::Create {
            name: "page.test".into(),
            relay: None,
        };
        exec(&c, false, domain_action(domain))
            .await
            .expect("domain");
        let logins: Vec<String> = (1..=5).map(|i| format!("u{i}@page.test")).collect();
        for login in logins.iter().rev() {
            let action = AccountAction::Create {
                login: login.clone(),
                name: None,
            };
            exec(&c, false, account_action(action))
                .await
                .expect("account create");
        }
        let list = |after: Option<String>| {
            account_action(AccountAction::List {
                login: None,
                prefix: Some("u".into()),
                after,
                limit: Some(2),
            })
        };
        let mut seen = Vec::new();
        let mut after = None;
        for _ in 0..logins.len() {
            let page: Value = serde_json::from_str(
                &exec(&c, true, list(after.clone()))
                    .await
                    .expect("page json"),
            )
            .expect("json");
            seen.extend(
                page.as_array()
                    .expect("array")
                    .iter()
                    .map(|a| a["login"].as_str().expect("login").to_owned()),
            );
            let text = exec(&c, false, list(after.clone())).await.expect("page");
            after = after_hint(&text);
            match &after {
                Some(next) => {
                    assert!(text.contains("--prefix u"), "{text}");
                    assert_eq!(seen.last(), Some(next), "{text}");
                }
                None => break,
            }
        }
        assert_eq!(after, None, "последняя страница без подсказки");
        assert_eq!(seen, logins);
    }

    /// Начало config.yaml профиля dev без внешних секретов.
    const DEV_CONFIG: &str = "domain: example.test\nprofile: dev\nroles: [admin]\nservice_http:\n  allow_plaintext: true\ndatabase:\n  url: postgres://thepost@db/thepost?sslmode=disable\n  tls: { allow_plaintext: true }\nblob:\n  url: file:///var/lib/thepost/blob\n";

    /// ADM-39: `login --oidc` — вход с устройства у поставщика `auth.oidc` (заглушка RFC 8628): код устройства,
    /// затем токен (код возврата 0); без `cli.oidc.client_id` — отказ без запроса токена.
    #[tokio::test]
    async fn login_oidc_gets_token() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        use axum::routing::{get, post};
        use axum::{Json, Router};

        let polls = Arc::new(AtomicUsize::new(0));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let issuer = format!("http://{}", listener.local_addr().expect("addr"));
        let doc = json!({
            "issuer": issuer,
            "device_authorization_endpoint": format!("{issuer}/device"),
            "token_endpoint": format!("{issuer}/token"),
            "jwks_uri": format!("{issuer}/jwks"),
        });
        let device = json!({
            "device_code": "dev-1",
            "user_code": "WDJB-MJHT",
            "verification_uri": "https://sso.example.test/device",
            "expires_in": 600,
            "interval": 0,
        });
        let counter = polls.clone();
        let router = Router::new()
            .route(
                "/.well-known/openid-configuration",
                get(move || async move { Json(doc) }),
            )
            .route("/device", post(move || async move { Json(device) }))
            .route(
                "/token",
                post(move || {
                    let counter = counter.clone();
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Json(json!({ "access_token": "at-1", "token_type": "Bearer" }))
                    }
                }),
            );
        tokio::spawn(async move { axum::serve(listener, router).await });

        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.yaml");
        let config = |cli: &str| {
            format!(
                "{DEV_CONFIG}http:\n  admin: {{ listen: \"127.0.0.1:9443\", allow_plaintext: true }}\n\
                 auth:\n  oidc:\n    issuer: \"{issuer}\"\n    audience: [thepost]\n    \
                 tls: {{ allow_plaintext: true }}\n{cli}"
            )
        };
        let login = || ApiAction::Login {
            user: None,
            password_stdin: false,
            oidc: true,
        };
        std::fs::write(&path, config("cli:\n  oidc:\n    client_id: thepost-cli\n"))
            .expect("write");
        assert_eq!(command(&path, ApiArgs::default(), login()).await, 0);
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        std::fs::write(&path, config("")).expect("write");
        assert_eq!(command(&path, ApiArgs::default(), login()).await, 1);
        assert_eq!(polls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn login_help_explains_oidc() {
        use clap::CommandFactory;

        let mut cmd = crate::cli::Cli::command();
        let mut cmd = &mut cmd;
        for name in ["admin", "login"] {
            cmd = cmd.find_subcommand_mut(name).expect(name);
        }
        let help = cmd.render_long_help().to_string();
        assert!(
            help.contains("--oidc") && help.contains("RFC 8628") && help.contains(ENV_TOKEN),
            "{help}"
        );
    }

    /// `cli.oidc` без `client_id` — вход с устройства недоступен (понятная ошибка до запросов к поставщику);
    /// с ним — настройки клиента как в секции.
    #[test]
    fn device_settings_need_client_id() {
        let e = device_settings(&CliOidc::default()).expect_err("no client");
        assert!(e.contains("cli.oidc.client_id"), "{e}");
        let s = device_settings(&CliOidc {
            client_id: Some("thepost-cli".into()),
            ..CliOidc::default()
        })
        .expect("settings");
        assert_eq!(s.client_id, "thepost-cli");
        assert_eq!(s.scope, "openid");
        assert_eq!(s.max_wait, CliOidc::default().max_wait);
    }

    /// ADM-38: политики, группы, корзина, журнал аудита, журнал доставки, источники и маршруты транспорта —
    /// каждая команда против роутера tp-admin.
    #[tokio::test]
    async fn resource_commands_against_admin_api() {
        let (_admin, url) = admin_api().await;
        let c = client(&url, "oidc");
        let a = || "a@res.test".to_owned();
        let b = || "b@res.test".to_owned();
        let domain = DomainAction::Create {
            name: "res.test".into(),
            relay: None,
        };
        exec(&c, false, domain_action(domain))
            .await
            .expect("domain");
        for login in [a(), b()] {
            let action = AccountAction::Create { login, name: None };
            exec(&c, false, account_action(action))
                .await
                .expect("account");
        }
        let json = |out: CliResult<String>| -> Value {
            serde_json::from_str(&out.expect("ответ")).expect("json")
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let write = |name: &str, text: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, text).expect("write");
            path
        };

        // Политика: организация, ящик, домен; действующее значение наследуется сверху.
        let policy = |action| ApiAction::Policy {
            action: Box::new(action),
        };
        let level = |domain: Option<&str>, account: Option<String>| PolicyLevel {
            domain: domain.map(str::to_owned),
            account,
        };
        let file = write(
            "org.json",
            r#"{"quota_bytes": 1000, "trash_retention": "30d"}"#,
        );
        let set = PolicyAction::Set {
            level: level(None, None),
            file,
        };
        assert_eq!(json(exec(&c, true, policy(set)).await)["quota_bytes"], 1000);
        let set = PolicyAction::Set {
            level: level(None, Some(a())),
            file: write("account.json", r#"{"send_per_hour": 5}"#),
        };
        exec(&c, false, policy(set)).await.expect("account policy");
        let get = |l| policy(PolicyAction::Get { level: l });
        let got = json(exec(&c, true, get(level(None, Some(a())))).await);
        assert_eq!(got["policy"]["send_per_hour"], 5);
        assert_eq!(got["policy"]["quota_bytes"], Value::Null);
        assert_eq!(got["effective"]["quota_bytes"], 1000);
        let text = exec(&c, false, get(level(None, None)))
            .await
            .expect("org policy");
        assert!(
            text.contains("ДЕЙСТВУЕТ")
                && text
                    .lines()
                    .any(|l| l.starts_with("quota_bytes") && l.contains("1000")),
            "{text}"
        );
        let got = json(exec(&c, true, get(level(Some("RES.test"), None))).await);
        assert_eq!(got["policy"]["trash_retention"], Value::Null);
        assert!(got["effective"]["trash_retention"].is_string(), "{got}");
        let set = PolicyAction::Set {
            level: level(None, None),
            file: write("bad.json", r#"{"quota": 1}"#),
        };
        let err = exec(&c, false, policy(set)).await.expect_err("поле");
        assert!(err.contains("quota"), "{err}");

        // Группы: создание с участником, список, чтение по адресу, замена участников, удаление.
        let group = group_action;
        let create = GroupAction::Create {
            name: "Отдел".into(),
            address: Some("otdel@res.test".into()),
            accounts: vec![a()],
            groups: vec![],
        };
        let created = json(exec(&c, true, group(create)).await);
        let a_id = created["members"][0]["account"]
            .as_str()
            .expect("account")
            .to_owned();
        assert_eq!(created["names"][a_id.as_str()], "a@res.test");
        let listed = exec(&c, false, group(GroupAction::List))
            .await
            .expect("group list");
        assert!(
            listed.contains("Отдел") && listed.contains("otdel@res.test"),
            "{listed}"
        );
        let shown = exec(
            &c,
            false,
            group(GroupAction::Get {
                group: "OTDEL@res.test".into(),
            }),
        )
        .await
        .expect("group get");
        assert!(
            shown.contains("a@res.test") && shown.contains("anyone"),
            "{shown}"
        );
        let members = |set, accounts| {
            group(GroupAction::Members {
                group: "отдел".into(),
                set,
                accounts,
                groups: vec![],
            })
        };
        let replaced = json(exec(&c, true, members(true, vec![b()])).await);
        assert_eq!(replaced.as_array().map(Vec::len), Some(1), "{replaced}");
        let shown = exec(&c, false, members(false, vec![]))
            .await
            .expect("members");
        assert!(
            shown.contains("b@res.test") && !shown.contains("a@res.test"),
            "{shown}"
        );
        let delete = GroupAction::Delete {
            group: "Отдел".into(),
        };
        exec(&c, false, group(delete)).await.expect("group delete");
        let get = GroupAction::Get {
            group: "Отдел".into(),
        };
        let err = exec(&c, false, group(get)).await.expect_err("нет группы");
        assert!(err.contains("не найдена"), "{err}");

        // Корзина: пустая видимая и скрытая, срок — действующий; восстановление чужого письма — отказ узла.
        let trash = |action| ApiAction::Trash { action };
        for hidden in [false, true] {
            let list = TrashAction::List {
                account: a(),
                hidden,
                subject: true,
                offset: 0,
                limit: None,
            };
            let text = exec(&c, false, trash(list)).await.expect("trash list");
            assert!(
                text.starts_with("нет записей")
                    && text.contains("Срок корзины:")
                    && !text.contains("по умолчанию"),
                "{text}"
            );
        }
        for (hidden, path) in [
            (false, "/trash/{message}/restore: 4"),
            (true, "/trash/hidden/{message}/restore: 404"),
        ] {
            let restore = TrashAction::Restore {
                account: a(),
                message: tp_contracts::MessageId::new().to_string(),
                hidden,
            };
            let err = exec(&c, false, trash(restore))
                .await
                .expect_err("нет письма");
            assert!(err.contains(path), "{err}");
        }

        // Журнал аудита: записи политик (PUT) по одной на страницу через подсказку --after, без повторов.
        let audit = |since: Option<&str>, after| ApiAction::Audit {
            action: Box::new(AuditAction::Search {
                since: since.map(str::to_owned),
                until: None,
                actor: None,
                method: Some("put".into()),
                route: None,
                limit: Some(1),
                after,
            }),
        };
        let (mut routes, mut seqs, mut after) = (Vec::new(), Vec::new(), None);
        for _ in 0..10 {
            let page = json(exec(&c, true, audit(None, after)).await);
            for e in page["entries"].as_array().expect("entries") {
                routes.push(e["route"].as_str().expect("route").to_owned());
                seqs.push(e["seq"].as_u64().expect("seq"));
            }
            let text = exec(&c, false, audit(None, after))
                .await
                .expect("audit text");
            after = page["next"].as_u64();
            assert_eq!(after_hint(&text), after.map(|n| n.to_string()), "{text}");
            if after.is_none() {
                break;
            }
        }
        assert_eq!(after, None, "последняя страница без подсказки");
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "{seqs:?}");
        for route in [
            "/api/v1/orgs/{org}/policy",
            "/api/v1/orgs/{org}/accounts/{account}/policy",
        ] {
            assert!(routes.iter().any(|r| r == route), "{routes:?}");
        }
        let err = exec(&c, false, audit(Some("завтра"), None))
            .await
            .expect_err("время");
        assert!(err.contains("RFC 3339"), "{err}");

        // Журнал доставки: поиск по отправителю.
        let search = DeliveryAction::Search {
            sender: Some(a()),
            recipient: None,
            message_id: None,
            since: Some("2026-01-01T00:00:00Z".into()),
            until: None,
            limit: None,
            after: None,
        };
        let found = exec(
            &c,
            false,
            ApiAction::Delivery {
                action: Box::new(search),
            },
        )
        .await
        .expect("delivery");
        assert_eq!(found, "нет записей\n");

        // Доверенные источники: шлюз по сети, приложение по сертификату; удаление; неверная сеть — отказ.
        let source = |action| transport(TransportAction::Source { action });
        let set = |name: &str, networks: Vec<&str>, cert: Option<&str>, senders: Vec<&str>| {
            source(SourceAction::Set {
                name: name.into(),
                networks: networks.into_iter().map(str::to_owned).collect(),
                client_cert: cert.map(str::to_owned),
                application: !senders.is_empty(),
                senders: senders.into_iter().map(str::to_owned).collect(),
                external: false,
                xclient: cert.is_none(),
                authserv_id: cert.is_none().then(|| "mx.res.test".to_owned()),
            })
        };
        let done = exec(&c, false, set("gw", vec!["192.0.2.0/24"], None, vec![]))
            .await
            .expect("source gw");
        assert!(done.starts_with("Источник gw записан"), "{done}");
        exec(
            &c,
            false,
            set("mfu", vec![], Some("mfu.res.test"), vec!["@res.test"]),
        )
        .await
        .expect("source mfu");
        let listed = exec(&c, false, source(SourceAction::List))
            .await
            .expect("source list");
        for part in [
            "192.0.2.0/24",
            "XCLIENT",
            "authserv-id mx.res.test",
            "сертификат mfu.res.test",
            "приложение: @res.test",
        ] {
            assert!(listed.contains(part), "{part}: {listed}");
        }
        let delete = SourceAction::Delete { name: "gw".into() };
        exec(&c, false, source(delete))
            .await
            .expect("source delete");
        let listed = json(exec(&c, true, source(SourceAction::List)).await);
        assert_eq!(
            listed["sources"].as_array().map(Vec::len),
            Some(1),
            "{listed}"
        );
        let err = exec(&c, false, set("bad", vec!["не сеть"], None, vec![]))
            .await
            .expect_err("сеть");
        assert!(err.starts_with("источник:"), "{err}");

        // Политики доменов назначения: TLS, smart host, закреплённый ключ; удаление; неверный адрес — отказ.
        let route = |action| transport(TransportAction::Route { action });
        let set = |pattern: &str, tls, pins: Vec<String>, egress_ip: Option<&str>| {
            route(RouteAction::Set {
                pattern: pattern.into(),
                tls,
                pins,
                smart_host: Some("relay.example.net:587".into()),
                egress_ip: egress_ip.map(str::to_owned),
            })
        };
        let done = exec(
            &c,
            false,
            set("Example.org", TlsMode::Required, vec![], Some("192.0.2.1")),
        )
        .await
        .expect("route set");
        assert!(done.starts_with("Политика Example.org записана"), "{done}");
        let pin = "ab".repeat(32);
        let pinned = format!("pinned: {pin}");
        exec(
            &c,
            false,
            set("*", TlsMode::Opportunistic, vec![pin.clone()], None),
        )
        .await
        .expect("route pin");
        let listed = exec(&c, false, route(RouteAction::List))
            .await
            .expect("route list");
        for part in [
            "example.org",
            "required",
            "relay.example.net:587",
            "192.0.2.1",
            pinned.as_str(),
        ] {
            assert!(listed.contains(part), "{part}: {listed}");
        }
        let delete = RouteAction::Delete {
            pattern: "example.org".into(),
        };
        exec(&c, false, route(delete)).await.expect("route delete");
        let listed = json(exec(&c, true, route(RouteAction::List)).await);
        assert_eq!(
            listed["routes"].as_array().map(Vec::len),
            Some(1),
            "{listed}"
        );
        let err = exec(
            &c,
            false,
            set("x.test", TlsMode::Required, vec![], Some("не адрес")),
        )
        .await
        .expect_err("адрес");
        assert!(err.contains("--egress-ip"), "{err}");
    }
}
