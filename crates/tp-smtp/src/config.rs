//! Секция `smtp` config.yaml (SRS 4.1, 5.5). Поля необязательны, неизвестное поле — ошибка.
//!
//! ```yaml
//! smtp:
//!   hostname: mx.example.org
//!   inbound:
//!     - listen: "[::]:25"
//!   submission:
//!     - listen: "[::]:587"
//!     - listen: "[::]:465"
//!       implicit_tls: true
//!   limits:
//!     max_message_size: 52428800
//!     max_recipients: 100
//!   timeouts:
//!     command: 5m
//!     data: 10m
//!   milter:
//!     filters:
//!       - name: rspamd
//!         address: "127.0.0.1:11332"
//!         submission: true
//!         on_failure: accept
//!   queue:
//!     retry: [5m, 15m, 30m, 1h, 2h, 4h, 8h]
//!     expire: 5d
//!     scheduler:
//!       domain_concurrency: 20
//!       backoff_min: 1m
//!   dkim:
//!     domains:
//!       example.org:
//!         selector: s2026
//!         keys:
//!           s2026: { algorithm: rsa-sha256, key: { secret: "thepost/dkim/example.org#s2026" } }
//!           e2026: { algorithm: ed25519-sha256, key: { secret: "thepost/dkim/example.org#e2026" } }
//!   addressing:
//!     subaddress_separators: "+"
//!   screen:
//!     mode: enforce
//!     greet_delay: 6s
//!     exempt: ["127.0.0.0/8", "10.0.0.0/8"]
//!   forward:
//!     srs:
//!       domain: example.org
//!       key: { secret: "thepost/srs#key" }
//!       max_age_days: 21
//!     arc:
//!       domain: example.org
//!       selector: arc2026
//!       algorithm: ed25519-sha256
//!       key: { secret: "thepost/arc/example.org#arc2026" }
//!   learn:
//!     enabled: true
//!     url: "https://rspamd.example.org:11334"
//!     password: { secret: "thepost/rspamd#controller" }
//! ```
//!
//! Слушатели (`inbound`, `submission`) применяются при запуске роли; лимиты и таймауты — к новым сессиям
//! без перезапуска.

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tp_contracts::config::duration;
use tp_contracts::config::duration::HumanDuration;
use tp_platform::EndpointConfig;

/// Секция `smtp`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SmtpConfig {
    /// Имя узла в приветствии, ответе `EHLO` и заголовке `Received`.
    pub hostname: String,
    /// Слушатели роли smtp-in (приём почты, 25 с STARTTLS). Требует перезапуска.
    pub inbound: Vec<EndpointConfig>,
    /// Слушатели роли submission: 587 (STARTTLS) и 465 (`implicit_tls: true`). Требует перезапуска.
    pub submission: Vec<EndpointConfig>,
    /// Лимиты сессии.
    pub limits: Limits,
    /// Таймауты сессии.
    pub timeouts: Timeouts,
    /// Локальная доставка.
    pub delivery: DeliveryConfig,
    /// Проверки SPF, DKIM, DMARC, ARC.
    pub authcheck: AuthCheckConfig,
    /// Фильтры Milter (антиспам, антивирус).
    pub milter: MilterConfig,
    /// Очередь исходящей почты и отправка (роль smtp-out).
    pub queue: QueueConfig,
    /// DKIM-подпись исходящей почты.
    pub dkim: DkimConfig,
    /// Экран соединений smtp-in (postscreen).
    pub screen: ScreenConfig,
    /// Адресация: подадреса, переписывание адресов конверта.
    pub addressing: AddressingConfig,
    /// Обучение антиспама по перемещению писем в «Спам» и из него.
    pub learn: LearnConfig,
    /// Пересылка наружу: SRS-переписывание отправителя и ARC-подпись.
    pub forward: ForwardConfig,
}

impl Default for SmtpConfig {
    fn default() -> Self {
        Self {
            hostname: "localhost".into(),
            inbound: Vec::new(),
            submission: Vec::new(),
            limits: Limits::default(),
            timeouts: Timeouts::default(),
            delivery: DeliveryConfig::default(),
            authcheck: AuthCheckConfig::default(),
            milter: MilterConfig::default(),
            queue: QueueConfig::default(),
            dkim: DkimConfig::default(),
            screen: ScreenConfig::default(),
            addressing: AddressingConfig::default(),
            learn: LearnConfig::default(),
            forward: ForwardConfig::default(),
        }
    }
}

/// Пересылка наружу (SRS 4.1; [`crate::forward`]): перенаправление Sieve и правилом потока почты, пересылки модулей.
/// Без секций письмо пересылается как раньше — с исходным отправителем и без ARC.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ForwardConfig {
    /// SRS-переписывание отправителя конверта; `None` — выключено.
    pub srs: Option<SrsConfig>,
    /// ARC-подпись пересылаемых писем (RFC 8617); `None` — выключена.
    pub arc: Option<ArcConfig>,
    /// Время жизни ключей в кэше (затем перечитываются из хранилища секретов).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub key_refresh: Duration,
}

impl Default for ForwardConfig {
    fn default() -> Self {
        Self {
            srs: None,
            arc: None,
            key_refresh: Duration::from_secs(3600),
        }
    }
}

/// SRS: домен адресов (его MX — этот кластер), ключ подписи и срок адреса.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SrsConfig {
    /// Домен адресов SRS (принятый домен организации).
    pub domain: String,
    /// Ключ подписи адресов в хранилище секретов.
    pub key: tp_contracts::secrets::SecretRef,
    /// Прежние ключи (ротация): адреса, подписанные ими, ещё принимаются.
    #[serde(default)]
    pub previous_keys: Vec<tp_contracts::secrets::SecretRef>,
    /// Срок адреса SRS, дней (уведомления о недоставке позже — `550`).
    #[serde(default = "srs_max_age")]
    pub max_age_days: u32,
}

fn srs_max_age() -> u32 {
    21
}

/// ARC-подпись: домен и селектор подписи (`<селектор>._domainkey.<домен>` в DNS), ключ.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArcConfig {
    /// Домен подписи (`d=`).
    pub domain: String,
    /// Селектор (`s=`).
    pub selector: String,
    /// Алгоритм.
    pub algorithm: DkimAlgorithm,
    /// Закрытый ключ (PEM) в хранилище секретов.
    pub key: tp_contracts::secrets::SecretRef,
    /// Подписываемые заголовки.
    #[serde(default = "arc_headers")]
    pub headers: Vec<String>,
}

fn arc_headers() -> Vec<String> {
    [
        "From",
        "To",
        "Cc",
        "Subject",
        "Date",
        "Message-ID",
        "Reply-To",
    ]
    .map(String::from)
    .to_vec()
}

/// Обучение антиспама (SRS 4.1, по модели imapsieve Dovecot; [`crate::learn`]): письмо, перенесённое
/// пользователем в «Спам», — в Rspamd controller на `learnspam`, перенесённое из «Спама» (не в корзину) — на
/// `learnham`. Выключено по умолчанию.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct LearnConfig {
    /// Включено.
    pub enabled: bool,
    /// Адрес Rspamd controller: `https://…`; `http://` — только для адресов loopback (controller на том же узле).
    pub url: Option<String>,
    /// Пароль controller (заголовок `Password`) в хранилище секретов; `None` — без пароля.
    pub password: Option<tp_contracts::secrets::SecretRef>,
    /// Таймаут запроса к controller.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub timeout: Duration,
    /// Письма крупнее (байт) на обучение не передаются.
    pub max_message_size: u64,
    /// Сколько последних обученных писем помнить, чтобы повтор пачки событий не обучал повторно.
    pub remember: usize,
}

impl Default for LearnConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: None,
            password: None,
            timeout: Duration::from_secs(30),
            max_message_size: 10 * 1024 * 1024,
            remember: 10_000,
        }
    }
}

/// Адресация (SRS 4.1; [`crate::address`]). Применяется без перезапуска (к новым сессиям).
///
/// Таблица переписывания адресов конверта — в настройках транспорта организации (Admin API,
/// `TenantTransport::rewrites`, SMTP-29); ключ `rewrite` — ошибка разбора с подсказкой.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AddressingConfig {
    /// Разделители подадреса `user+tag` (любой из символов); пусто — подадреса выключены.
    pub subaddress_separators: String,
    /// Упразднённый ключ `rewrite`: любое значение — ошибка разбора с подсказкой (в схеме — `deprecated`, чтобы
    /// разбор, а не отбрасывание неизвестного параметра, сообщил, куда перенесена таблица).
    #[serde(deserialize_with = "rewrite_moved")]
    #[schemars(schema_with = "rewrite_moved_schema")]
    pub rewrite: RewriteMoved,
}

impl Default for AddressingConfig {
    fn default() -> Self {
        Self {
            subaddress_separators: "+".into(),
            rewrite: RewriteMoved,
        }
    }
}

/// Упразднённый ключ `smtp.addressing.rewrite` (SMTP-29): значения нет, задать его в config.yaml нельзя.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RewriteMoved;

/// Сообщение разбора для упразднённого ключа `smtp.addressing.rewrite`.
const REWRITE_MOVED: &str = "smtp.addressing.rewrite is no longer supported: envelope address rewrite rules \
     are per-organization transport settings, manage them through the Admin API (TenantTransport::rewrites)";

fn rewrite_moved_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    schemars::json_schema!({
        "deprecated": true,
        "description": "Removed: envelope address rewrite rules are managed through the Admin API.",
    })
}

fn rewrite_moved<'de, D: serde::Deserializer<'de>>(
    _: D,
) -> std::result::Result<RewriteMoved, D::Error> {
    Err(serde::de::Error::custom(REWRITE_MOVED))
}

/// Экран соединений smtp-in (SRS 4.1, модель postscreen Postfix; [`crate::screen`]). Применяется к новым
/// соединениям без перезапуска.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ScreenConfig {
    /// Экран включён.
    pub enabled: bool,
    /// Что делать с нарушителем.
    pub mode: ScreenMode,
    /// Задержка приветствия: после первой строки `220-` сервер ждёт; клиент, заговоривший раньше, нарушает
    /// протокол. `0` — без проверки.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub greet_delay: Duration,
    /// Проверять конвейер команд без PIPELINING (до `EHLO`, после `HELO`, вслед за `EHLO`).
    pub pipelining: bool,
    /// Соединений с адреса за `rate_window`; сверх — отказ. `0` — без проверки.
    pub max_rate: u32,
    /// Окно подсчёта частоты соединений.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub rate_window: Duration,
    /// Срок, на который адрес, прошедший проверки, освобождается от них.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub pass_ttl: Duration,
    /// Срок, на который нарушитель отклоняется без проверки (режим `enforce`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub reject_ttl: Duration,
    /// Адресов в кэше результатов и в таблице частоты.
    pub cache_size: usize,
    /// Сети без проверок (CIDR или адрес): узлы организации, доверенные шлюзы.
    pub exempt: Vec<String>,
}

impl Default for ScreenConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: ScreenMode::default(),
            greet_delay: Duration::from_secs(6),
            pipelining: true,
            max_rate: 60,
            rate_window: Duration::from_secs(60),
            pass_ttl: Duration::from_secs(24 * 3600),
            reject_ttl: Duration::from_secs(10 * 60),
            cache_size: 100_000,
            exempt: vec!["127.0.0.0/8".into(), "::1/128".into()],
        }
    }
}

/// Действие экрана при нарушении.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ScreenMode {
    /// Отказ и закрытие соединения; адрес запоминается на `reject_ttl`.
    #[default]
    Enforce,
    /// Только журнал и метрика: сессия продолжается.
    Log,
}

/// Лимиты сессии SMTP.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Наибольший размер письма, байт (объявляется в `SIZE`).
    pub max_message_size: usize,
    /// Получателей в одной транзакции.
    pub max_recipients: usize,
    /// Ошибочных команд за сессию; затем соединение закрывается (`421`).
    pub max_errors: u32,
    /// Одновременных соединений на слушатель; сверх лимита — `421` и закрытие.
    pub max_connections: usize,
    /// Неудачных `AUTH` подряд; затем адрес клиента блокируется (`ratelimit.block_duration`) и соединение
    /// закрывается.
    pub max_auth_failures: u32,
    /// Писем в час на учётную запись (submission, скользящее окно); `0` — без лимита.
    pub messages_per_hour: u32,
    /// Получателей в час на учётную запись (сумма по письмам); `0` — без лимита.
    pub recipients_per_hour: u32,
    /// Предел учётных записей со счётчиками лимитов отправки на узле.
    pub max_tracked_accounts: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_message_size: 50 * 1024 * 1024,
            max_recipients: 100,
            max_errors: 10,
            max_connections: 1000,
            max_auth_failures: 3,
            messages_per_hour: 1000,
            recipients_per_hour: 5000,
            max_tracked_accounts: 100_000,
        }
    }
}

/// Таймауты сессии SMTP (RFC 5321, 4.5.3.2).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    /// Ожидание команды.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub command: Duration,
    /// Ожидание очередного блока данных письма (`DATA`, `BDAT`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub data: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            command: Duration::from_secs(5 * 60),
            data: Duration::from_secs(10 * 60),
        }
    }
}

/// Локальная доставка.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DeliveryConfig {
    /// Локальные домены сверх доменов каталога (установка без LDAP, переходный период).
    pub local_domains: Vec<String>,
    /// Время жизни снимка каталога (домены, псевдонимы, группы) для разрешения получателей.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub directory_refresh: Duration,
    /// Ответ на `RCPT TO` получателю с переполненным ящиком (проверка до `250`, SRS 4.1).
    pub quota_reject: QuotaReject,
    /// Автоответов Sieve учётной записи в час (защита от рассылки автоответами, SMTP-32); 0 — автоответы не
    /// отправляются. Период ответа одному отправителю (`:days`) проверяют правила.
    pub vacation_rate: u32,
    /// Наибольшая длина цепочки перенаправлений правилами Sieve (SMTP-34): письмо, перенаправленное больше этого
    /// числа раз, правилами получателя не обрабатывается и остаётся во «Входящих»; 0 — перенаправленные письма
    /// правилами не обрабатываются.
    pub redirect_hops: u32,
    /// Перенаправлений одного письма правилами Sieve за одну доставку (защита от размножения письма встречными
    /// правилами): сверх этого числа получатели перенаправления получают письмо во «Входящие» без правил.
    pub redirect_total: u32,
}

impl Default for DeliveryConfig {
    fn default() -> Self {
        Self {
            local_domains: Vec::new(),
            directory_refresh: Duration::from_secs(5 * 60),
            quota_reject: QuotaReject::default(),
            vacation_rate: 60,
            redirect_hops: 5,
            redirect_total: 50,
        }
    }
}

/// Отказ получателю с переполненным ящиком.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum QuotaReject {
    /// Временный отказ `452 4.2.2`: отправитель повторит, пока владелец освобождает место.
    #[default]
    Temporary,
    /// Постоянный отказ `552 5.2.2`: отправитель сразу получает уведомление о недоставке.
    Permanent,
}

/// Проверки SPF, DKIM, DMARC, ARC входящей почты.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AuthCheckConfig {
    /// Записей DNS каждого типа (TXT, MX, A, AAAA, PTR) в кэше проверок. Требует перезапуска.
    pub dns_cache_size: usize,
}

impl Default for AuthCheckConfig {
    fn default() -> Self {
        Self {
            dns_cache_size: 10_000,
        }
    }
}

/// Фильтры Milter (SRS 4.1): цепочка в порядке перечисления.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MilterConfig {
    /// Фильтры; пусто — письма не фильтруются.
    pub filters: Vec<MilterFilter>,
}

/// Фильтр Milter.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct MilterFilter {
    /// Имя (журнал, заголовок о недоступности).
    pub name: String,
    /// Адрес `хост:порт` (TCP).
    pub address: String,
    /// Проверять входящую почту (smtp-in).
    pub inbound: bool,
    /// Проверять почту пользователей (submission).
    pub submission: bool,
    /// Ожидание соединения с фильтром.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub connect_timeout: Duration,
    /// Ожидание ответа на команду.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub command_timeout: Duration,
    /// Ожидание итога после конца письма (`progress` фильтра продлевает ожидание).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub end_timeout: Duration,
    /// Что делать, если фильтр недоступен.
    pub on_failure: MilterOnFailure,
}

impl Default for MilterFilter {
    fn default() -> Self {
        Self {
            name: "milter".into(),
            address: "127.0.0.1:11332".into(),
            inbound: true,
            submission: false,
            connect_timeout: Duration::from_secs(5),
            command_timeout: Duration::from_secs(10),
            end_timeout: Duration::from_secs(60),
            on_failure: MilterOnFailure::default(),
        }
    }
}

/// Политика при недоступности фильтра.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MilterOnFailure {
    /// Принять с пометкой: заголовок `X-ThePost-Milter: <имя>; unavailable`.
    #[default]
    Accept,
    /// Временный отказ (`451 4.7.1`), отправитель повторит.
    Tempfail,
}

/// Очередь исходящей почты (SRS 4.1): расписание повторов, срок доставки, отправка. Применяется без перезапуска.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct QueueConfig {
    /// Интервалы до повтора после 1-й, 2-й, … неудачи; последний повторяется до `expire`.
    pub retry: Vec<HumanDuration>,
    /// Срок доставки от постановки в очередь; затем — окончательный отказ и уведомление отправителю.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub expire: Duration,
    /// Уведомление о задержке (DSN `delayed`) — однажды, когда письмо в очереди дольше этого срока.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub delay_notify: Duration,
    /// Пауза опроса очереди, когда записей со сроком попытки нет.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub poll_interval: Duration,
    /// Записей за один захват.
    pub batch: usize,
    /// Одновременно обрабатываемых записей на узле.
    pub concurrency: usize,
    /// Лиза записи: за это время узел должен завершить попытку, иначе запись берёт другой узел.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub lease: Duration,
    /// Таймаут соединения и каждой команды SMTP (RFC 5321, 4.5.3.2).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub timeout: Duration,
    /// Узел-ретранслятор `хост:порт` (smarthost) вместо MX получателя; политика домена назначения со своим
    /// `smart_host` (в том числе `*`) точнее.
    pub relay: Option<String>,
    /// Порт MX.
    pub port: u16,
    /// MX-узлов, которые пробуются за попытку.
    pub max_mx: usize,
    /// Исходящий адрес (egress-IP, согласованный с PTR и SPF); `None` — выбирает система. `egress_ip` политики
    /// домена назначения точнее.
    pub source_ip: Option<std::net::IpAddr>,
    /// TLS при отправке.
    pub tls: OutboundTls,
    /// Политики TLS доменов назначения: MTA-STS и DANE (при доставке на MX, не через `relay`).
    pub policy: PolicyConfig,
    /// Планировщик по доменам назначения.
    pub scheduler: SchedulerConfig,
    /// Пауза перед повторной подпиской на сигналы настроек транспорта (политики доменов назначения).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub resubscribe: Duration,
}

impl Default for QueueConfig {
    fn default() -> Self {
        let m = |n: u64| HumanDuration(Duration::from_secs(n * 60));
        Self {
            retry: vec![m(5), m(15), m(30), m(60), m(120), m(240), m(480)],
            expire: Duration::from_secs(5 * 24 * 3600),
            delay_notify: Duration::from_secs(4 * 3600),
            poll_interval: Duration::from_secs(5),
            batch: 32,
            concurrency: 16,
            lease: Duration::from_secs(10 * 60),
            timeout: Duration::from_secs(5 * 60),
            relay: None,
            port: 25,
            max_mx: 5,
            source_ip: None,
            tls: OutboundTls::default(),
            policy: PolicyConfig::default(),
            scheduler: SchedulerConfig::default(),
            resubscribe: Duration::from_secs(5),
        }
    }
}

/// Планировщик исходящей очереди по доменам назначения (SRS 4.1, модель qmgr Postfix;
/// [`crate::queue::sched`]). Применяется без перезапуска.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SchedulerConfig {
    /// Одновременных отправок в один домен (предел окна медленного старта).
    pub domain_concurrency: usize,
    /// Начальное окно нового домена (медленный старт): +1 после каждой отправки с ответом сервера.
    pub initial_concurrency: usize,
    /// Отправок в домен за `rate_window`; `0` — без лимита.
    pub domain_rate: u32,
    /// Окно лимита скорости.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub rate_window: Duration,
    /// Ожидание места в домене или пуле; затем получатели переносятся на `busy_delay`.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub wait: Duration,
    /// Перенос получателей занятого домена.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub busy_delay: Duration,
    /// Первая пауза недоступного домена; удваивается с каждой неудачей подряд.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub backoff_min: Duration,
    /// Наибольшая пауза недоступного домена.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub backoff_max: Duration,
    /// Одновременных отправок внутренней доставки (домены кластера, `internal_domains`).
    pub internal_concurrency: usize,
    /// Одновременных отправок внешней доставки.
    pub external_concurrency: usize,
    /// Внутренние домены сверх принятых доменов кластера.
    pub internal_domains: Vec<String>,
    /// Доменов с состоянием планировщика на узле.
    pub max_domains: usize,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            domain_concurrency: 20,
            initial_concurrency: 5,
            domain_rate: 0,
            rate_window: Duration::from_secs(60),
            wait: Duration::from_secs(10),
            busy_delay: Duration::from_secs(60),
            backoff_min: Duration::from_secs(60),
            backoff_max: Duration::from_secs(3600),
            internal_concurrency: 16,
            external_concurrency: 64,
            internal_domains: Vec::new(),
            max_domains: 10_000,
        }
    }
}

/// MTA-STS (RFC 8461) и DANE (RFC 7672) при отправке. Применяется без перезапуска.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PolicyConfig {
    /// Соблюдать политики MTA-STS доменов назначения.
    pub mta_sts: bool,
    /// Проверять сертификаты MX по записям TLSA, подписанным DNSSEC.
    pub dane: bool,
    /// Ожидание ответа HTTPS при получении политики MTA-STS.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub fetch_timeout: Duration,
    /// Наибольший размер политики MTA-STS, байт.
    pub max_policy_size: usize,
    /// Доменов с политикой MTA-STS в кэше узла.
    pub cache_size: usize,
}

impl Default for PolicyConfig {
    fn default() -> Self {
        Self {
            mta_sts: true,
            dane: true,
            fetch_timeout: Duration::from_secs(10),
            max_policy_size: 64 * 1024,
            cache_size: 10_000,
        }
    }
}

impl QueueConfig {
    /// Интервал до повтора после `attempts`-й неудачи (с 1).
    pub fn retry_after(&self, attempts: u32) -> Duration {
        let i = (attempts.max(1) - 1) as usize;
        self.retry
            .get(i)
            .or(self.retry.last())
            .map_or(Duration::from_secs(3600), |d| d.0)
    }
}

/// TLS при отправке на MX.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum OutboundTls {
    /// STARTTLS, если сервер его предлагает, без проверки сертификата (RFC 7435); иначе — без TLS.
    #[default]
    Opportunistic,
    /// Только STARTTLS с проверкой сертификата по системным корням; иначе — временная неудача.
    Require,
}

/// DKIM-подпись исходящей почты (RFC 6376). Применяется без перезапуска.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DkimConfig {
    /// Домены подписи (домен адреса `From`, нижний регистр) → селекторы и ключи.
    pub domains: std::collections::BTreeMap<String, DkimDomain>,
    /// Подписываемые заголовки.
    pub headers: Vec<String>,
    /// Время жизни разобранного ключа в кэше (затем перечитывается из хранилища секретов).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub key_refresh: Duration,
}

impl Default for DkimConfig {
    fn default() -> Self {
        Self {
            domains: Default::default(),
            headers: [
                "From",
                "To",
                "Cc",
                "Subject",
                "Date",
                "Message-ID",
                "Reply-To",
                "In-Reply-To",
                "References",
                "MIME-Version",
                "Content-Type",
                "Content-Transfer-Encoding",
            ]
            .map(String::from)
            .to_vec(),
            key_refresh: Duration::from_secs(3600),
        }
    }
}

/// Ключи домена.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DkimDomain {
    /// Активный селектор: им подписываются письма (ротация — переключение на новый).
    pub selector: String,
    /// Ключи по селекторам (старые остаются для истории ротации).
    pub keys: std::collections::BTreeMap<String, DkimKeyConfig>,
}

/// Ключ селектора.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DkimKeyConfig {
    /// Алгоритм.
    pub algorithm: DkimAlgorithm,
    /// Закрытый ключ (PEM) в хранилище секретов.
    pub key: tp_contracts::secrets::SecretRef,
}

/// Алгоритм подписи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
pub enum DkimAlgorithm {
    /// RSA-SHA256 (RFC 6376).
    #[serde(rename = "rsa-sha256")]
    RsaSha256,
    /// Ed25519-SHA256 (RFC 8463).
    #[serde(rename = "ed25519-sha256")]
    Ed25519Sha256,
}

#[cfg(test)]
mod tests {
    use tp_platform::SectionRegistry;

    use super::*;

    const BASE: &str = "database: { url: postgres://db/thepost }\nblob: { url: file:///var/lib/thepost }\ndomain: example.com\n";

    fn registry() -> SectionRegistry {
        let mut r = SectionRegistry::new();
        r.add_section::<SmtpConfig>("smtp").expect("smtp");
        r
    }

    #[test]
    fn addressing_rewrite_key_is_an_error_with_hint() {
        let ok = registry()
            .parse(
                &format!("{BASE}smtp:\n  addressing:\n    subaddress_separators: \"+-\"\n"),
                Vec::new(),
            )
            .expect("parse");
        let smtp = ok.section::<SmtpConfig>("smtp").expect("smtp");
        assert_eq!(smtp.addressing.subaddress_separators, "+-");
        // Ключ таблицы переписывания — ошибка разбора (и в мягком режиме), подсказка — Admin API.
        for value in [
            "\n      - { from: \"@old.example.org\", to: \"@example.org\" }",
            " []",
        ] {
            let src = format!("{BASE}smtp:\n  addressing:\n    rewrite:{value}\n");
            for strict in [false, true] {
                let r = registry();
                let res = if strict {
                    r.parse_strict(&src, Vec::new())
                } else {
                    r.parse(&src, Vec::new())
                };
                let err = res.expect_err("rewrite key must fail").to_string();
                assert!(err.contains("Admin API"), "{err}");
                assert!(err.contains("smtp.addressing.rewrite"), "{err}");
            }
        }
    }
}
