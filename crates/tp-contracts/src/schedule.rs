//! Планирование встреч: iTIP, iMIP, входящие планирования, занятость (CTR-47; CONTRACTS.md#Schedule; SRS 4.4).
//!
//! iCalendar контракты не разбирают: разбор и построение iTIP (RFC 5546) — `tp_dav::itip` поверх calcard, общий для
//! CalDAV Scheduling (RFC 6638) и JMAP (`sendSchedulingMessages`).
//!
//! - [`ItipDelivery`] — приём iTIP учётной записью. Реализация — `tp-dav` (DAV-15); вызывают `tp-smtp` при доставке
//!   письма iMIP (RFC 6047, SMTP-33), `tp-dav` и `tp-jmap` при планировании внутри организации (DAV-13, JMAP-31).
//! - [`ItipSender`] — iTIP внешним участникам письмом iMIP через `queue::Outbound` (очередь и DKIM).
//! - Входящие планирования (`schedule-inbox`, RFC 6638 §2.2) — методы `inbox_*` [`crate::dav::DavStore`].
//! - [`FreeBusy`] — занятость: индекс [`crate::dav::ObjectIndex::busy`] неповторяющихся объектов и разворот повторов
//!   при запросе (META-54, решение 59); на нём строятся VFREEBUSY (RFC 6638 §5),
//!   JMAP `Principal/getAvailability` и `Availability/get` (`urn:thepost:availability`).

use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::bus::{Event, EventKind};
use crate::dav::{DavObjectId, MAX_UID_LEN};
use crate::error::{Error, Result};
use crate::ids::{AccountId, ModSeq, TenantId};
use crate::meta::AccountScope;
use crate::queue::Submitted;

/// Наибольшая длина адреса календарного пользователя (без `mailto:`), байт.
pub const MAX_ADDRESS_LEN: usize = 320;

/// Наибольшее число периодов занятости в индексе одного объекта ([`crate::dav::ObjectIndex::busy`]): больше —
/// вызывающий склеивает или обрезает по горизонту.
pub const MAX_BUSY_PERIODS: usize = 4096;

/// Адрес календарного пользователя без `mailto:`: 1–[`MAX_ADDRESS_LEN`] байт без управляющих символов и пробелов.
/// Регистр и нормализацию (подадрес, IDN) выполняет вызывающий.
pub fn valid_address(addr: &str) -> bool {
    !addr.is_empty()
        && addr.len() <= MAX_ADDRESS_LEN
        && !addr.chars().any(|c| c.is_control() || c.is_whitespace())
}

// ---------- iTIP ----------

/// Метод iTIP (RFC 5546 §1.4, свойство `METHOD`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ItipMethod {
    /// `PUBLISH`.
    Publish,
    /// `REQUEST`: приглашение, перенос, обновление.
    Request,
    /// `REPLY`: ответ участника.
    Reply,
    /// `ADD`: новые вхождения повторяющегося события.
    Add,
    /// `CANCEL`: отмена встречи или участия.
    Cancel,
    /// `REFRESH`: участник просит актуальную версию.
    Refresh,
    /// `COUNTER`: участник предлагает изменение.
    Counter,
    /// `DECLINECOUNTER`: организатор отклонил предложение.
    DeclineCounter,
}

impl ItipMethod {
    /// Все методы.
    pub const ALL: [ItipMethod; 8] = [
        ItipMethod::Publish,
        ItipMethod::Request,
        ItipMethod::Reply,
        ItipMethod::Add,
        ItipMethod::Cancel,
        ItipMethod::Refresh,
        ItipMethod::Counter,
        ItipMethod::DeclineCounter,
    ];

    /// Значение `METHOD`: `PUBLISH`, `REQUEST`… `DECLINECOUNTER`.
    pub const fn as_str(self) -> &'static str {
        match self {
            ItipMethod::Publish => "PUBLISH",
            ItipMethod::Request => "REQUEST",
            ItipMethod::Reply => "REPLY",
            ItipMethod::Add => "ADD",
            ItipMethod::Cancel => "CANCEL",
            ItipMethod::Refresh => "REFRESH",
            ItipMethod::Counter => "COUNTER",
            ItipMethod::DeclineCounter => "DECLINECOUNTER",
        }
    }

    /// Сообщение от организатора (`PUBLISH`, `REQUEST`, `ADD`, `CANCEL`, `DECLINECOUNTER`); иначе — от участника.
    pub const fn from_organizer(self) -> bool {
        matches!(
            self,
            ItipMethod::Publish
                | ItipMethod::Request
                | ItipMethod::Add
                | ItipMethod::Cancel
                | ItipMethod::DeclineCounter
        )
    }
}

impl fmt::Display for ItipMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ItipMethod {
    type Err = Error;

    /// Значение `METHOD` без учёта регистра; иное — `Error::Invalid`.
    fn from_str(s: &str) -> Result<Self> {
        ItipMethod::ALL
            .into_iter()
            .find(|m| m.as_str().eq_ignore_ascii_case(s))
            .ok_or_else(|| Error::invalid(format!("неизвестный метод iTIP: {s:?}")))
    }
}

/// Кто отправитель сообщения iTIP относительно организации получателя (решение 123) — первый аргумент матрицы
/// [`inbound_allowed`]. Своя организация — по входу в систему, а не по домену `From`; остальные — по домену `From`
/// письма через smtp-in (домен и его поддомены).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderKind {
    /// Своя организация — вошедший в систему: CalDAV и JMAP (`tp-dav`, `tp-jmap`) и письмо из почтового клиента
    /// (submission со входом, JMAP `EmailSubmission`; решение 78, SMTP-36), `From` которого принадлежит вошедшей
    /// учётной записи (свой адрес, псевдоним, право Send As или Send on Behalf).
    Own,
    /// Домен-партнёр организации получателя — официально заведённые доверительные отношения в политике организации
    /// (письмо iMIP через smtp-in, прошедшее DMARC).
    Partner,
    /// Другая организация той же установки (MSSP), не партнёр.
    Neighbor,
    /// Остальные, в том числе письмо через smtp-in с адресом своей организации: DMARC проходит и сторонний сервис,
    /// разрешённый в SPF или DKIM домена.
    External,
}

/// Что сообщение iTIP делает у получателя — второй аргумент матрицы [`inbound_allowed`]. Каждая клетка — отдельное
/// решение «кто — что»; содержательные проверки (UID, `ORGANIZER`, `ATTENDEE`, `SEQUENCE`) — у приёма, сверх неё.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboundAction {
    /// Новое приглашение (`REQUEST`) сотруднику: события с этим UID у получателя нет (или оно не того организатора, или
    /// `SEQUENCE` не больше).
    Invite,
    /// Обновление (`REQUEST`) события, уже внесённого в календарь сотрудника: тот же `ORGANIZER`, `SEQUENCE` строго
    /// больше, чем у события получателя (как у Exchange).
    Update,
    /// Ответ участника (`REPLY`) на событие нашего организатора: статус участника в событии организатора.
    Reply,
    /// Отмена (`CANCEL`) события в календаре сотрудника от его организатора.
    Cancel,
    /// Прочие методы у сотрудника: `ADD`, `REFRESH`, `COUNTER`, `DECLINECOUNTER` (`PUBLISH` не применяется никогда).
    Other,
    /// Любое сообщение ресурсу (переговорной, оборудованию): бронирование и ответ ресурса организатору.
    Booking,
}

impl InboundAction {
    /// Действие сообщения `method` получателю; `resource` — получатель — ресурс; `update` — `REQUEST` к событию,
    /// уже внесённому в календарь получателя, того же `ORGANIZER` и со строго большим `SEQUENCE` (определяет приём).
    pub const fn of(method: ItipMethod, resource: bool, update: bool) -> Self {
        match method {
            _ if resource => InboundAction::Booking,
            ItipMethod::Request if update => InboundAction::Update,
            ItipMethod::Request => InboundAction::Invite,
            ItipMethod::Reply => InboundAction::Reply,
            ItipMethod::Cancel => InboundAction::Cancel,
            _ => InboundAction::Other,
        }
    }
}

/// Матрица «кто — что» приёма iTIP (решение 123, «Снаружи — только почта») — единственное место решения «применять,
/// бронировать, отвечать»; позже — настройка администратора (политика или Sieve) с этими значениями по умолчанию.
///
/// | Отправитель \ действие | `Invite` | `Update` | `Reply` | `Cancel` | `Other` | `Booking` |
/// |---|---|---|---|---|---|---|
/// | `Own` — своя организация (вошедший) | да | да | да | да | да | да |
/// | `Partner` — домен-партнёр | нет¹ | да | да | да | нет | нет |
/// | `Neighbor` — соседняя организация установки | нет | да | да | да | нет | нет |
/// | `External` — внешний | нет | да | да | да | нет | нет |
///
/// «Нет» — письмо остаётся письмом: ни события, ни брони, ни ответа. `Update`, `Reply` и `Cancel` извне — как у
/// Exchange и Google: только к существующему событию (RFC 5546 §6.1, RFC 6638 §11.2) — обновление и отмена от
/// организатора события (тот же `ORGANIZER`, у обновления `SEQUENCE` строго больше) получателю из `ATTENDEE`, `REPLY`
/// от участника этого события его организатору; UID совпадает, письмо прошло DMARC; проверяет приём. ¹ Новое
/// приглашение партнёра — письмом, пока администратор не включит автодобавление (настройки ещё нет).
pub const fn inbound_allowed(sender: SenderKind, action: InboundAction) -> bool {
    use InboundAction::{Booking, Cancel, Invite, Other, Reply, Update};
    match (sender, action) {
        (SenderKind::Own, _) => true,
        (
            SenderKind::Partner | SenderKind::Neighbor | SenderKind::External,
            Update | Reply | Cancel,
        ) => true,
        (
            SenderKind::Partner | SenderKind::Neighbor | SenderKind::External,
            Invite | Other | Booking,
        ) => false,
    }
}

/// Сообщение iTIP одному получателю — учётной записи организации.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItipMessage {
    /// Учётная запись получателя.
    pub recipient: AccountScope,
    /// Адрес получателя (`ATTENDEE` или `ORGANIZER` в сообщении; у iMIP — получатель конверта без подадреса).
    pub to: String,
    /// Адрес отправителя, проверенный вызывающим ([`SenderKind`]); сверяется с `ORGANIZER` или `ATTENDEE`.
    pub from: String,
    /// Метод (`METHOD` объекта; у iMIP — и параметр `method` части `text/calendar`).
    pub method: ItipMethod,
    /// Объект iCalendar как получен.
    pub ics: String,
    /// Кто отправитель относительно организации получателя; что ему можно — [`inbound_allowed`].
    pub sender: SenderKind,
}

impl ItipMessage {
    /// Проверяет адреса ([`valid_address`]) и непустой `ics`; иначе — `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if !valid_address(&self.to) || !valid_address(&self.from) {
            return Err(Error::invalid("недопустимый адрес сообщения iTIP"));
        }
        if self.ics.trim().is_empty() {
            return Err(Error::invalid("пустое сообщение iTIP"));
        }
        Ok(())
    }
}

/// Итог [`ItipDelivery::deliver`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ItipOutcome {
    /// Применено: событие в календаре получателя создано или изменено, сообщение — во входящих планирования.
    /// Повторная доставка того же сообщения — тоже `Applied`, без новых изменений.
    Applied,
    /// Не применено, причина — для журнала (без содержимого): неразборчивый iCalendar, отправитель не организатор
    /// и не участник, устаревший `SEQUENCE`, чужой `UID` организатора, метод не поддерживается.
    Ignored(String),
}

/// Приём iTIP учётной записью (RFC 5546): `REQUEST`, `ADD` — событие в календаре по умолчанию (`PARTSTAT=NEEDS-ACTION`),
/// `REPLY` — статус участника у организатора, `CANCEL` — `STATUS:CANCELLED`, `REFRESH`, `COUNTER`,
/// `DECLINECOUNTER` — во входящие планирования. Реализация — `tp-dav` (DAV-15); заглушка — `tp-testkit::FakeItip`.
#[async_trait]
pub trait ItipDelivery: Send + Sync + 'static {
    /// Применяет сообщение. Недопустимое сообщение ([`ItipMessage::validate`]) и отказ по содержанию —
    /// `Ok(Ignored)`; `Err` — сбой хранилища: доставка идемпотентна, повтор безопасен. Письмо iMIP при любом итоге
    /// остаётся в ящике (`tp-smtp` пишет итог в журнал доставки).
    async fn deliver(&self, msg: &ItipMessage) -> Result<ItipOutcome>;
}

/// Сообщение iTIP внешним участникам письмом iMIP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItipOutgoing {
    /// Учётная запись отправителя: организатор (`REQUEST`, `CANCEL`…) или участник (`REPLY`, `COUNTER`…).
    pub sender: AccountScope,
    /// Адрес отправителя — `ORGANIZER` или `ATTENDEE` в объекте, один из адресов учётной записи (проверяет вызывающий);
    /// `From` письма и `MAIL FROM`.
    pub from: String,
    /// Получатели — внешние адреса, не пусто.
    pub to: Vec<String>,
    /// Метод.
    pub method: ItipMethod,
    /// Объект iCalendar с `METHOD`.
    pub ics: String,
    /// Тема письма («Приглашение: …»), без управляющих символов.
    pub subject: String,
    /// Текст для клиентов без поддержки iMIP (`text/plain`).
    pub text: String,
}

impl ItipOutgoing {
    /// Проверяет адреса ([`valid_address`]), непустых получателей, непустой `ics`, тему без управляющих символов;
    /// иначе — `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if !valid_address(&self.from)
            || self.to.is_empty()
            || !self.to.iter().all(|a| valid_address(a))
        {
            return Err(Error::invalid("недопустимые адреса письма iMIP"));
        }
        if self.ics.trim().is_empty() {
            return Err(Error::invalid("пустое сообщение iTIP"));
        }
        if self.subject.chars().any(char::is_control) {
            return Err(Error::invalid("недопустимая тема письма iMIP"));
        }
        Ok(())
    }
}

/// Отправка iTIP внешним участникам письмом iMIP (RFC 6047): `multipart/alternative` с `text/plain` и
/// `text/calendar; method=…`, постановка через `queue::Outbound::submit` от имени организации отправителя (подпись
/// DKIM — в очереди). Реализация — `tp-dav` поверх `Outbound` (сборка в узле — APP-28); in-memory —
/// `tp-testkit::MemItipSender`.
#[async_trait]
pub trait ItipSender: Send + Sync + 'static {
    /// Отправляет письмо; недопустимое ([`ItipOutgoing::validate`]) — `Invalid`. Возвращает статусы получателей
    /// ([`Submitted`], порядок `to`); ошибки — как у `Outbound::submit`.
    async fn send(&self, msg: &ItipOutgoing) -> Result<Submitted>;
}

// ---------- Входящие планирования ----------

/// Сообщение iTIP во входящих планирования (RFC 6638 §2.2): то, что вызывающий кладёт в [`crate::dav::DavStore::inbox_put`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxMessage {
    /// Метод.
    pub method: ItipMethod,
    /// UID события: 1–[`MAX_UID_LEN`] байт без управляющих символов; во входящих не уникален (`REQUEST`, затем `CANCEL`).
    pub uid: String,
    /// Адрес отправителя ([`valid_address`]).
    pub from: String,
    /// Тело: объект iCalendar с `METHOD`.
    pub body: String,
}

impl InboxMessage {
    /// Проверяет UID и адрес; иначе — `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if self.uid.is_empty()
            || self.uid.len() > MAX_UID_LEN
            || self.uid.chars().any(char::is_control)
        {
            return Err(Error::invalid("недопустимый UID сообщения"));
        }
        if !valid_address(&self.from) {
            return Err(Error::invalid("недопустимый адрес отправителя"));
        }
        Ok(())
    }
}

/// Ресурс входящих планирования.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InboxItem {
    /// Идентификатор (JMAP `CalendarEventNotification`); прежний при перезаписи.
    pub id: DavObjectId,
    /// Имя ресурса — сегмент URL ([`crate::dav::valid_name`]).
    pub name: String,
    /// ETag без кавычек; новый при каждой записи.
    pub etag: String,
    /// `ModSeq` последней записи.
    pub modseq: ModSeq,
    /// Время поступления (UTC; CTR-65, JMAP `CalendarEventNotification.created`): первой записи ресурса, перезапись его
    /// не меняет. Задаёт хранилище.
    pub received_at: OffsetDateTime,
    /// Сообщение.
    pub message: InboxMessage,
}

/// Входящие планирования учётной записи ([`crate::dav::DavStore::inbox_list`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Inbox {
    /// Ресурсы по `ModSeq` (порядок поступления).
    pub items: Vec<InboxItem>,
    /// `ModSeq` последнего изменения входящих (запись или удаление; CTag `schedule-inbox`, JMAP `state`
    /// `CalendarEventNotification`); без изменений — `ModSeq::ZERO`.
    pub state: ModSeq,
}

/// Сигнал об изменении входящих планирования учётной записи `scope` ([`EventKind::ScheduleInboxChanged`]): публикуется
/// после фиксации каждого `inbox_put` и `inbox_delete` (JMAP push `CalendarEventNotification`).
pub fn schedule_inbox_changed(scope: AccountScope, modseq: ModSeq) -> Event {
    Event {
        tenant: scope.tenant,
        account: Some(scope.account),
        kind: EventKind::ScheduleInboxChanged { modseq },
    }
}

// ---------- Занятость ----------

/// Вид занятости (RFC 5545 `FBTYPE`); свободное время не хранится.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BusyKind {
    /// `BUSY`: событие `STATUS:CONFIRMED` или без статуса.
    Busy,
    /// `BUSY-TENTATIVE`: `STATUS:TENTATIVE`.
    Tentative,
    /// `BUSY-UNAVAILABLE`: недоступен (`VAVAILABILITY`, нерабочее время ресурса).
    Unavailable,
}

impl BusyKind {
    /// Значение `FBTYPE`.
    pub const fn as_str(self) -> &'static str {
        match self {
            BusyKind::Busy => "BUSY",
            BusyKind::Tentative => "BUSY-TENTATIVE",
            BusyKind::Unavailable => "BUSY-UNAVAILABLE",
        }
    }
}

/// Интервал `[start, end)` (UTC) с обеими границами: запрос занятости.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Period {
    /// Начало.
    pub start: OffsetDateTime,
    /// Конец, позже начала.
    pub end: OffsetDateTime,
}

impl Period {
    /// Конец позже начала; иначе — `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if self.end <= self.start {
            return Err(Error::invalid("конец интервала не позже начала"));
        }
        Ok(())
    }
}

/// Период занятости `[start, end)` (UTC).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BusyPeriod {
    /// Начало.
    pub start: OffsetDateTime,
    /// Конец, позже начала.
    pub end: OffsetDateTime,
    /// Вид.
    pub kind: BusyKind,
}

impl BusyPeriod {
    /// Пересекается с `range`: `start < range.end` и `end > range.start`.
    pub fn overlaps(&self, range: &Period) -> bool {
        self.start < range.end && self.end > range.start
    }

    /// Часть периода внутри `range`; не пересекается — `None`.
    pub fn clip(&self, range: &Period) -> Option<BusyPeriod> {
        self.overlaps(range).then(|| BusyPeriod {
            start: self.start.max(range.start),
            end: self.end.min(range.end),
            kind: self.kind,
        })
    }
}

/// Нормализует периоды: по `(start, end, kind)`, пересекающиеся и смежные периоды одного вида склеены; периоды
/// разных видов не склеиваются и могут пересекаться. Результат [`FreeBusy::query`] нормализован.
pub fn merge_busy(mut periods: Vec<BusyPeriod>) -> Vec<BusyPeriod> {
    periods.sort_by_key(|p| (p.kind, p.start, p.end));
    let mut out: Vec<BusyPeriod> = Vec::with_capacity(periods.len());
    for p in periods {
        match out.last_mut() {
            Some(last) if last.kind == p.kind && p.start <= last.end => {
                last.end = last.end.max(p.end);
            }
            _ => out.push(p),
        }
    }
    out.sort_by_key(|p| (p.start, p.end, p.kind));
    out
}

/// Проверяет индекс занятости объекта: каждый период — конец позже начала, не больше [`MAX_BUSY_PERIODS`];
/// иначе — `Error::Invalid`. Вызывает [`crate::dav::ObjectIndex::validate`].
pub fn validate_busy(periods: &[BusyPeriod]) -> Result<()> {
    if periods.len() > MAX_BUSY_PERIODS {
        return Err(Error::invalid("слишком много периодов занятости"));
    }
    if periods.iter().any(|p| p.end <= p.start) {
        return Err(Error::invalid("конец периода занятости не позже начала"));
    }
    Ok(())
}

/// Занятость учётных записей по календарям, кроме прозрачных ([`crate::dav::Collection::transparent`]).
/// Неповторяющиеся объекты — по индексу [`crate::dav::ObjectIndex::busy`] (вычисляет `tp-dav`, DAV-16).
/// Повторяющиеся ([`crate::dav::ObjectIndex::recurring`]; META-54, решение 59) хранилище разворачивает из тела при
/// запросе библиотекой calcard: `RRULE`, `RDATE`, `EXDATE`, переопределённые вхождения (`RECURRENCE-ID`), часовые
/// пояса; время без пояса — в поясе календаря ([`crate::dav::Collection::timezone`], иначе UTC); `TRANSP:TRANSPARENT`
/// и `STATUS:CANCELLED` не занимают, `STATUS:TENTATIVE` — [`BusyKind::Tentative`]; горизонта нет, от начала повторов
/// разворачивается не больше [`crate::dav::DavLimits::max_busy_instances`] вхождений объекта; тело, которое не
/// разбирается, занятости не даёт. Реализации — хранилища (`MemDav`, `PgMeta` — META-51, META-54). Права на чтение занятости и политику
/// организации проверяет вызывающий (CTR-48).
#[async_trait]
pub trait FreeBusy: Send + Sync + 'static {
    /// Занятость учётных записей `accounts` организации `tenant` в интервале `range`: по записи на каждую учётную
    /// запись в порядке `accounts` (повторы — один раз), периоды пересекающиеся с `range`, обрезанные по нему и
    /// нормализованные ([`merge_busy`]). Учётная запись без календарей, чужой организации или несуществующая — пустой
    /// список (существование проверяет вызывающий). Недопустимый `range` — `Invalid`.
    async fn query(
        &self,
        tenant: TenantId,
        accounts: &[AccountId],
        range: Period,
    ) -> Result<Vec<(AccountId, Vec<BusyPeriod>)>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn t(h: i64) -> OffsetDateTime {
        OffsetDateTime::UNIX_EPOCH + time::Duration::hours(h)
    }

    fn busy(s: i64, e: i64, kind: BusyKind) -> BusyPeriod {
        BusyPeriod {
            start: t(s),
            end: t(e),
            kind,
        }
    }

    #[test]
    fn object_safe() {
        fn _delivery(_: Arc<dyn ItipDelivery>) {}
        fn _sender(_: Arc<dyn ItipSender>) {}
        fn _freebusy(_: Arc<dyn FreeBusy>) {}
    }

    #[test]
    fn methods() {
        for m in ItipMethod::ALL {
            assert_eq!(m.as_str().parse::<ItipMethod>(), Ok(m));
            assert_eq!(m.to_string(), m.as_str());
        }
        assert_eq!("reply".parse::<ItipMethod>(), Ok(ItipMethod::Reply));
        assert!("VEVENT".parse::<ItipMethod>().is_err());
        assert!(ItipMethod::Request.from_organizer());
        assert!(ItipMethod::DeclineCounter.from_organizer());
        assert!(!ItipMethod::Reply.from_organizer());
        assert!(!ItipMethod::Counter.from_organizer());
        assert!(!ItipMethod::Refresh.from_organizer());
    }

    #[test]
    fn inbound_matrix() {
        use InboundAction::{Booking, Cancel, Invite, Other, Reply, Update};
        use SenderKind::{External, Neighbor, Own, Partner};
        for action in [Invite, Update, Reply, Cancel, Other, Booking] {
            assert!(inbound_allowed(Own, action));
            for sender in [Partner, Neighbor, External] {
                assert_eq!(
                    inbound_allowed(sender, action),
                    matches!(action, Update | Reply | Cancel),
                    "{sender:?} {action:?}"
                );
            }
        }
        assert_eq!(InboundAction::of(ItipMethod::Request, false, false), Invite);
        assert_eq!(InboundAction::of(ItipMethod::Request, false, true), Update);
        assert_eq!(InboundAction::of(ItipMethod::Reply, false, false), Reply);
        assert_eq!(InboundAction::of(ItipMethod::Cancel, false, false), Cancel);
        for m in ItipMethod::ALL {
            assert_eq!(InboundAction::of(m, true, true), Booking);
            assert_eq!(InboundAction::of(m, true, false), Booking);
        }
        for m in [
            ItipMethod::Add,
            ItipMethod::Refresh,
            ItipMethod::Counter,
            ItipMethod::DeclineCounter,
        ] {
            assert_eq!(InboundAction::of(m, false, false), Other);
        }
    }

    #[test]
    fn addresses() {
        assert!(valid_address("anna@example.org"));
        for bad in [
            "",
            "a b@example.org",
            "a\n@x",
            &"a".repeat(MAX_ADDRESS_LEN + 1),
        ] {
            assert!(!valid_address(bad), "{bad:?}");
        }
    }

    #[test]
    fn messages_validate() {
        let scope = AccountScope {
            tenant: TenantId::new(),
            account: AccountId::new(),
        };
        let msg = ItipMessage {
            recipient: scope,
            to: "anna@example.org".into(),
            from: "boss@partner.example".into(),
            method: ItipMethod::Request,
            ics: "BEGIN:VCALENDAR\r\nEND:VCALENDAR\r\n".into(),
            sender: SenderKind::Partner,
        };
        assert_eq!(msg.validate(), Ok(()));
        assert!(
            ItipMessage {
                ics: " \r\n".into(),
                ..msg.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            ItipMessage {
                from: String::new(),
                ..msg.clone()
            }
            .validate()
            .is_err()
        );
        let out = ItipOutgoing {
            sender: scope,
            from: "anna@example.org".into(),
            to: vec!["guest@partner.example".into()],
            method: ItipMethod::Reply,
            ics: msg.ics.clone(),
            subject: "Принято: планёрка".into(),
            text: String::new(),
        };
        assert_eq!(out.validate(), Ok(()));
        for bad in [
            ItipOutgoing {
                to: Vec::new(),
                ..out.clone()
            },
            ItipOutgoing {
                to: vec!["a b".into()],
                ..out.clone()
            },
            ItipOutgoing {
                subject: "a\r\nBcc: x".into(),
                ..out.clone()
            },
        ] {
            assert!(bad.validate().is_err(), "{bad:?}");
        }
        let inbox = InboxMessage {
            method: ItipMethod::Cancel,
            uid: "u1".into(),
            from: "boss@partner.example".into(),
            body: msg.ics,
        };
        assert_eq!(inbox.validate(), Ok(()));
        assert!(
            InboxMessage {
                uid: String::new(),
                ..inbox.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            InboxMessage {
                from: "x y".into(),
                ..inbox
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn busy_periods() {
        let range = Period {
            start: t(10),
            end: t(12),
        };
        assert_eq!(range.validate(), Ok(()));
        assert!(
            Period {
                start: t(1),
                end: t(1)
            }
            .validate()
            .is_err()
        );
        assert!(busy(9, 11, BusyKind::Busy).overlaps(&range));
        assert!(
            !busy(12, 13, BusyKind::Busy).overlaps(&range),
            "конец не включён"
        );
        assert!(!busy(8, 10, BusyKind::Busy).overlaps(&range));
        assert_eq!(
            busy(9, 13, BusyKind::Tentative).clip(&range),
            Some(busy(10, 12, BusyKind::Tentative))
        );
        assert_eq!(busy(1, 2, BusyKind::Busy).clip(&range), None);
        assert_eq!(validate_busy(&[busy(1, 2, BusyKind::Busy)]), Ok(()));
        assert!(validate_busy(&[busy(2, 2, BusyKind::Busy)]).is_err());
        assert!(validate_busy(&vec![busy(1, 2, BusyKind::Busy); MAX_BUSY_PERIODS + 1]).is_err());
    }

    #[test]
    fn merge() {
        let merged = merge_busy(vec![
            busy(5, 6, BusyKind::Busy),
            busy(1, 3, BusyKind::Busy),
            busy(2, 4, BusyKind::Busy),
            busy(4, 5, BusyKind::Busy),
            busy(2, 3, BusyKind::Tentative),
            busy(8, 9, BusyKind::Busy),
        ]);
        assert_eq!(
            merged,
            vec![
                busy(1, 6, BusyKind::Busy),
                busy(2, 3, BusyKind::Tentative),
                busy(8, 9, BusyKind::Busy),
            ]
        );
        assert_eq!(merge_busy(Vec::new()), Vec::new());
    }

    #[test]
    fn serde_roundtrip() {
        let scope = AccountScope {
            tenant: TenantId::new(),
            account: AccountId::new(),
        };
        let e = schedule_inbox_changed(scope, ModSeq(3));
        let s = serde_json::to_string(&e).expect("json");
        assert_eq!(serde_json::from_str::<Event>(&s).expect("parse"), e);
        let p = busy(1, 2, BusyKind::Unavailable);
        let s = serde_json::to_string(&p).expect("json");
        assert!(s.contains("\"unavailable\""), "{s}");
        assert_eq!(serde_json::from_str::<BusyPeriod>(&s).expect("parse"), p);
        assert_eq!(BusyKind::Tentative.as_str(), "BUSY-TENTATIVE");
    }
}
