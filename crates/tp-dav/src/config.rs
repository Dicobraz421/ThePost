//! Секция `dav` config.yaml: лимиты запросов и ответов CalDAV и CardDAV, развёртка повторов для индекса.

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use time::OffsetDateTime;
use tp_contracts::config::duration;

use crate::index::IndexOptions;
use crate::xml::XmlLimits;

/// Конфигурация DAV (поля необязательны, неизвестное — ошибка). Применяется при создании маршрутов
/// [`crate::router`].
///
/// ```yaml
/// max_request_size: 1048576
/// expand_horizon: 730d
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct DavConfig {
    /// Размер XML-тела запроса (`PROPFIND`, `PROPPATCH`, `REPORT`, `MKCOL`, `MKCALENDAR`), байт; больше — 413.
    pub max_request_size: usize,
    /// Глубина вложенности элементов XML-тела; глубже — 400.
    pub max_xml_depth: usize,
    /// Элементов в XML-теле; больше — 400.
    pub max_xml_elements: usize,
    /// Элементов `response` в одном ответе `multistatus` (`Depth: 1`, `sync-collection`); остальные —
    /// продолжением: `507` с `number-of-matches-within-limits`.
    pub max_response_items: usize,
    /// Размер объекта (тело `PUT`), байт; действует меньшее из этого значения и лимита хранилища
    /// (`DavLimits::max_object_size`). Публикуется как `max-resource-size`.
    pub max_object_size: usize,
    /// Горизонт развёртки повторов для индекса объекта (от момента записи): повторы дальше горизонта
    /// и правила без конца дают интервал без конца.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub expand_horizon: Duration,
    /// Наибольшее число развёрнутых вхождений одного объекта; достигнуто — интервал без конца.
    pub max_instances: usize,
}

impl Default for DavConfig {
    fn default() -> Self {
        Self {
            max_request_size: 1024 * 1024,
            max_xml_depth: 32,
            max_xml_elements: 10_000,
            max_response_items: 1_000,
            max_object_size: 1024 * 1024,
            expand_horizon: Duration::from_secs(730 * 24 * 3600),
            max_instances: 10_000,
        }
    }
}

impl DavConfig {
    /// Лимиты разбора XML-тел.
    pub fn xml_limits(&self) -> XmlLimits {
        XmlLimits {
            max_depth: self.max_xml_depth,
            max_elements: self.max_xml_elements,
        }
    }

    /// Параметры индекса для записи в момент `now`.
    pub fn index_options(&self, now: OffsetDateTime) -> IndexOptions {
        let horizon = time::Duration::try_from(self.expand_horizon).unwrap_or(time::Duration::MAX);
        IndexOptions {
            horizon: now.checked_add(horizon).unwrap_or(now),
            max_instances: self.max_instances,
        }
    }
}
