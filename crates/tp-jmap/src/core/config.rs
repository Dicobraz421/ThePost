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
    /// `maxSizeRequest`: размер тела запроса API, байт.
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
        }
    }
}
