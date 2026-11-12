//! Календари и контакты: коллекции, объекты, журнал изменений (CTR-45; CONTRACTS.md#Dav; SRS 4.4, 3.1).
//!
//! Одно хранилище для CalDAV (RFC 4791), CardDAV (RFC 6352) и JMAP Calendars/Contacts. Тело объекта (iCalendar, vCard)
//! хранилище не разбирает: индекс [`ObjectIndex`] вычисляет вызывающий (`tp-dav`, `tp-jmap`) библиотекой calcard.
//!
//! # Номера изменений
//!
//! Каждое изменение (коллекция создана, изменена, удалена; объект записан, удалён) получает новый [`ModSeq`] из счётчика
//! учётной записи — того же, что у писем (CONTRACTS.md#Metadata). Состояние коллекции [`Collection::state`] — наибольший
//! `ModSeq` её изменений (CTag, токен синхронизации RFC 6578). Удалённые объекты и коллекции остаются надгробиями
//! (имя, идентификатор, `ModSeq`) до очистки реализацией; `since` старше очищенных надгробий — `Error::Conflict`
//! (полная синхронизация: RFC 6578 `valid-sync-token`, JMAP `cannotCalculateChanges`).
//!
//! После фиксации каждого изменения реализация публикует сигнал [`dav_changed`] ([`EventKind::DavChanged`]).
//!
//! # Планирование (CTR-47)
//!
//! Входящие планирования (`schedule-inbox`, RFC 6638 §2.2) — отдельное хранилище учётной записи ([`DavStore::inbox_put`],
//! [`DavStore::inbox_list`], [`DavStore::inbox_delete`]): UID в нём не уникален, в коллекции и их изменения не входит.
//! `Schedule-Tag` объекта (RFC 6638 §3.2) и индекс занятости — поля [`ObjectIndex`], их задаёт вызывающий; занятость
//! читает [`crate::schedule::FreeBusy`].

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::bus::{Event, EventKind};
use crate::error::{Error, Result};
use crate::ids::ModSeq;
pub use crate::ids::{CollectionId, DavObjectId};
use crate::meta::{AccountScope, Changes};
use crate::schedule::{BusyPeriod, Inbox, InboxMessage, validate_busy};

/// Наибольшая длина имени коллекции и имени ресурса (сегмент URL), байт.
pub const MAX_NAME_LEN: usize = 255;

/// Наибольшая длина отображаемого имени коллекции, символов.
pub const MAX_DISPLAY_NAME_LEN: usize = 255;

/// Наибольшая длина описания коллекции, байт.
pub const MAX_DESCRIPTION_LEN: usize = 4096;

/// Наибольшая длина цвета и часового пояса коллекции, байт.
pub const MAX_PROPERTY_LEN: usize = 255;

/// Наибольшая длина UID объекта, байт.
pub const MAX_UID_LEN: usize = 1024;

/// Вид коллекции.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CollectionKind {
    /// Календарь (CalDAV, JMAP Calendars): события и задачи.
    Calendar,
    /// Адресная книга (CardDAV, JMAP Contacts).
    AddressBook,
}

/// Компонент объекта: верхний компонент iCalendar или vCard.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Component {
    /// `VEVENT`.
    Event,
    /// `VTODO`.
    Todo,
    /// `VJOURNAL`.
    Journal,
    /// `VCARD`.
    Card,
}

impl Component {
    /// Имя компонента: `VEVENT`, `VTODO`, `VJOURNAL`, `VCARD`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Component::Event => "VEVENT",
            Component::Todo => "VTODO",
            Component::Journal => "VJOURNAL",
            Component::Card => "VCARD",
        }
    }

    /// Компонент допустим в коллекции вида `kind`: `VCARD` — в адресной книге, остальные — в календаре.
    pub const fn fits(self, kind: CollectionKind) -> bool {
        matches!(
            (self, kind),
            (Component::Card, CollectionKind::AddressBook)
                | (
                    Component::Event | Component::Todo | Component::Journal,
                    CollectionKind::Calendar
                )
        )
    }
}

impl fmt::Display for Component {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Component {
    type Err = Error;

    /// Имя без учёта регистра (`VEVENT`, `vtodo`); иное — `Error::Invalid`.
    fn from_str(s: &str) -> Result<Self> {
        [
            Component::Event,
            Component::Todo,
            Component::Journal,
            Component::Card,
        ]
        .into_iter()
        .find(|c| c.as_str().eq_ignore_ascii_case(s))
        .ok_or_else(|| Error::invalid(format!("неизвестный компонент: {s:?}")))
    }
}

/// Имя коллекции или ресурса — сегмент URL после декодирования: 1–[`MAX_NAME_LEN`] байт, без `/` и управляющих
/// символов, не `.` и не `..`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name != "."
        && name != ".."
        && !name.chars().any(|c| c == '/' || c.is_control())
}

/// Строка без управляющих символов и не длиннее `max` байт.
fn plain(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}

/// Коллекция учётной записи: календарь или адресная книга.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collection {
    /// Идентификатор (JMAP id `Calendar`, `AddressBook`); при создании задаёт вызывающий ([`CollectionId::new`]).
    pub id: CollectionId,
    /// Вид; после создания не меняется.
    pub kind: CollectionKind,
    /// Имя — сегмент URL ([`valid_name`]), уникально среди коллекций вида в учётной записи; после создания не меняется.
    pub name: String,
    /// Отображаемое имя (WebDAV `displayname`, JMAP `name`); до [`MAX_DISPLAY_NAME_LEN`] символов.
    pub display_name: String,
    /// Описание (`calendar-description`, `addressbook-description`); до [`MAX_DESCRIPTION_LEN`] байт.
    pub description: Option<String>,
    /// Цвет CSS (`calendar-color`, JMAP `color`).
    pub color: Option<String>,
    /// Порядок в списке (`calendar-order`, JMAP `sortOrder`).
    pub order: u32,
    /// Часовой пояс IANA по умолчанию (`calendar-timezone-id`, RFC 7809; JMAP `timeZone`).
    pub timezone: Option<String>,
    /// Допустимые компоненты объектов (`supported-calendar-component-set`): у календаря — непустое подмножество
    /// `VEVENT`, `VTODO`, `VJOURNAL`, у адресной книги — `{VCARD}`.
    pub components: BTreeSet<Component>,
    /// Календарь не влияет на занятость (RFC 6638 `schedule-calendar-transp: transparent`): его объекты не входят
    /// в [`crate::schedule::FreeBusy::query`]. У адресной книги не используется.
    #[serde(default)]
    pub transparent: bool,
    /// Наибольший `ModSeq` изменений коллекции: свойств и объектов (CTag). Задаёт хранилище; при записи не учитывается.
    pub state: ModSeq,
}

impl Collection {
    /// Новая коллекция вида `kind` с именем `name`: отображаемое имя — `name`, компоненты по умолчанию —
    /// `VEVENT`, `VTODO` (календарь) или `VCARD` (адресная книга), остальные свойства пусты.
    pub fn new(kind: CollectionKind, name: impl Into<String>) -> Self {
        let name = name.into();
        let components = match kind {
            CollectionKind::Calendar => BTreeSet::from([Component::Event, Component::Todo]),
            CollectionKind::AddressBook => BTreeSet::from([Component::Card]),
        };
        Self {
            id: CollectionId::new(),
            kind,
            display_name: name.clone(),
            name,
            description: None,
            color: None,
            order: 0,
            timezone: None,
            components,
            transparent: false,
            state: ModSeq::ZERO,
        }
    }

    /// Проверяет поля: имя — [`valid_name`]; отображаемое имя — до [`MAX_DISPLAY_NAME_LEN`] символов, описание — до
    /// [`MAX_DESCRIPTION_LEN`] байт, цвет и часовой пояс — до [`MAX_PROPERTY_LEN`] байт, все без управляющих символов
    /// (в описании допустимы перевод строки и табуляция); компоненты — непустые и подходят виду ([`Component::fits`]).
    /// Иначе — `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if !valid_name(&self.name) {
            return Err(Error::invalid(format!(
                "недопустимое имя коллекции: {:?}",
                self.name
            )));
        }
        if self.display_name.chars().count() > MAX_DISPLAY_NAME_LEN
            || self.display_name.chars().any(char::is_control)
        {
            return Err(Error::invalid("недопустимое отображаемое имя коллекции"));
        }
        if self.description.as_deref().is_some_and(|d| {
            d.len() > MAX_DESCRIPTION_LEN
                || d.chars()
                    .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
        }) {
            return Err(Error::invalid("недопустимое описание коллекции"));
        }
        for p in [&self.color, &self.timezone].into_iter().flatten() {
            if p.is_empty() || !plain(p, MAX_PROPERTY_LEN) {
                return Err(Error::invalid(
                    "недопустимый цвет или часовой пояс коллекции",
                ));
            }
        }
        if self.components.is_empty() || !self.components.iter().all(|c| c.fits(self.kind)) {
            return Err(Error::invalid("недопустимые компоненты коллекции"));
        }
        Ok(())
    }
}

/// Интервал объекта (UTC) с развёрнутыми повторами: от начала первого вхождения до конца последнего.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    /// Начало.
    pub start: OffsetDateTime,
    /// Конец (не раньше начала); `None` — бесконечный (повторы без конца).
    pub end: Option<OffsetDateTime>,
}

/// Интервал запроса time-range (RFC 4791 §9.9, UTC); `None` — без границы с этой стороны.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TimeRange {
    /// Начало.
    pub start: Option<OffsetDateTime>,
    /// Конец.
    pub end: Option<OffsetDateTime>,
}

/// Индекс объекта: вычисляет вызывающий из тела (calcard), хранилище хранит и ищет по нему.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectIndex {
    /// Компонент объекта.
    pub component: Component,
    /// UID (iCalendar `UID`, vCard `UID`): 1–[`MAX_UID_LEN`] байт без управляющих символов; уникален в коллекции.
    pub uid: String,
    /// Интервал; `None` — без дат (контакт, задача без `DTSTART`/`DUE`): кандидат любого запроса time-range.
    pub span: Option<Span>,
    /// Индекс занятости (CTR-47) неповторяющегося объекта: его периоды, не больше
    /// [`crate::schedule::MAX_BUSY_PERIODS`]. Вычисляет `tp-dav` (DAV-16): `TRANSP:TRANSPARENT`, `STATUS:CANCELLED`,
    /// задачи и контакты — пусто, `STATUS:TENTATIVE` — [`crate::schedule::BusyKind::Tentative`]. У повторяющегося
    /// объекта ([`ObjectIndex::recurring`]) — пусто.
    #[serde(default)]
    pub busy: Vec<BusyPeriod>,
    /// Повторяющееся событие (META-54, решение 59): в теле есть `RRULE` или `RDATE`. Его занятость в индекс не
    /// пишется: хранилище разворачивает повторы и исключения из тела при запросе
    /// ([`crate::schedule::FreeBusy::query`]). Задаёт вызывающий; только у [`Component::Event`].
    #[serde(default)]
    pub recurring: bool,
    /// `Schedule-Tag` объекта планирования (RFC 6638 §3.2) без кавычек ([`valid_etag`]); `None` — объект без
    /// планирования. Задаёт вызывающий: новый при изменении организатором, прежний — при изменении участником только
    /// своего `PARTSTAT`. `If-Schedule-Tag-Match` проверяет вызывающий по прочитанному объекту и пишет с
    /// [`Precondition::IfMatch`] его ETag.
    #[serde(default)]
    pub schedule_tag: Option<String>,
}

impl ObjectIndex {
    /// Индекс без занятости и `Schedule-Tag`.
    pub fn new(component: Component, uid: impl Into<String>, span: Option<Span>) -> Self {
        Self {
            component,
            uid: uid.into(),
            span,
            busy: Vec::new(),
            recurring: false,
            schedule_tag: None,
        }
    }

    /// Проверяет UID, интервал (конец не раньше начала), занятость ([`validate_busy`]; у повторяющегося объекта —
    /// пусто, повторяющийся — только событие) и `Schedule-Tag` ([`valid_etag`]); иначе — `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if self.uid.is_empty() || !plain(&self.uid, MAX_UID_LEN) {
            return Err(Error::invalid("недопустимый UID объекта"));
        }
        validate_busy(&self.busy)?;
        if self.recurring && (self.component != Component::Event || !self.busy.is_empty()) {
            return Err(Error::invalid(
                "повторяющийся объект — событие без индекса занятости",
            ));
        }
        if self.schedule_tag.as_deref().is_some_and(|t| !valid_etag(t)) {
            return Err(Error::invalid("недопустимый Schedule-Tag объекта"));
        }
        if let Some(Span {
            start,
            end: Some(end),
        }) = self.span
            && end < start
        {
            return Err(Error::invalid("конец интервала объекта раньше начала"));
        }
        Ok(())
    }

    /// Объект — кандидат запроса `range` ([`DavStore::in_range`]): без интервала — всегда; иначе
    /// `span.start < range.end` и `span.end ≥ range.start` (нет границы — бесконечность). Условие шире точного
    /// RFC 4791 §9.9 (нулевая длительность, `DUE` без `DTSTART`): точную проверку делает вызывающий по телу.
    pub fn may_overlap(&self, range: &TimeRange) -> bool {
        let Some(span) = self.span else {
            return true;
        };
        range.end.is_none_or(|end| span.start < end)
            && match (span.end, range.start) {
                (Some(end), Some(start)) => end >= start,
                _ => true,
            }
    }
}

/// Объект коллекции: событие, задача, контакт.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DavObject {
    /// Идентификатор (JMAP id); новый при каждом создании ресурса, в том числе с прежним именем.
    pub id: DavObjectId,
    /// Коллекция.
    pub collection: CollectionId,
    /// Имя ресурса — сегмент URL ([`valid_name`]), уникально в коллекции.
    pub name: String,
    /// ETag без кавычек ([`valid_etag`]); новый при каждой записи. Кавычки HTTP добавляет `tp-dav`.
    pub etag: String,
    /// Тело: iCalendar или vCard как записано.
    pub body: String,
    /// Индекс.
    pub index: ObjectIndex,
}

/// ETag хранилища: 1–64 видимых символа ASCII без `"` и `\`.
pub fn valid_etag(etag: &str) -> bool {
    !etag.is_empty()
        && etag.len() <= 64
        && etag
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b'"' && b != b'\\')
}

/// Ресурс в списке коллекции (RFC 6578, `PROPFIND` `getetag`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRef {
    /// Идентификатор.
    pub id: DavObjectId,
    /// Имя ресурса.
    pub name: String,
    /// ETag.
    pub etag: String,
}

/// Условие записи и удаления объекта (HTTP `If-Match`, `If-None-Match: *`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Precondition {
    /// Без условия.
    Any,
    /// Ресурс существует с этим ETag (без кавычек).
    IfMatch(String),
    /// Ресурса нет (`If-None-Match: *`).
    IfNoneMatch,
}

/// Объект записан.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Put {
    /// Идентификатор объекта: прежний при перезаписи, новый при создании.
    pub id: DavObjectId,
    /// Новый ETag.
    pub etag: String,
    /// `ModSeq` изменения.
    pub modseq: ModSeq,
    /// Ресурс создан (HTTP 201), а не перезаписан (204).
    pub created: bool,
}

/// Результат [`DavStore::put_object`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PutOutcome {
    /// Записан.
    Stored(Put),
    /// Не выполнено условие (HTTP 412); ничего не изменено.
    PreconditionFailed,
    /// В коллекции уже есть объект с этим UID под другим именем `name` (RFC 4791 `no-uid-conflict`,
    /// RFC 6352 `no-uid-conflict`); ничего не изменено.
    UidConflict {
        /// Имя ресурса с этим UID.
        name: String,
    },
}

/// Результат [`DavStore::delete_object`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeleteOutcome {
    /// Удалён; `ModSeq` изменения.
    Deleted(ModSeq),
    /// Не выполнено условие (HTTP 412); ничего не изменено.
    PreconditionFailed,
}

/// Лимиты хранилища — из конфигурации реализации (`meta`); `tp-dav` публикует их (`max-resource-size`), JMAP —
/// в capabilities. Превышение — `Error::QuotaExceeded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DavLimits {
    /// Размер тела объекта (и сообщения входящих планирования), байт.
    pub max_object_size: usize,
    /// Объектов в коллекции (и сообщений во входящих планирования).
    pub max_objects: usize,
    /// Коллекций учётной записи (всех видов).
    pub max_collections: usize,
    /// Наибольшее число вхождений одного повторяющегося объекта, разворачиваемых при запросе занятости
    /// ([`crate::schedule::FreeBusy::query`], META-54), считая от начала повторов; вхождения сверх него в занятость
    /// не попадают.
    pub max_busy_instances: usize,
}

impl Default for DavLimits {
    /// 1 МиБ на объект, 20 000 объектов в коллекции, 100 коллекций, 20 000 вхождений повторяющегося объекта.
    fn default() -> Self {
        Self {
            max_object_size: 1024 * 1024,
            max_objects: 20_000,
            max_collections: 100,
            max_busy_instances: 20_000,
        }
    }
}

/// Сигнал об изменении коллекции `collection` вида `kind` учётной записи `scope` ([`EventKind::DavChanged`]):
/// публикуется после фиксации каждого изменения [`DavStore`] (JMAP push, кэши `tp-dav`).
pub fn dav_changed(
    scope: AccountScope,
    kind: CollectionKind,
    collection: CollectionId,
    modseq: ModSeq,
) -> Event {
    Event {
        tenant: scope.tenant,
        account: Some(scope.account),
        kind: EventKind::DavChanged {
            kind,
            collection,
            modseq,
        },
    }
}

/// Хранилище календарей и контактов учётной записи. Реализация — `tp-meta` (META-47; тела — в PostgreSQL, SRS 3.1);
/// in-memory — `tp-testkit`. Коллекция или объект чужой учётной записи (другой `scope`) — как отсутствующие.
/// Удаление учётной записи удаляет её коллекции.
#[async_trait]
pub trait DavStore: Send + Sync + 'static {
    /// Лимиты хранилища.
    fn limits(&self) -> DavLimits;

    // ---------- Коллекции ----------

    /// Коллекции вида `kind` по `order`, затем по имени (побайтно).
    async fn list_collections(
        &self,
        scope: AccountScope,
        kind: CollectionKind,
    ) -> Result<Vec<Collection>>;

    /// Коллекция по идентификатору.
    async fn get_collection(
        &self,
        scope: AccountScope,
        id: CollectionId,
    ) -> Result<Option<Collection>>;

    /// Коллекция вида `kind` по имени (сегменту URL).
    async fn collection_by_name(
        &self,
        scope: AccountScope,
        kind: CollectionKind,
        name: &str,
    ) -> Result<Option<Collection>>;

    /// Создаёт коллекцию; недопустимые поля ([`Collection::validate`]) — `Invalid`, занятый `id` или имя вида —
    /// `Conflict`, коллекций уже [`DavLimits::max_collections`] — `QuotaExceeded`. Возвращает `ModSeq` изменения.
    async fn create_collection(
        &self,
        scope: AccountScope,
        collection: Collection,
    ) -> Result<ModSeq>;

    /// Заменяет свойства коллекции `collection.id` (отображаемое имя, описание, цвет, порядок, часовой пояс,
    /// компоненты); нет — `NotFound`, другие `kind` или `name` — `Invalid`. Объекты не проверяются заново.
    /// Возвращает `ModSeq` изменения.
    async fn update_collection(
        &self,
        scope: AccountScope,
        collection: Collection,
    ) -> Result<ModSeq>;

    /// Удаляет коллекцию сразу, без корзины: её объекты и она сама — в надгробия, каждый со своим `ModSeq` (коллекция —
    /// последней; страницы `/changes` не делят одно изменение). Нет — `NotFound`. Возвращает `ModSeq` коллекции.
    async fn delete_collection(&self, scope: AccountScope, id: CollectionId) -> Result<ModSeq>;

    // ---------- Объекты ----------

    /// Объект коллекции по имени ресурса. Нет коллекции — `NotFound`.
    async fn get_object(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        name: &str,
    ) -> Result<Option<DavObject>>;

    /// Объект коллекции по UID. Нет коллекции — `NotFound`.
    async fn object_by_uid(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        uid: &str,
    ) -> Result<Option<DavObject>>;

    /// Ресурсы коллекции по имени (побайтно). Нет коллекции — `NotFound`.
    async fn list_objects(
        &self,
        scope: AccountScope,
        collection: CollectionId,
    ) -> Result<Vec<ObjectRef>>;

    /// Объекты коллекции по именам (`calendar-multiget`, `addressbook-multiget`) в порядке `names`; отсутствующие
    /// пропускаются. Нет коллекции — `NotFound`.
    async fn multiget(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        names: &[String],
    ) -> Result<Vec<DavObject>>;

    /// Объекты учётной записи по идентификаторам (JMAP `/get`) в порядке `ids`; отсутствующие пропускаются.
    async fn objects_by_id(
        &self,
        scope: AccountScope,
        ids: &[DavObjectId],
    ) -> Result<Vec<DavObject>>;

    /// Кандидаты запроса time-range ([`ObjectIndex::may_overlap`]) по имени. Нет коллекции — `NotFound`.
    async fn in_range(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        range: TimeRange,
    ) -> Result<Vec<DavObject>>;

    /// Создаёт или перезаписывает ресурс `name` коллекции при условии `pre`. Проверки по порядку: нет коллекции —
    /// `NotFound`; недопустимое имя или индекс ([`ObjectIndex::validate`]), компонент не из
    /// [`Collection::components`] — `Invalid`; тело больше [`DavLimits::max_object_size`] — `QuotaExceeded`; условие —
    /// [`PutOutcome::PreconditionFailed`]; UID у другого ресурса — [`PutOutcome::UidConflict`]; новый ресурс при
    /// [`DavLimits::max_objects`] — `QuotaExceeded`. Перезапись сохраняет `id`, ETag и `ModSeq` — новые.
    async fn put_object(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        name: &str,
        body: String,
        index: ObjectIndex,
        pre: Precondition,
    ) -> Result<PutOutcome>;

    /// Удаляет ресурс при условии `pre`, оставляя надгробие. Нет коллекции или ресурса при `Any` — `NotFound`;
    /// `IfMatch` с другим ETag или без ресурса, `IfNoneMatch` при существующем — [`DeleteOutcome::PreconditionFailed`].
    async fn delete_object(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        name: &str,
        pre: Precondition,
    ) -> Result<DeleteOutcome>;

    // ---------- Изменения ----------

    /// Изменения ресурсов коллекции после `since` — имена (RFC 6578 `sync-collection`), не более `limit`, по `ModSeq`.
    /// Существующий ресурс — в `created` (создан после `since`) или `updated`; удалённый — в `destroyed`, только если
    /// имени сейчас нет. `since = ModSeq::ZERO` — все ресурсы в `created`, `destroyed` пуст (так у всех методов
    /// изменений). Каждое изменение — свой `ModSeq`; неполный ответ (`has_more`): `new_state` — `ModSeq` последнего
    /// возвращённого. Полный ответ: `new_state` —
    /// наибольшее из `since` и [`Collection::state`]. `since` старше очищенных надгробий или больше счётчика
    /// учётной записи — `Conflict`. Нет коллекции — `NotFound`.
    async fn changes(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<String>>;

    /// Изменения коллекций вида `kind` после `since` (JMAP `Calendar/changes`, `AddressBook/changes`): создание,
    /// свойства, удаление; изменения объектов сюда не входят. Полный ответ: `new_state` — [`DavStore::state`] вида
    /// (не меньше `since`). Устаревший `since` — `Conflict`.
    async fn collection_changes(
        &self,
        scope: AccountScope,
        kind: CollectionKind,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<CollectionId>>;

    /// Изменения объектов всех коллекций вида `kind` после `since` (JMAP `CalendarEvent/changes`,
    /// `ContactCard/changes`), включая объекты удалённых коллекций. Полный ответ: `new_state` — [`DavStore::state`]
    /// вида (не меньше `since`). Устаревший `since` — `Conflict`.
    async fn account_changes(
        &self,
        scope: AccountScope,
        kind: CollectionKind,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<DavObjectId>>;

    /// Состояние вида `kind` (JMAP `state`): наибольший `ModSeq` изменений коллекций и объектов этого вида,
    /// у новой учётной записи — `ModSeq::ZERO`. Входящие планирования не входят.
    async fn state(&self, scope: AccountScope, kind: CollectionKind) -> Result<ModSeq>;

    // ---------- Входящие планирования (CTR-47) ----------

    /// Создаёт или перезаписывает ресурс `name` входящих планирования (RFC 6638 §2.2; кладёт `ItipDelivery`). Проверки
    /// по порядку: недопустимое имя ([`valid_name`]) или сообщение ([`InboxMessage::validate`]) — `Invalid`; тело больше
    /// [`DavLimits::max_object_size`] или новый ресурс при [`DavLimits::max_objects`] сообщений — `QuotaExceeded`.
    /// `ModSeq` — из счётчика учётной записи; перезапись сохраняет `id`. После фиксации — сигнал
    /// [`crate::schedule::schedule_inbox_changed`]. По умолчанию — `Error::Internal`.
    async fn inbox_put(
        &self,
        scope: AccountScope,
        name: &str,
        message: InboxMessage,
    ) -> Result<Put> {
        let _ = (scope, name, message);
        Err(Error::internal("dav store does not support schedule inbox"))
    }

    /// Входящие планирования учётной записи: ресурсы по `ModSeq` и состояние ([`Inbox::state`]). По умолчанию —
    /// `Error::Internal`.
    async fn inbox_list(&self, scope: AccountScope) -> Result<Inbox> {
        let _ = scope;
        Err(Error::internal("dav store does not support schedule inbox"))
    }

    /// Удаляет ресурс входящих планирования (без надгробия: `CalendarEventNotification/changes` после удаления —
    /// полная сверка по [`Inbox::state`]). Нет — `NotFound`. Возвращает `ModSeq` изменения; после фиксации — сигнал
    /// [`crate::schedule::schedule_inbox_changed`]. По умолчанию — `Error::Internal`.
    async fn inbox_delete(&self, scope: AccountScope, name: &str) -> Result<ModSeq> {
        let _ = (scope, name);
        Err(Error::internal("dav store does not support schedule inbox"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn object_safe() {
        fn _store(_: Arc<dyn DavStore>) {}
    }

    #[test]
    fn names() {
        for ok in ["work", "a.ics", "Календарь", "x y"] {
            assert!(valid_name(ok), "{ok:?}");
        }
        for bad in ["", ".", "..", "a/b", "a\nb", &"x".repeat(MAX_NAME_LEN + 1)] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        assert!(valid_etag("17-abc"));
        for bad in ["", "a\"b", "a b", "a\\b", &"e".repeat(65)] {
            assert!(!valid_etag(bad), "{bad:?}");
        }
    }

    #[test]
    fn components() {
        assert_eq!("vevent".parse::<Component>(), Ok(Component::Event));
        assert_eq!(Component::Card.to_string(), "VCARD");
        assert!("VALARM".parse::<Component>().is_err());
        assert!(Component::Todo.fits(CollectionKind::Calendar));
        assert!(!Component::Card.fits(CollectionKind::Calendar));
        assert!(!Component::Event.fits(CollectionKind::AddressBook));
    }

    #[test]
    fn collection_validate() {
        let cal = Collection::new(CollectionKind::Calendar, "work");
        assert_eq!(cal.validate(), Ok(()));
        assert_eq!(
            Collection::new(CollectionKind::AddressBook, "contacts").validate(),
            Ok(())
        );
        let bad = [
            Collection::new(CollectionKind::Calendar, "a/b"),
            Collection {
                components: BTreeSet::new(),
                ..cal.clone()
            },
            Collection {
                components: BTreeSet::from([Component::Card]),
                ..cal.clone()
            },
            Collection {
                display_name: "a\u{7}".into(),
                ..cal.clone()
            },
            Collection {
                color: Some(String::new()),
                ..cal.clone()
            },
            Collection {
                description: Some("x".repeat(MAX_DESCRIPTION_LEN + 1)),
                ..cal.clone()
            },
        ];
        for c in bad {
            assert!(c.validate().is_err(), "{c:?}");
        }
        let multiline = Collection {
            description: Some("строка 1\nстрока 2".into()),
            ..cal
        };
        assert_eq!(multiline.validate(), Ok(()));
    }

    #[test]
    fn index_overlap() {
        let t = |h: u8| OffsetDateTime::UNIX_EPOCH + time::Duration::hours(h.into());
        let idx = |start, end| ObjectIndex::new(Component::Event, "u", Some(Span { start, end }));
        let range = TimeRange {
            start: Some(t(10)),
            end: Some(t(12)),
        };
        assert!(idx(t(9), Some(t(11))).may_overlap(&range));
        assert!(idx(t(11), Some(t(13))).may_overlap(&range));
        assert!(
            idx(t(10), Some(t(10))).may_overlap(&range),
            "нулевая длительность в начале"
        );
        assert!(idx(t(1), None).may_overlap(&range), "бесконечные повторы");
        assert!(
            !idx(t(12), Some(t(13))).may_overlap(&range),
            "начало в конце интервала"
        );
        assert!(!idx(t(1), Some(t(9))).may_overlap(&range));
        assert!(!idx(t(13), None).may_overlap(&range));
        assert!(idx(t(1), Some(t(2))).may_overlap(&TimeRange::default()));
        let no_span = ObjectIndex {
            span: None,
            ..idx(t(0), None)
        };
        assert!(no_span.may_overlap(&range));
        assert_eq!(no_span.validate(), Ok(()));
        assert!(idx(t(2), Some(t(1))).validate().is_err());
        assert!(
            ObjectIndex {
                uid: String::new(),
                ..no_span.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            ObjectIndex {
                schedule_tag: Some("a b".into()),
                ..no_span.clone()
            }
            .validate()
            .is_err()
        );
        let busy = crate::schedule::BusyPeriod {
            start: t(1),
            end: t(2),
            kind: crate::schedule::BusyKind::Busy,
        };
        let scheduled = ObjectIndex {
            busy: vec![busy],
            schedule_tag: Some("17".into()),
            ..no_span.clone()
        };
        assert_eq!(scheduled.validate(), Ok(()));
        assert!(
            ObjectIndex {
                busy: vec![crate::schedule::BusyPeriod { end: t(1), ..busy }],
                ..no_span
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn serde_roundtrip() {
        let c = Collection::new(CollectionKind::Calendar, "work");
        let s = serde_json::to_string(&c).expect("json");
        assert_eq!(serde_json::from_str::<Collection>(&s).expect("parse"), c);
        // Записи CTR-45 без полей CTR-47 читаются со значениями по умолчанию.
        let mut v = serde_json::to_value(&c).expect("json");
        v.as_object_mut().expect("object").remove("transparent");
        assert_eq!(serde_json::from_value::<Collection>(v).expect("old"), c);
        let old: ObjectIndex =
            serde_json::from_str(r#"{"component":"Event","uid":"u","span":null}"#)
                .expect("old index");
        assert_eq!(old, ObjectIndex::new(Component::Event, "u", None));
        let e = dav_changed(
            AccountScope {
                tenant: crate::TenantId::new(),
                account: crate::AccountId::new(),
            },
            CollectionKind::AddressBook,
            c.id,
            ModSeq(7),
        );
        let s = serde_json::to_string(&e).expect("json");
        assert_eq!(serde_json::from_str::<Event>(&s).expect("parse"), e);
    }
}
