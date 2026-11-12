//! Секция `dav` config.yaml: лимиты запросов и ответов CalDAV и CardDAV, развёртка повторов для индекса.

use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use time::{Date, Month, OffsetDateTime};
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
    /// Наименьший год дат объекта календаря; раньше — `PUT` отклоняется (`min-date-time`, RFC 4791 §5.2.6).
    pub min_date_year: i32,
    /// Наибольший год дат объекта календаря; позже — `PUT` отклоняется (`max-date-time`, §5.2.7). Повтор без
    /// конца допустим.
    pub max_date_year: i32,
    /// Ссылок `href` в одном запросе `calendar-multiget`; больше — 403 `number-of-matches-within-limits`.
    pub max_multiget_hrefs: usize,
    /// Отображаемое имя календаря по умолчанию (создаётся при первом обращении к дому календарей).
    pub default_calendar_name: String,
    /// Отображаемое имя списка задач по умолчанию (VTODO; создаётся вместе с календарём по умолчанию).
    pub default_tasks_name: String,
    /// Отображаемое имя адресной книги по умолчанию (создаётся при первом обращении к дому адресных книг).
    pub default_addressbook_name: String,
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
            min_date_year: 1900,
            max_date_year: 2100,
            max_multiget_hrefs: 1_000,
            default_calendar_name: "Календарь".to_owned(),
            default_tasks_name: "Задачи".to_owned(),
            default_addressbook_name: "Контакты".to_owned(),
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

    /// Допустимые даты объектов календаря: с начала `min_date_year` до конца `max_date_year` (граница не входит).
    /// Год вне диапазона библиотеки времени — без границы.
    pub fn date_bounds(&self) -> (Option<OffsetDateTime>, Option<OffsetDateTime>) {
        let year = |y: i32| {
            Date::from_calendar_date(y, Month::January, 1)
                .ok()
                .map(|d| d.midnight().assume_utc())
        };
        (
            year(self.min_date_year),
            self.max_date_year.checked_add(1).and_then(year),
        )
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
