//! iTIP (RFC 5546) поверх calcard: разбор объектов планирования и построение сообщений `REQUEST`, `CANCEL`,
//! `REPLY`. Общий модуль CalDAV Scheduling (RFC 6638, [`crate::schedule`]) и JMAP (`sendSchedulingMessages`,
//! `tp-jmap`).
//!
//! - [`meeting`] — участники объекта: UID, организатор, участники со статусами, `SEQUENCE`;
//! - [`parse`] — сообщение iTIP (объект с `METHOD`);
//! - [`plan`] — какие сообщения следуют из изменения объекта владельцем (создание, правка, удаление): организатор —
//!   `REQUEST` и `CANCEL` участникам, участник — `REPLY` организатору;
//! - [`request`], [`cancel`], [`reply`] — построение отдельных сообщений;
//! - [`schedule_tag`] — `Schedule-Tag` (RFC 6638 §3.2): не зависит от `PARTSTAT`, меняется от существенных правок;
//! - [`rescheduled`] — изменилось время (перенос);
//! - [`set_schedule_status`] — `SCHEDULE-STATUS` участников и организатора в объекте.
//!
//! Адрес календарного пользователя — без `mailto:`, в нижнем регистре ([`address`]). Объект планирования — VEVENT
//! или VTODO с `ORGANIZER` и хотя бы одним `ATTENDEE`. Участник с `SCHEDULE-AGENT=CLIENT` или `NONE` сервером не
//! обслуживается (RFC 6638 §7.1).

use std::collections::BTreeSet;
use std::hash::{Hash, Hasher};

use calcard::common::PartialDateTime;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarEntry, ICalendarMethod,
    ICalendarParameter, ICalendarParameterName, ICalendarParticipationStatus, ICalendarProperty,
    ICalendarStatus, ICalendarValue,
};
use time::OffsetDateTime;
use tp_contracts::schedule::ItipMethod;

use crate::props::PRODID;

/// Статус участия по умолчанию (RFC 5545 §3.2.12).
pub const NEEDS_ACTION: &str = "NEEDS-ACTION";

/// Адрес календарного пользователя: без `mailto:` и пробелов по краям, в нижнем регистре.
pub fn address(value: &str) -> String {
    let v = value.trim();
    let v = match v.split_once(':') {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("mailto") => rest,
        _ => v,
    };
    v.trim().to_lowercase()
}

/// Участник объекта планирования.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attendee {
    /// Адрес ([`address`]).
    pub address: String,
    /// `PARTSTAT` в верхнем регистре; нет параметра — [`NEEDS_ACTION`].
    pub partstat: String,
    /// Планирует сервер: `SCHEDULE-AGENT` нет или `SERVER`.
    pub server: bool,
}

/// Сведения о планировании объекта (по всем его компонентам VEVENT и VTODO).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meeting {
    /// UID.
    pub uid: String,
    /// Адрес организатора.
    pub organizer: Option<String>,
    /// Организатора уведомляет сервер: у `ORGANIZER` нет `SCHEDULE-AGENT` или он `SERVER`.
    pub organizer_server: bool,
    /// Участники по первому упоминанию (основной компонент — раньше исключений).
    pub attendees: Vec<Attendee>,
    /// Наибольший `SEQUENCE` компонентов.
    pub sequence: i64,
    /// `SUMMARY` первого компонента.
    pub summary: Option<String>,
    /// Все компоненты — `STATUS:CANCELLED`.
    pub cancelled: bool,
    /// Есть VEVENT (иначе — VTODO).
    pub event: bool,
}

impl Meeting {
    /// Объект планирования: есть организатор и участники.
    pub fn is_scheduling(&self) -> bool {
        self.organizer.is_some() && !self.attendees.is_empty()
    }

    /// Участник с адресом `addr` ([`address`]).
    pub fn attendee(&self, addr: &str) -> Option<&Attendee> {
        self.attendees.iter().find(|a| a.address == addr)
    }
}

/// Компонент планирования: VEVENT или VTODO.
pub(crate) fn is_sched(c: &ICalendarComponent) -> bool {
    matches!(
        c.component_type,
        ICalendarComponentType::VEvent | ICalendarComponentType::VTodo
    )
}

fn sched(ical: &ICalendar) -> impl Iterator<Item = &ICalendarComponent> {
    ical.components.iter().filter(|c| is_sched(c))
}

pub(crate) fn entry_address(e: &ICalendarEntry) -> Option<String> {
    e.calendar_address().map(address).filter(|a| !a.is_empty())
}

fn param(e: &ICalendarEntry, name: &ICalendarParameterName) -> Option<String> {
    e.parameter(name)
        .and_then(|v| v.as_text())
        .map(str::to_uppercase)
}

fn server_agent(e: &ICalendarEntry) -> bool {
    param(e, &ICalendarParameterName::ScheduleAgent).is_none_or(|v| v == "SERVER")
}

fn partstat_of(e: &ICalendarEntry) -> String {
    param(e, &ICalendarParameterName::Partstat).unwrap_or_else(|| NEEDS_ACTION.to_owned())
}

fn sequence_of(c: &ICalendarComponent) -> i64 {
    c.property(&ICalendarProperty::Sequence)
        .and_then(|e| e.values.first())
        .and_then(ICalendarValue::as_integer)
        .unwrap_or(0)
}

fn text_of(c: &ICalendarComponent, p: &ICalendarProperty) -> Option<String> {
    c.property(p)
        .and_then(|e| e.values.first())
        .and_then(|v| v.as_text())
        .map(str::to_owned)
}

fn is_master(c: &ICalendarComponent) -> bool {
    !c.has_property(&ICalendarProperty::RecurrenceId)
}

/// Запись свойства строкой iCalendar (для сравнения и хеша).
pub(crate) fn line(e: &ICalendarEntry) -> String {
    let mut s = String::new();
    let _ = e.write_to(&mut s);
    s
}

/// `RECURRENCE-ID` компонента строкой; основной компонент — `None`.
pub(crate) fn recurrence_id(c: &ICalendarComponent) -> Option<String> {
    c.property(&ICalendarProperty::RecurrenceId).map(line)
}

/// Сведения о планировании; `None` — нет VEVENT или VTODO с UID.
pub fn meeting(ical: &ICalendar) -> Option<Meeting> {
    let mut m: Option<Meeting> = None;
    let mut cancelled = true;
    for c in sched(ical) {
        let m = match &mut m {
            Some(m) => m,
            None => m.insert(Meeting {
                uid: c.uid()?.to_owned(),
                organizer: None,
                organizer_server: true,
                attendees: Vec::new(),
                sequence: 0,
                summary: text_of(c, &ICalendarProperty::Summary),
                cancelled: false,
                event: false,
            }),
        };
        m.event |= c.component_type == ICalendarComponentType::VEvent;
        m.sequence = m.sequence.max(sequence_of(c));
        cancelled &= c.status() == Some(&ICalendarStatus::Cancelled);
        if m.organizer.is_none()
            && let Some(o) = c.property(&ICalendarProperty::Organizer)
        {
            m.organizer = entry_address(o);
            m.organizer_server = server_agent(o);
        }
        for e in c.properties(&ICalendarProperty::Attendee) {
            let Some(addr) = entry_address(e) else {
                continue;
            };
            if m.attendee(&addr).is_none() {
                m.attendees.push(Attendee {
                    address: addr,
                    partstat: partstat_of(e),
                    server: server_agent(e),
                });
            }
        }
    }
    let mut m = m?;
    m.cancelled = cancelled;
    Some(m)
}

/// `METHOD` объекта.
pub fn method(ical: &ICalendar) -> Option<ItipMethod> {
    ical.components
        .first()?
        .property(&ICalendarProperty::Method)?
        .values
        .first()?
        .as_text()?
        .parse()
        .ok()
}

/// Сообщение iTIP не разобрано.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ItipError {
    /// Не разбирается как VCALENDAR.
    #[error("недопустимые данные iCalendar")]
    Calendar,
    /// Нет `METHOD` или он неизвестен.
    #[error("нет допустимого METHOD")]
    Method,
    /// Нет VEVENT или VTODO с UID.
    #[error("нет компонента с UID")]
    Component,
}

/// Разобранное сообщение iTIP.
#[derive(Debug, Clone, PartialEq)]
pub struct Itip {
    /// Метод.
    pub method: ItipMethod,
    /// Сведения о планировании.
    pub meeting: Meeting,
    /// Объект как получен.
    pub ical: ICalendar,
}

/// Разбирает сообщение iTIP: VCALENDAR с `METHOD` и компонентом VEVENT или VTODO с UID.
pub fn parse(ics: &str) -> Result<Itip, ItipError> {
    let ical = ICalendar::parse(ics).map_err(|_| ItipError::Calendar)?;
    if ical.components.first().map(|c| &c.component_type)
        != Some(&ICalendarComponentType::VCalendar)
    {
        return Err(ItipError::Calendar);
    }
    let method = method(&ical).ok_or(ItipError::Method)?;
    let meeting = meeting(&ical).ok_or(ItipError::Component)?;
    Ok(Itip {
        method,
        meeting,
        ical,
    })
}

// ---------- Построение ----------

fn stamp(now: OffsetDateTime) -> ICalendarEntry {
    ICalendarEntry::new(ICalendarProperty::Dtstamp)
        .with_value(PartialDateTime::from_utc_timestamp(now.unix_timestamp()))
}

/// Добавляет компонент `comp` верхнего уровня; с `children` — его вложенные компоненты из `src`.
pub(crate) fn push(
    dst: &mut ICalendar,
    src: &ICalendar,
    mut comp: ICalendarComponent,
    children: bool,
) {
    let ids = std::mem::take(&mut comp.component_ids);
    let id = dst.components.len();
    dst.components.push(comp);
    if let (Some(root), Ok(id)) = (dst.components.first_mut(), u32::try_from(id)) {
        root.component_ids.push(id);
    }
    if !children {
        return;
    }
    for child in ids {
        let Some(c) = src.component_by_id(child) else {
            continue;
        };
        let mut c = c.clone();
        // Вложенность глубже одного уровня (VTIMEZONE → STANDARD) в iCalendar не встречается.
        c.component_ids.clear();
        if let Ok(cid) = u32::try_from(dst.components.len()) {
            dst.components[id].component_ids.push(cid);
            dst.components.push(c);
        }
    }
}

/// Пустое сообщение с методом и часовыми поясами `src`.
fn envelope(src: &ICalendar, method: ICalendarMethod) -> ICalendar {
    let mut root = ICalendarComponent::new(ICalendarComponentType::VCalendar);
    root.add_property(ICalendarProperty::Version, "2.0");
    root.add_property(ICalendarProperty::Prodid, PRODID);
    root.add_property(ICalendarProperty::Method, method);
    let mut out = ICalendar {
        components: vec![root],
    };
    for c in &src.components {
        if c.component_type == ICalendarComponentType::VTimezone {
            push(&mut out, src, c.clone(), true);
        }
    }
    out
}

/// Убирает служебные параметры планирования (`SCHEDULE-STATUS`, `SCHEDULE-AGENT`, `SCHEDULE-FORCE-SEND`).
fn clean(mut e: ICalendarEntry) -> ICalendarEntry {
    e.params.retain(|p| {
        !matches!(
            p.name,
            ICalendarParameterName::ScheduleStatus
                | ICalendarParameterName::ScheduleAgent
                | ICalendarParameterName::ScheduleForceSend
        )
    });
    e
}

fn lists(c: &ICalendarComponent, addr: &str) -> bool {
    attendee_entry(c, addr).is_some()
}

/// `ATTENDEE` компонента с адресом `addr`.
pub(crate) fn attendee_entry<'a>(
    c: &'a ICalendarComponent,
    addr: &str,
) -> Option<&'a ICalendarEntry> {
    c.properties(&ICalendarProperty::Attendee)
        .find(|e| entry_address(e).as_deref() == Some(addr))
}

/// `ATTENDEE` компонента с адресом `addr` для правки.
pub(crate) fn attendee_entry_mut<'a>(
    c: &'a mut ICalendarComponent,
    addr: &str,
) -> Option<&'a mut ICalendarEntry> {
    c.entries.iter_mut().find(|e| {
        e.name == ICalendarProperty::Attendee && entry_address(e).as_deref() == Some(addr)
    })
}

/// Заменяет параметр `name` записи на `value` (`None` — убирает).
pub(crate) fn replace_param(
    e: &mut ICalendarEntry,
    name: &ICalendarParameterName,
    value: Option<ICalendarParameter>,
) {
    e.params.retain(|p| p.name != *name);
    e.params.extend(value);
}

/// Параметр `name` записи.
pub(crate) fn param_of(
    e: &ICalendarEntry,
    name: &ICalendarParameterName,
) -> Option<ICalendarParameter> {
    e.params.iter().find(|p| p.name == *name).cloned()
}

/// `REQUEST` участнику `to`: компоненты объекта, где он участник (весь ряд или отдельные вхождения), без
/// напоминаний и служебных параметров планирования. `None` — участника нет ни в одном компоненте.
pub fn request(ical: &ICalendar, to: &str, now: OffsetDateTime) -> Option<ICalendar> {
    let mut out = envelope(ical, ICalendarMethod::Request);
    let mut any = false;
    for c in sched(ical).filter(|c| lists(c, to)) {
        let mut c = c.clone();
        c.component_ids.clear();
        c.entries.retain(|e| e.name != ICalendarProperty::Dtstamp);
        c.entries = std::mem::take(&mut c.entries)
            .into_iter()
            .map(|e| match e.name {
                ICalendarProperty::Attendee | ICalendarProperty::Organizer => clean(e),
                _ => e,
            })
            .collect();
        c.entries.push(stamp(now));
        push(&mut out, ical, c, false);
        any = true;
    }
    any.then_some(out)
}

/// Свойства, переносимые в `CANCEL` и `REPLY` как есть.
const IDENTITY: [ICalendarProperty; 7] = [
    ICalendarProperty::Uid,
    ICalendarProperty::RecurrenceId,
    ICalendarProperty::Dtstart,
    ICalendarProperty::Dtend,
    ICalendarProperty::Duration,
    ICalendarProperty::Summary,
    ICalendarProperty::Location,
];

fn skeleton(c: &ICalendarComponent, now: OffsetDateTime, sequence: i64) -> ICalendarComponent {
    let mut out = ICalendarComponent::new(c.component_type.clone());
    out.entries = c
        .entries
        .iter()
        .filter(|e| IDENTITY.contains(&e.name))
        .cloned()
        .collect();
    out.entries.push(stamp(now));
    out.add_sequence(sequence);
    if let Some(o) = c.property(&ICalendarProperty::Organizer) {
        out.entries.push(clean(o.clone()));
    }
    out
}

/// `CANCEL` участникам `to`: основной компонент (или все исключения, если его нет) со `STATUS:CANCELLED`;
/// `bump` — `SEQUENCE` на единицу больше (удаление объекта организатором).
pub fn cancel(ical: &ICalendar, to: &[String], bump: bool, now: OffsetDateTime) -> ICalendar {
    let mut out = envelope(ical, ICalendarMethod::Cancel);
    let has_master = sched(ical).any(is_master);
    let sequence = meeting(ical).map_or(0, |m| m.sequence) + i64::from(bump);
    for c in sched(ical).filter(|c| !has_master || is_master(c)) {
        let mut comp = skeleton(c, now, sequence);
        for addr in to {
            let e = c
                .properties(&ICalendarProperty::Attendee)
                .find(|e| entry_address(e).as_deref() == Some(addr.as_str()))
                .cloned()
                .map_or_else(
                    || {
                        ICalendarEntry::new(ICalendarProperty::Attendee)
                            .with_value(format!("mailto:{addr}"))
                    },
                    clean,
                );
            comp.entries.push(e);
        }
        comp.add_property(ICalendarProperty::Status, ICalendarStatus::Cancelled);
        push(&mut out, ical, comp, false);
    }
    out
}

/// `REPLY` участника `attendee` организатору: компоненты, где он участник, с его `ATTENDEE` и `PARTSTAT`;
/// `partstat` — статус вместо записанного в объекте (`DECLINED` при удалении). `None` — участника нет.
pub fn reply(
    ical: &ICalendar,
    attendee: &str,
    partstat: Option<ICalendarParticipationStatus>,
    now: OffsetDateTime,
) -> Option<ICalendar> {
    let mut out = envelope(ical, ICalendarMethod::Reply);
    let mut any = false;
    for c in sched(ical) {
        let Some(me) = c
            .properties(&ICalendarProperty::Attendee)
            .find(|e| entry_address(e).as_deref() == Some(attendee))
        else {
            continue;
        };
        let mut comp = skeleton(c, now, sequence_of(c));
        let mut me = clean(me.clone());
        me.params.retain(|p| p.name != ICalendarParameterName::Rsvp);
        if let Some(ps) = &partstat {
            me.params
                .retain(|p| p.name != ICalendarParameterName::Partstat);
            me.params.push(ICalendarParameter::partstat(ps.clone()));
        }
        comp.entries.push(me);
        push(&mut out, ical, comp, false);
        any = true;
    }
    any.then_some(out)
}

// ---------- Сравнение ----------

pub(crate) fn hash_hex(lines: &BTreeSet<String>) -> String {
    // Хеш стандартной библиотеки с нулевым ключом: значение постоянно в пределах версии компилятора; смена версии
    // даёт новый тег при следующей записи объекта — как после существенной правки.
    let mut h = std::hash::DefaultHasher::new();
    lines.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// Свойства, не влияющие на `Schedule-Tag`: отметки времени записи и личные данные копии.
fn insignificant(p: &ICalendarProperty) -> bool {
    matches!(
        p,
        ICalendarProperty::Dtstamp
            | ICalendarProperty::LastModified
            | ICalendarProperty::Created
            | ICalendarProperty::Transp
    )
}

/// `Schedule-Tag` объекта планирования (RFC 6638 §3.2.10): хеш содержимого без `PARTSTAT`, `RSVP`, служебных
/// параметров планирования, отметок времени записи, `TRANSP` и напоминаний — не меняется от ответа участника,
/// меняется от правок организатора. `None` — не объект планирования.
pub fn schedule_tag(ical: &ICalendar) -> Option<String> {
    if !meeting(ical)?.is_scheduling() {
        return None;
    }
    let mut lines = BTreeSet::new();
    for (i, c) in sched(ical).enumerate() {
        let rid = recurrence_id(c).unwrap_or_else(|| format!("#{i}"));
        for e in c.entries.iter().filter(|e| !insignificant(&e.name)) {
            let e = match e.name {
                ICalendarProperty::Attendee | ICalendarProperty::Organizer => {
                    let mut e = clean(e.clone());
                    e.params.retain(|p| {
                        !matches!(
                            p.name,
                            ICalendarParameterName::Partstat | ICalendarParameterName::Rsvp
                        )
                    });
                    line(&e)
                }
                _ => line(e),
            };
            lines.insert(format!("{rid}\u{0}{e}"));
        }
    }
    Some(hash_hex(&lines))
}

fn timing(ical: &ICalendar) -> BTreeSet<String> {
    let mut lines = BTreeSet::new();
    for c in sched(ical) {
        let rid = recurrence_id(c).unwrap_or_default();
        for e in c.entries.iter().filter(|e| {
            matches!(
                e.name,
                ICalendarProperty::Dtstart
                    | ICalendarProperty::Dtend
                    | ICalendarProperty::Due
                    | ICalendarProperty::Duration
                    | ICalendarProperty::Rrule
                    | ICalendarProperty::Rdate
                    | ICalendarProperty::Exdate
            )
        }) {
            lines.insert(format!("{rid}\u{0}{}", line(e)));
        }
    }
    lines
}

/// Время объекта изменилось (перенос): отличаются начало, конец, длительность или повторы какого-либо компонента.
pub fn rescheduled(old: &ICalendar, new: &ICalendar) -> bool {
    timing(old) != timing(new)
}

/// Статусы участия адреса `addr` по компонентам: (`RECURRENCE-ID`, `PARTSTAT`).
fn partstats(ical: &ICalendar, addr: &str) -> BTreeSet<(Option<String>, String)> {
    sched(ical)
        .filter_map(|c| {
            let e = c
                .properties(&ICalendarProperty::Attendee)
                .find(|e| entry_address(e).as_deref() == Some(addr))?;
            Some((recurrence_id(c), partstat_of(e)))
        })
        .collect()
}

/// Записывает `SCHEDULE-STATUS` (RFC 6638 §7.3) участникам и организатору по адресам `statuses`.
pub fn set_schedule_status(ical: &mut ICalendar, statuses: &[(String, String)]) {
    for c in ical.components.iter_mut().filter(|c| is_sched(c)) {
        for e in c.entries.iter_mut().filter(|e| {
            matches!(
                e.name,
                ICalendarProperty::Attendee | ICalendarProperty::Organizer
            )
        }) {
            let Some(addr) = entry_address(e) else {
                continue;
            };
            if let Some((_, st)) = statuses.iter().find(|(a, _)| *a == addr) {
                e.params
                    .retain(|p| p.name != ICalendarParameterName::ScheduleStatus);
                e.params
                    .push(ICalendarParameter::schedule_status(st.clone()));
            }
        }
    }
}

// ---------- План рассылки ----------

/// Сообщение iTIP к отправке.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// Метод.
    pub method: ItipMethod,
    /// Получатели ([`address`]).
    pub to: Vec<String>,
    /// Объект с `METHOD`.
    pub ical: ICalendar,
}

impl Message {
    /// Тело сообщения — текст iCalendar.
    pub fn ics(&self) -> String {
        self.ical.to_string()
    }
}

/// Роль владельца объекта в планировании.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    /// Не объект планирования или владелец в нём не участвует.
    None,
    /// Организатор: адрес из `ORGANIZER`.
    Organizer(String),
    /// Участник: его адрес из `ATTENDEE`.
    Attendee(String),
}

/// План рассылки после изменения объекта его владельцем.
#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    /// Роль владельца.
    pub role: Role,
    /// Сообщения в порядке отправки.
    pub messages: Vec<Message>,
}

/// Сообщения iTIP, следующие из изменения объекта владельцем с адресами `me` ([`address`]): `old` — прежний объект
/// (`None` — создание), `new` — новый (`None` — удаление).
///
/// Организатор: создание — `REQUEST` всем участникам; правка — `REQUEST` добавленным, а при существенном изменении
/// ([`schedule_tag`]) — всем, `CANCEL` удалённым; `STATUS:CANCELLED` или удаление — `CANCEL` всем. Участник:
/// изменение своего `PARTSTAT` — `REPLY` организатору, удаление — `REPLY` с `DECLINED`. Участники и организатор с
/// `SCHEDULE-AGENT` не `SERVER` пропускаются.
pub fn plan(
    old: Option<&ICalendar>,
    new: Option<&ICalendar>,
    me: &[String],
    now: OffsetDateTime,
) -> Plan {
    let none = Plan {
        role: Role::None,
        messages: Vec::new(),
    };
    let (om, nm) = (old.and_then(meeting), new.and_then(meeting));
    let Some(basis) = nm.as_ref().or(om.as_ref()).filter(|m| m.is_scheduling()) else {
        return none;
    };
    let Some(organizer) = basis.organizer.clone() else {
        return none;
    };
    if me.contains(&organizer) {
        let messages = organizer_plan(old.zip(om.as_ref()), new.zip(nm.as_ref()), me, now);
        return Plan {
            role: Role::Organizer(organizer),
            messages,
        };
    }
    let Some(mine) = basis
        .attendees
        .iter()
        .find(|a| me.contains(&a.address))
        .map(|a| a.address.clone())
    else {
        return none;
    };
    let mut messages = Vec::new();
    if basis.organizer_server {
        let before = old.map(|o| partstats(o, &mine));
        let ical = match (old, new) {
            (Some(o), None) => {
                let declined = before
                    .as_ref()
                    .is_some_and(|b| b.iter().all(|(_, p)| p == "DECLINED"));
                (!declined && !om.as_ref().is_some_and(|m| m.cancelled))
                    .then(|| reply(o, &mine, Some(ICalendarParticipationStatus::Declined), now))
                    .flatten()
            }
            (_, Some(n)) => {
                let after = partstats(n, &mine);
                let changed = match &before {
                    Some(b) => *b != after,
                    None => after.iter().any(|(_, p)| p != NEEDS_ACTION),
                };
                changed.then(|| reply(n, &mine, None, now)).flatten()
            }
            (None, None) => None,
        };
        if let Some(ical) = ical {
            messages.push(Message {
                method: ItipMethod::Reply,
                to: vec![organizer],
                ical,
            });
        }
    }
    Plan {
        role: Role::Attendee(mine),
        messages,
    }
}

/// Адреса участников, которых уведомляет сервер: без самого организатора.
fn targets(m: &Meeting, me: &[String]) -> Vec<String> {
    m.attendees
        .iter()
        .filter(|a| {
            a.server && !me.contains(&a.address) && Some(&a.address) != m.organizer.as_ref()
        })
        .map(|a| a.address.clone())
        .collect()
}

fn organizer_plan(
    old: Option<(&ICalendar, &Meeting)>,
    new: Option<(&ICalendar, &Meeting)>,
    me: &[String],
    now: OffsetDateTime,
) -> Vec<Message> {
    let cancel_msg = |ical: &ICalendar, to: Vec<String>, bump: bool| Message {
        method: ItipMethod::Cancel,
        ical: cancel(ical, &to, bump, now),
        to,
    };
    let mut out = Vec::new();
    let old_targets = old.map(|(_, m)| targets(m, me)).unwrap_or_default();
    let Some((n, nm)) = new else {
        if let Some((o, om)) = old
            && !om.cancelled
            && !old_targets.is_empty()
        {
            out.push(cancel_msg(o, old_targets, true));
        }
        return out;
    };
    let new_targets = targets(nm, me);
    if let Some((o, _)) = old {
        let removed: Vec<String> = old_targets
            .iter()
            .filter(|a| nm.attendee(a).is_none())
            .cloned()
            .collect();
        if !removed.is_empty() {
            out.push(cancel_msg(o, removed, true));
        }
    }
    if nm.cancelled {
        if !old.is_some_and(|(_, m)| m.cancelled) && !new_targets.is_empty() {
            out.push(cancel_msg(n, new_targets, false));
        }
        return out;
    }
    let significant = old.is_none_or(|(o, om)| om.cancelled || schedule_tag(o) != schedule_tag(n));
    for to in new_targets {
        let known = old.is_some_and(|(_, m)| m.attendee(&to).is_some());
        if !significant && known {
            continue;
        }
        if let Some(ical) = request(n, &to, now) {
            out.push(Message {
                method: ItipMethod::Request,
                to: vec![to],
                ical,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: OffsetDateTime = OffsetDateTime::UNIX_EPOCH;

    fn ical(extra: &str) -> ICalendar {
        ICalendar::parse(format!(
            "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//RU\r\nBEGIN:VEVENT\r\nUID:m-1\r\n\
             DTSTAMP:20260101T000000Z\r\nDTSTART:20260105T100000Z\r\nDTEND:20260105T110000Z\r\n\
             SUMMARY:Планёрка\r\nORGANIZER:mailto:Ivan@example.org\r\n{extra}\
             BEGIN:VALARM\r\nACTION:DISPLAY\r\nTRIGGER:-PT15M\r\nDESCRIPTION:x\r\nEND:VALARM\r\n\
             END:VEVENT\r\nEND:VCALENDAR\r\n"
        ))
        .expect("ical")
    }

    const PETR: &str = "ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:petr@example.org\r\n";
    const ANNA: &str = "ATTENDEE;PARTSTAT=NEEDS-ACTION:mailto:anna@example.net\r\n";

    fn me(a: &str) -> Vec<String> {
        vec![a.to_owned()]
    }

    fn summary(p: &Plan) -> Vec<(ItipMethod, Vec<String>)> {
        p.messages
            .iter()
            .map(|m| (m.method, m.to.clone()))
            .collect()
    }

    #[test]
    fn meeting_and_address() {
        assert_eq!(address(" MAILTO:Ivan@Example.org "), "ivan@example.org");
        let m = meeting(&ical(&format!("{PETR}{ANNA}"))).expect("meeting");
        assert_eq!(m.organizer.as_deref(), Some("ivan@example.org"));
        assert_eq!(m.attendees.len(), 2);
        assert!(m.is_scheduling() && m.event && !m.cancelled);
        assert!(!meeting(&ical("")).expect("m").is_scheduling());
        assert_eq!(schedule_tag(&ical("")), None);
    }

    #[test]
    fn organizer_creates_updates_cancels() {
        let v1 = ical(&format!("{PETR}{ANNA}"));
        let p = plan(None, Some(&v1), &me("ivan@example.org"), NOW);
        assert_eq!(p.role, Role::Organizer("ivan@example.org".into()));
        assert_eq!(
            summary(&p),
            vec![
                (ItipMethod::Request, me("petr@example.org")),
                (ItipMethod::Request, me("anna@example.net")),
            ]
        );
        let text = p.messages[0].ics();
        assert!(
            text.contains("METHOD:REQUEST") && !text.contains("VALARM"),
            "{text}"
        );
        assert_eq!(parse(&text).expect("itip").method, ItipMethod::Request);

        // Ответ участника (только PARTSTAT) — ничего не рассылается, тег прежний.
        let accepted = ical(&format!(
            "ATTENDEE;PARTSTAT=ACCEPTED;SCHEDULE-STATUS=2.0:mailto:petr@example.org\r\n{ANNA}"
        ));
        assert_eq!(schedule_tag(&v1), schedule_tag(&accepted));
        assert!(
            plan(Some(&v1), Some(&accepted), &me("ivan@example.org"), NOW)
                .messages
                .is_empty()
        );

        // Удалён участник: ему CANCEL, остальным REQUEST.
        let without = ical(PETR);
        let p = plan(Some(&v1), Some(&without), &me("ivan@example.org"), NOW);
        assert_eq!(
            summary(&p),
            vec![
                (ItipMethod::Cancel, me("anna@example.net")),
                (ItipMethod::Request, me("petr@example.org")),
            ]
        );
        assert!(p.messages[0].ics().contains("STATUS:CANCELLED"));

        // Удаление объекта — CANCEL всем с большим SEQUENCE.
        let p = plan(Some(&v1), None, &me("ivan@example.org"), NOW);
        assert_eq!(p.messages.len(), 1);
        assert_eq!(p.messages[0].to.len(), 2);
        assert!(p.messages[0].ics().contains("SEQUENCE:1"));
    }

    #[test]
    fn schedule_agent_client_skipped() {
        let v = ical("ATTENDEE;SCHEDULE-AGENT=CLIENT:mailto:petr@example.org\r\n");
        let p = plan(None, Some(&v), &me("ivan@example.org"), NOW);
        assert!(p.messages.is_empty());
        assert!(schedule_tag(&v).is_some());
    }

    #[test]
    fn attendee_replies() {
        let v1 = ical(PETR);
        let acc = ical("ATTENDEE;PARTSTAT=ACCEPTED;RSVP=TRUE:mailto:petr@example.org\r\n");
        let who = me("petr@example.org");
        assert!(plan(None, Some(&v1), &who, NOW).messages.is_empty());
        let p = plan(Some(&v1), Some(&acc), &who, NOW);
        assert_eq!(p.role, Role::Attendee("petr@example.org".into()));
        assert_eq!(
            summary(&p),
            vec![(ItipMethod::Reply, me("ivan@example.org"))]
        );
        let text = p.messages[0].ics();
        assert!(
            text.contains("PARTSTAT=ACCEPTED") && !text.contains("RSVP"),
            "{text}"
        );
        let p = plan(Some(&acc), None, &who, NOW);
        assert!(p.messages[0].ics().contains("PARTSTAT=DECLINED"));
        assert!(!rescheduled(&v1, &acc));
        let moved = ICalendar::parse(v1.to_string().replace("T100000Z", "T120000Z")).expect("i");
        assert!(rescheduled(&v1, &moved));
        assert_ne!(schedule_tag(&v1), schedule_tag(&moved));
    }

    #[test]
    fn status_and_errors() {
        let mut v = ical(PETR);
        set_schedule_status(&mut v, &[("petr@example.org".into(), "1.2".into())]);
        assert!(v.to_string().contains("SCHEDULE-STATUS=1.2"), "{v}");
        assert_eq!(parse("x").err(), Some(ItipError::Calendar));
        assert_eq!(
            parse(&ical(PETR).to_string()).err(),
            Some(ItipError::Method)
        );
    }
}
