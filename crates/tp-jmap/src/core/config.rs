//! Секция `jmap` config.yaml: адрес сервера и лимиты RFC 8620 (`urn:ietf:params:jmap:core`).

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use tp_contracts::config::duration;

/// Конфигурация JMAP (поля необязательны, неизвестное — ошибка). Применяется при создании [`crate::Jmap`].
///
/// ```yaml
/// public_url: https://mail.example.org
/// max_calls_in_request: 16
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct JmapConfig {
    /// Внешний адрес для URL в объекте сессии (`https://mail.example.org`, без `/` в конце);
    /// не задан — `https://` + заголовок `Host` запроса.
    pub public_url: Option<String>,
    /// `maxSizeRequest`: размер тела запроса API, байт; столько же байт за запрос — значений, подставленных
    /// по ссылкам на результаты.
    pub max_size_request: u64,
    /// `maxConcurrentRequests`: одновременных запросов API на учётную запись.
    pub max_concurrent_requests: u32,
    /// `maxCallsInRequest`: вызовов методов в одном запросе.
    pub max_calls_in_request: u32,
    /// `maxObjectsInGet`: идентификаторов в одном `/get`.
    pub max_objects_in_get: u32,
    /// `maxObjectsInSet`: создаваемых, изменяемых и удаляемых объектов в одном `/set`.
    pub max_objects_in_set: u32,
    /// `maxSizeUpload`: размер загружаемого файла, байт (сессия; загрузку выполняет JMAP-06).
    pub max_size_upload: u64,
    /// `maxConcurrentUpload`: одновременных загрузок (сессия).
    pub max_concurrent_upload: u32,
    /// Изменений в одном ответе `/changes` и `/queryChanges`, если клиент не задал `maxChanges` меньше.
    pub max_changes: u32,
    /// Наибольший `limit` в `/query`; больший урезается (ответ содержит `limit`).
    pub max_query_limit: u32,
    /// Наибольшее число результатов одного текстового условия в `Email/query` (запрос к `SearchIndex`).
    pub max_search_results: u32,
    /// Сколько первых байтов письма читать для заголовков (`references`, `headers`, `header:*`).
    pub header_window: u64,
    /// Получателей конверта в одной отправке `EmailSubmission/set`; больше — `tooManyRecipients`.
    pub max_submission_recipients: u32,
    /// Наименьший период `ping` EventSource (RFC 8620 §7.3): меньший период клиента увеличивается до него.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub push_min_ping: Duration,
    /// Сколько загруженный файл (`U…`) доступен загрузившей учётной записи; по умолчанию — как окно
    /// сборки мусора BLOB (`blob.gc.window`), после которого объект без ссылок может быть удалён.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub upload_ttl: Duration,
    /// Автонастройка клиентов: серверы IMAP, SMTP, CalDAV и CardDAV в ответах (`crate::autoconfig`).
    pub autoconfig: AutoconfigConfig,
    /// Календари JMAP: развёртка повторов событий.
    pub calendars: CalendarsConfig,
}

/// Секция `jmap.calendars`: лимиты развёртки повторов событий (JMAP Calendars).
///
/// ```yaml
/// calendars:
///   expand_horizon: 730d
///   max_expanded_query_duration: 400d
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CalendarsConfig {
    /// Горизонт развёртки повторов для индекса события (от момента записи), как `dav.expand_horizon`:
    /// повторы дальше горизонта и правила без конца дают интервал без конца.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub expand_horizon: Duration,
    /// Наибольшее число развёрнутых вхождений одного события (индекс, `CalendarEvent/query`).
    pub max_instances: usize,
    /// `maxExpandedQueryDuration`: наибольший интервал `after`…`before` запроса с `expandRecurrences`.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub max_expanded_query_duration: Duration,
    /// `maxAvailabilityDuration`: наибольший интервал `Principal/getAvailability`.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub max_availability_duration: Duration,
}

impl CalendarsConfig {
    /// Параметры индекса события для записи в момент `now`.
    pub(crate) fn index_options(&self, now: time::OffsetDateTime) -> tp_dav::index::IndexOptions {
        let horizon = time::Duration::try_from(self.expand_horizon).unwrap_or(time::Duration::MAX);
        tp_dav::index::IndexOptions {
            horizon: now.checked_add(horizon).unwrap_or(now),
            max_instances: self.max_instances,
        }
    }
}

impl Default for CalendarsConfig {
    fn default() -> Self {
        Self {
            expand_horizon: Duration::from_secs(730 * 24 * 3600),
            max_instances: 10_000,
            max_expanded_query_duration: Duration::from_secs(400 * 24 * 3600),
            max_availability_duration: Duration::from_secs(400 * 24 * 3600),
        }
    }
}

impl Default for JmapConfig {
    fn default() -> Self {
        Self {
            public_url: None,
            max_size_request: 10_000_000,
            max_concurrent_requests: 4,
            max_calls_in_request: 16,
            max_objects_in_get: 500,
            max_objects_in_set: 500,
            max_size_upload: 50_000_000,
            max_concurrent_upload: 4,
            max_changes: 5_000,
            max_query_limit: 1_000,
            max_search_results: 10_000,
            header_window: 262_144,
            max_submission_recipients: 100,
            push_min_ping: Duration::from_secs(10),
            upload_ttl: Duration::from_secs(7 * 24 * 3600),
            autoconfig: AutoconfigConfig::default(),
            calendars: CalendarsConfig::default(),
        }
    }
}

/// Секция `jmap.autoconfig`: что сообщать клиентам при автонастройке (Thunderbird, `.mobileconfig`,
/// `/.well-known/caldav`, `/.well-known/carddav`).
///
/// ```yaml
/// autoconfig:
///   host: mail.example.org
///   organization: ООО «Пример»
///   imap: { port: 993, security: tls }
///   submission: { port: 587, security: starttls }
///   caldav: /dav/
///   carddav: /dav/
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct AutoconfigConfig {
    /// Имя сервера IMAP и SMTP; не задано — узел из `public_url`, иначе заголовок `Host` без префикса `autoconfig.`.
    pub host: Option<String>,
    /// Название организации (имя поставщика у Thunderbird, `PayloadOrganization` профиля); не задано — домен.
    pub organization: Option<String>,
    /// IMAP (по умолчанию 993, TLS).
    pub imap: MailEndpoint,
    /// SMTP submission (по умолчанию 465, TLS; RFC 8314).
    pub submission: MailEndpoint,
    /// Контекст CalDAV (RFC 6764 §5): путь на этом сервере (от `public_url`) или полный URL.
    pub caldav: String,
    /// Контекст CardDAV (RFC 6764 §5): путь на этом сервере (от `public_url`) или полный URL.
    pub carddav: String,
}

impl Default for AutoconfigConfig {
    fn default() -> Self {
        Self {
            host: None,
            organization: None,
            imap: MailEndpoint {
                port: 993,
                security: Security::Tls,
            },
            submission: MailEndpoint {
                port: 465,
                security: Security::Tls,
            },
            caldav: "/dav/".into(),
            carddav: "/dav/".into(),
        }
    }
}

/// Порт и защита почтового сервера (поля обязательны).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MailEndpoint {
    /// Порт.
    pub port: u16,
    /// Защита соединения.
    pub security: Security,
}

/// Защита соединения почтового сервера; открытый текст не предлагается.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// TLS с начала соединения (IMAPS 993, submissions 465).
    Tls,
    /// STARTTLS (IMAP 143, submission 587).
    Starttls,
}
