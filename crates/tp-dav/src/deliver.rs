//! Приём iTIP учётной записью: реализация [`ItipDelivery`] поверх [`DavStore`] (RFC 5546, RFC 6638 §3.2).
//!
//! - `REQUEST` — событие (VEVENT) или задача (VTODO) в календаре по умолчанию получателя для этого компонента (или
//!   обновление уже принятого): свой `PARTSTAT` — `NEEDS-ACTION` у нового и перенесённого, прежний — при правке без
//!   переноса; сообщение только с исключениями (`RECURRENCE-ID`) дополняет ряд;
//! - `REPLY` — `PARTSTAT` участника в событии организатора (`SCHEDULE-STATUS=2.0`), `Schedule-Tag` прежний; ответ
//!   на вхождение ряда, которого нет отдельным компонентом, — новое переопределение вхождения;
//! - `CANCEL` — `STATUS:CANCELLED` всему событию, отдельному вхождению — `EXDATE` или статус исключения;
//! - `ADD` — новые вхождения ряда (`RDATE` и переопределение); ряда нет — во входящие;
//! - `REFRESH`, `COUNTER`, `DECLINECOUNTER` — во входящие планирования как есть (разбирает клиент); `PUBLISH` не
//!   применяется.
//!
//! Применённое сообщение во входящих CalDAV не остаётся; при `dav.schedule_keep_processed` оно хранится для ленты
//! уведомлений JMAP с именем [`PROCESSED_PREFIX`] и удаляется по сроку ([`ItipInbox::purge_processed`]). Защита:
//! `REQUEST`, `ADD` и `CANCEL` — только от организатора сообщения и события с тем же UID, `REPLY` — только от адреса
//! участника события и только организатору; меньший `SEQUENCE` — устаревшее сообщение. Отказ — `Ok(Ignored)` с причиной без содержимого.
//! Повторная доставка ничего не меняет. Запись — с условием по ETag: одновременное изменение — повтор
//! (`dav.schedule_write_attempts`), затем `Unavailable`.
//!
//! Приглашение ресурсу (переговорной) при [`ItipInbox::with_booking`] — решение по правилам бронирования и ответ
//! организатору ([`crate::booking`], DAV-19).

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarEntry, ICalendarParameter, ICalendarParameterName,
    ICalendarParticipationStatus, ICalendarProperty, ICalendarStatus,
};
use time::OffsetDateTime;
use tp_contracts::dav::{
    Collection, CollectionKind, Component, DavObject, DavStore, Precondition, PutOutcome,
    default_of, valid_name,
};
use tp_contracts::meta::AccountScope;
use tp_contracts::schedule::{
    InboxMessage, ItipDelivery, ItipMessage, ItipMethod, ItipOutcome, ItipOutgoing, ItipSource,
};
use tp_contracts::sharing::BookingDecision;
use tp_contracts::{Error, Result};

use crate::booking::{self, Booking, BookingDeps};
use crate::config::DavConfig;
use crate::index;
use crate::itip::{self, Itip};
use crate::props;

/// Приём iTIP в календари учётных записей.
#[derive(Clone)]
pub struct ItipInbox {
    store: Arc<dyn DavStore>,
    cfg: DavConfig,
    /// Бронирование ресурсов; `None` — приглашения ресурсам принимаются как обычные.
    booking: Option<Arc<BookingDeps>>,
}

impl std::fmt::Debug for ItipInbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ItipInbox").finish_non_exhaustive()
    }
}

impl ItipInbox {
    /// Приём в хранилище `store` с настройками секции `dav`.
    pub fn new(store: Arc<dyn DavStore>, cfg: DavConfig) -> Self {
        Self {
            store,
            cfg,
            booking: None,
        }
    }

    /// С бронированием ресурсов по правилам ([`crate::booking`], DAV-19): приглашение ресурсу принимается,
    /// отклоняется или остаётся под вопросом, организатору уходит ответ.
    pub fn with_booking(mut self, deps: BookingDeps) -> Self {
        self.booking = Some(Arc::new(deps));
        self
    }

    /// Шаг очистки роли maintenance (DAV-32; подключение — APP-53): удаляет из входящих планирования учётной записи
    /// `scope` применённые сервером сообщения ([`processed`]), полученные раньше `now` минус
    /// `dav.schedule_processed_ttl`. Необработанные не трогает. Возвращает число удалённых.
    pub async fn purge_processed(&self, scope: AccountScope, now: OffsetDateTime) -> Result<usize> {
        let ttl = time::Duration::try_from(self.cfg.schedule_processed_ttl)
            .unwrap_or(time::Duration::MAX);
        let Some(before) = now.checked_sub(ttl) else {
            return Ok(0);
        };
        let list = self.store.inbox_list(scope).await?;
        let mut removed = 0;
        for i in list
            .items
            .iter()
            .filter(|i| processed(&i.name) && i.received_at < before)
        {
            match self.store.inbox_delete(scope, &i.name).await {
                Ok(_) => removed += 1,
                Err(Error::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(removed)
    }
}

/// Префикс имени применённого сервером сообщения во входящих планирования (DAV-32): хранится при
/// `dav.schedule_keep_processed` для ленты уведомлений JMAP, в CalDAV не виден.
pub const PROCESSED_PREFIX: &str = "p-";

/// Сообщение входящих с именем `name` применено сервером ([`PROCESSED_PREFIX`]).
pub fn processed(name: &str) -> bool {
    name.starts_with(PROCESSED_PREFIX)
}

/// Календарь по умолчанию для приглашений компонента `component` (VEVENT — `schedule-default-calendar-URL`, VTODO —
/// список задач) — правило контракта [`tp_contracts::dav::default_of`] среди календарей с этим компонентом
/// (назначенный `Collection::is_default`, CTR-56); у учётной записи без календарей создаются календари по умолчанию.
pub(crate) async fn default_calendar(
    store: &dyn DavStore,
    scope: AccountScope,
    cfg: &DavConfig,
    component: Component,
) -> Result<Option<Collection>> {
    props::defaults(store, scope, cfg, CollectionKind::Calendar).await?;
    let list = store
        .list_collections(scope, CollectionKind::Calendar)
        .await?;
    let suitable: Vec<Collection> = list
        .into_iter()
        .filter(|c| c.components.contains(&component))
        .collect();
    Ok(default_of(&suitable).cloned())
}

/// Объект с UID `uid` в календарях учётной записи.
pub(crate) async fn find_uid(
    store: &dyn DavStore,
    scope: AccountScope,
    uid: &str,
) -> Result<Option<DavObject>> {
    for c in store
        .list_collections(scope, CollectionKind::Calendar)
        .await?
    {
        if let Some(o) = store.object_by_uid(scope, c.id, uid).await? {
            return Ok(Some(o));
        }
    }
    Ok(None)
}

/// Объект `existing` можно заменить приглашением `it` (`REQUEST`): у него тот же организатор, `SEQUENCE` приглашения
/// не меньше. Возвращает разобранный объект; иначе — причина отказа. Проверка — до любого изменения календаря
/// получателя, в том числе до снятия прежней брони ресурса ([`crate::booking`], DAV-40).
pub(crate) fn replaceable(
    existing: &DavObject,
    it: &Itip,
) -> std::result::Result<ICalendar, &'static str> {
    let Some((ical, m)) = ICalendar::parse(&existing.body)
        .ok()
        .and_then(|i| itip::meeting(&i).map(|m| (i, m)))
    else {
        return Err("чужой UID организатора");
    };
    if m.organizer != it.meeting.organizer {
        return Err("чужой UID организатора");
    }
    if it.meeting.sequence < m.sequence {
        return Err("устаревший SEQUENCE");
    }
    Ok(ical)
}

fn hash(parts: &[&str]) -> String {
    itip::hash_hex(
        &parts
            .iter()
            .map(|p| (*p).to_owned())
            .collect::<BTreeSet<_>>(),
    )
}

/// Шаг применения: итог, сообщение оставлено во входящих для клиента или повтор после одновременного изменения.
enum Step {
    Done(ItipOutcome),
    /// Применено: сообщение — во входящих необработанным (разбирает клиент).
    Inboxed,
    Retry,
}

fn ignored(reason: &str) -> Result<Step> {
    Ok(Step::Done(ItipOutcome::Ignored(reason.to_owned())))
}

const APPLIED: Result<Step> = Ok(Step::Done(ItipOutcome::Applied));

fn set_status(c: &mut ICalendarComponent, status: ICalendarStatus) {
    c.entries.retain(|e| e.name != ICalendarProperty::Status);
    c.add_property(ICalendarProperty::Status, status);
}

fn set_sequence(c: &mut ICalendarComponent, seq: i64) {
    c.entries.retain(|e| e.name != ICalendarProperty::Sequence);
    c.add_sequence(seq);
}

fn needs_action() -> ICalendarParameter {
    ICalendarParameter::partstat(ICalendarParticipationStatus::NeedsAction)
}

/// Индексы компонентов планирования объекта.
fn sched_ids(ical: &ICalendar) -> Vec<usize> {
    (0..ical.components.len())
        .filter(|i| itip::is_sched(&ical.components[*i]))
        .collect()
}

/// Индекс компонента планирования с `RECURRENCE-ID` `rid`.
fn by_rid(ical: &ICalendar, rid: Option<&str>) -> Option<usize> {
    sched_ids(ical)
        .into_iter()
        .find(|i| itip::recurrence_id(&ical.components[*i]).as_deref() == rid)
}

/// Переопределение вхождения ряда `ical` для ответа `rc` (компонент с `RECURRENCE-ID`) участника `attendee`: копия
/// основного компонента без повторов с `RECURRENCE-ID` и временем вхождения из ответа (`DTSTART`; конец — `DTEND`
/// или `DURATION` ответа, иначе `DURATION` ряда). Нет ряда, участника в нём или времени — `None`.
fn instance_of(
    ical: &ICalendar,
    rc: &ICalendarComponent,
    attendee: &str,
) -> Option<ICalendarComponent> {
    let master = &ical.components[by_rid(ical, None)?];
    itip::attendee_entry(master, attendee)?;
    let rid = rc.property(&ICalendarProperty::RecurrenceId)?;
    let start = rc.property(&ICalendarProperty::Dtstart)?;
    let end = rc
        .property(&ICalendarProperty::Dtend)
        .or_else(|| rc.property(&ICalendarProperty::Duration))
        .or_else(|| master.property(&ICalendarProperty::Duration))?
        .clone();
    let mut c = master.clone();
    c.entries.retain(|e| {
        !matches!(
            e.name,
            ICalendarProperty::Rrule
                | ICalendarProperty::Rdate
                | ICalendarProperty::Exdate
                | ICalendarProperty::Dtstart
                | ICalendarProperty::Dtend
                | ICalendarProperty::Duration
                | ICalendarProperty::RecurrenceId
        )
    });
    c.entries.extend([rid.clone(), start.clone(), end]);
    Some(c)
}

impl ItipInbox {
    /// Записывает объект; `existing` — заменяемый, иначе создаётся в календаре по умолчанию с именем по `attempt`.
    async fn write(
        &self,
        scope: AccountScope,
        existing: Option<&DavObject>,
        ical: &ICalendar,
        attempt: usize,
    ) -> Result<Step> {
        let body = ical.to_string();
        if existing.is_some_and(|o| o.body == body) {
            return APPLIED;
        }
        let limits = self.store.limits();
        if body.len() > self.cfg.max_object_size.min(limits.max_object_size) {
            return ignored("объект больше допустимого размера");
        }
        let Ok(idx) = index::for_write(ical, &self.cfg.index_options(OffsetDateTime::now_utc()))
        else {
            return ignored("недопустимый объект календаря");
        };
        let (coll, name, pre) = match existing {
            Some(o) => (
                o.collection,
                o.name.clone(),
                Precondition::IfMatch(o.etag.clone()),
            ),
            None => {
                let calendar =
                    default_calendar(self.store.as_ref(), scope, &self.cfg, idx.component).await?;
                let Some(c) = calendar else {
                    return ignored("нет календаря для приглашения");
                };
                let plain = format!("{}.ics", idx.uid);
                let name = if attempt == 0 && valid_name(&plain) && plain.len() <= 128 {
                    plain
                } else {
                    format!("{}-{attempt}.ics", hash(&[&idx.uid]))
                };
                (c.id, name, Precondition::IfNoneMatch)
            }
        };
        match self
            .store
            .put_object(scope, coll, &name, body, idx, pre)
            .await
        {
            Ok(PutOutcome::Stored(_)) => APPLIED,
            Ok(PutOutcome::PreconditionFailed | PutOutcome::UidConflict { .. }) => Ok(Step::Retry),
            Err(Error::QuotaExceeded) => ignored("календарь переполнен"),
            Err(Error::Invalid(_)) => ignored("недопустимый объект календаря"),
            Err(e) => Err(e),
        }
    }

    /// Кладёт сообщение во входящие планирования (имя — по содержимому: повторная доставка перезаписывает);
    /// `processed` — применённое сервером, с префиксом [`PROCESSED_PREFIX`].
    async fn to_inbox(
        &self,
        msg: &ItipMessage,
        it: &Itip,
        from: &str,
        processed: bool,
    ) -> Result<Step> {
        let rids: Vec<String> = it
            .ical
            .components
            .iter()
            .filter(|c| itip::is_sched(c))
            .map(|c| itip::recurrence_id(c).unwrap_or_default())
            .collect();
        let seq = it.meeting.sequence.to_string();
        let mut parts = vec![it.method.as_str(), it.meeting.uid.as_str(), &seq, from];
        parts.extend(rids.iter().map(String::as_str));
        let prefix = if processed { PROCESSED_PREFIX } else { "" };
        let name = format!("{prefix}{}.ics", hash(&parts));
        let message = InboxMessage {
            method: it.method,
            uid: it.meeting.uid.clone(),
            from: from.to_owned(),
            body: msg.ics.clone(),
        };
        match self.store.inbox_put(msg.recipient, &name, message).await {
            Ok(_) if processed => APPLIED,
            Ok(_) => Ok(Step::Inboxed),
            Err(Error::QuotaExceeded) => ignored("входящие планирования переполнены"),
            Err(Error::Invalid(_)) => ignored("недопустимое сообщение"),
            Err(e) => Err(e),
        }
    }

    /// `forced` — свой `PARTSTAT` вместо прежнего или `NEEDS-ACTION` (решение ресурса).
    async fn request(
        &self,
        scope: AccountScope,
        it: &Itip,
        to: &str,
        existing: Option<&DavObject>,
        attempt: usize,
        forced: Option<&ICalendarParticipationStatus>,
    ) -> Result<Step> {
        let old = match existing.map(|o| replaceable(o, it)).transpose() {
            Ok(old) => old,
            Err(reason) => return ignored(reason),
        };
        let mut msg = it.ical.clone();
        if let Some(root) = msg.components.first_mut() {
            root.entries.retain(|e| e.name != ICalendarProperty::Method);
        }
        let partial = by_rid(&msg, None).is_none();
        let mut new = match (&old, partial) {
            (Some(old), true) => {
                // Только исключения: заменяют одноимённые в принятом ряду, остальное сохраняется.
                let mut merged = old.clone();
                let replaced: Vec<u32> = sched_ids(&msg)
                    .into_iter()
                    .filter_map(|i| {
                        by_rid(&merged, itip::recurrence_id(&msg.components[i]).as_deref())
                    })
                    .filter_map(|i| u32::try_from(i).ok())
                    .collect();
                merged.remove_component_ids(&replaced);
                for i in sched_ids(&msg) {
                    itip::push(&mut merged, &msg, msg.components[i].clone(), true);
                }
                merged
            }
            _ => msg,
        };
        let moved = old.as_ref().is_none_or(|o| itip::rescheduled(o, &new));
        for i in sched_ids(&new) {
            let rid = itip::recurrence_id(&new.components[i]);
            let kept = if moved {
                None
            } else {
                old.as_ref().and_then(|o| {
                    let c = by_rid(o, rid.as_deref()).or_else(|| by_rid(o, None))?;
                    let e = itip::attendee_entry(&o.components[c], to)?;
                    itip::param_of(e, &ICalendarParameterName::Partstat)
                })
            };
            if let Some(e) = itip::attendee_entry_mut(&mut new.components[i], to) {
                let value = forced
                    .map(|ps| ICalendarParameter::partstat(ps.clone()))
                    .or(kept)
                    .unwrap_or_else(needs_action);
                itip::replace_param(e, &ICalendarParameterName::Partstat, Some(value));
            }
        }
        self.write(scope, existing, &new, attempt).await
    }

    async fn reply(
        &self,
        msg: &ItipMessage,
        it: &Itip,
        from: &str,
        to: &str,
        existing: Option<&DavObject>,
    ) -> Result<Step> {
        let Some(obj) = existing else {
            return ignored("событие не найдено");
        };
        let Some((old, m)) = ICalendar::parse(&obj.body)
            .ok()
            .and_then(|i| itip::meeting(&i).map(|m| (i, m)))
        else {
            return ignored("событие не найдено");
        };
        if m.organizer.as_deref() != Some(to) {
            return ignored("получатель не организатор события");
        }
        let alone = matches!(it.meeting.attendees.as_slice(), [a] if a.address == from);
        if !alone || m.attendee(from).is_none() {
            return ignored("отправитель не участник события");
        }
        if it.meeting.sequence < m.sequence {
            return ignored("устаревший SEQUENCE");
        }
        let mut new = old;
        let mut unmatched = false;
        for i in sched_ids(&it.ical) {
            let rc = &it.ical.components[i];
            let Some(answer) = itip::attendee_entry(rc, from) else {
                continue;
            };
            let rid = itip::recurrence_id(rc);
            let t = match by_rid(&new, rid.as_deref()) {
                Some(t) => Some(t),
                // Ответ на вхождение ряда, которого нет отдельным компонентом: переопределение вхождения.
                None if rid.is_some() => instance_of(&new, rc, from).map(|c| {
                    let src = new.clone();
                    let t = new.components.len();
                    itip::push(&mut new, &src, c, true);
                    t
                }),
                None => None,
            };
            let target = t.and_then(|t| itip::attendee_entry_mut(&mut new.components[t], from));
            let Some(e) = target else {
                // Вхождение не построить (нет времени в ответе и в ряде): оставляется клиенту организатора.
                unmatched = true;
                continue;
            };
            let value = itip::param_of(answer, &ICalendarParameterName::Partstat)
                .unwrap_or_else(needs_action);
            itip::replace_param(e, &ICalendarParameterName::Partstat, Some(value));
            itip::replace_param(e, &ICalendarParameterName::Rsvp, None);
            itip::replace_param(
                e,
                &ICalendarParameterName::ScheduleStatus,
                Some(ICalendarParameter::schedule_status("2.0".to_owned())),
            );
        }
        match self.write(msg.recipient, existing, &new, 0).await? {
            Step::Done(ItipOutcome::Applied) if unmatched => {
                self.to_inbox(msg, it, from, false).await
            }
            step => Ok(step),
        }
    }

    /// `ADD` (RFC 5546 §3.2.4): новые вхождения ряда — `RDATE` основного компонента и переопределение вхождения с
    /// содержимым сообщения (свой `PARTSTAT` — `NEEDS-ACTION`). Ряда нет — во входящие клиенту (он запросит
    /// `REFRESH`).
    async fn add(
        &self,
        msg: &ItipMessage,
        it: &Itip,
        from: &str,
        to: &str,
        existing: Option<&DavObject>,
    ) -> Result<Step> {
        let parsed = existing.and_then(|o| {
            ICalendar::parse(&o.body)
                .ok()
                .and_then(|i| itip::meeting(&i).map(|m| (i, m)))
        });
        let Some((old, m)) = parsed else {
            return self.to_inbox(msg, it, from, false).await;
        };
        if m.organizer.as_deref() != Some(from) {
            return ignored("чужой UID организатора");
        }
        if it.meeting.sequence < m.sequence {
            return ignored("устаревший SEQUENCE");
        }
        let Some(master) = by_rid(&old, None) else {
            return self.to_inbox(msg, it, from, false).await;
        };
        let mut new = old;
        for i in sched_ids(&it.ical) {
            let rc = &it.ical.components[i];
            let Some(start) = rc.property(&ICalendarProperty::Dtstart) else {
                continue;
            };
            let mut rdate =
                ICalendarEntry::new(ICalendarProperty::Rdate).with_values(start.values.clone());
            rdate.params.clone_from(&start.params);
            let line = itip::line(&rdate);
            let comp = &mut new.components[master];
            if !comp.entries.iter().any(|e| itip::line(e) == line) {
                comp.entries.push(rdate);
            }
            let mut rid = ICalendarEntry::new(ICalendarProperty::RecurrenceId)
                .with_values(start.values.clone());
            rid.params.clone_from(&start.params);
            if by_rid(&new, Some(itip::line(&rid).as_str())).is_some() {
                continue;
            }
            let mut c = rc.clone();
            c.entries
                .retain(|e| e.name != ICalendarProperty::RecurrenceId);
            c.entries.push(rid);
            if let Some(e) = itip::attendee_entry_mut(&mut c, to) {
                itip::replace_param(e, &ICalendarParameterName::Partstat, Some(needs_action()));
            }
            itip::push(&mut new, &it.ical, c, true);
        }
        self.write(msg.recipient, existing, &new, 0).await
    }

    async fn cancel(
        &self,
        scope: AccountScope,
        it: &Itip,
        from: &str,
        existing: Option<&DavObject>,
    ) -> Result<Step> {
        let Some(obj) = existing else {
            return ignored("событие не найдено");
        };
        let Some((old, m)) = ICalendar::parse(&obj.body)
            .ok()
            .and_then(|i| itip::meeting(&i).map(|m| (i, m)))
        else {
            return ignored("чужой UID организатора");
        };
        if m.organizer.as_deref() != Some(from) {
            return ignored("чужой UID организатора");
        }
        if it.meeting.sequence < m.sequence {
            return ignored("устаревший SEQUENCE");
        }
        let mut new = old;
        if by_rid(&it.ical, None).is_some() {
            for i in sched_ids(&new) {
                set_status(&mut new.components[i], ICalendarStatus::Cancelled);
                set_sequence(&mut new.components[i], it.meeting.sequence);
            }
        } else {
            for i in sched_ids(&it.ical) {
                let rc = &it.ical.components[i];
                let rid = itip::recurrence_id(rc);
                if let Some(t) = by_rid(&new, rid.as_deref()) {
                    set_status(&mut new.components[t], ICalendarStatus::Cancelled);
                    continue;
                }
                let (Some(master), Some(rid)) = (
                    by_rid(&new, None),
                    rc.property(&ICalendarProperty::RecurrenceId),
                ) else {
                    continue;
                };
                let mut exdate =
                    ICalendarEntry::new(ICalendarProperty::Exdate).with_values(rid.values.clone());
                exdate.params = rid
                    .params
                    .iter()
                    .filter(|p| p.name != ICalendarParameterName::Range)
                    .cloned()
                    .collect();
                let line = itip::line(&exdate);
                let comp = &mut new.components[master];
                if !comp.entries.iter().any(|e| itip::line(e) == line) {
                    comp.entries.push(exdate);
                }
            }
        }
        self.write(scope, existing, &new, 0).await
    }

    async fn apply(
        &self,
        msg: &ItipMessage,
        it: &Itip,
        from: &str,
        to: &str,
        attempt: usize,
        forced: Option<&ICalendarParticipationStatus>,
    ) -> Result<Step> {
        let scope = msg.recipient;
        let auto = matches!(
            it.method,
            ItipMethod::Request | ItipMethod::Reply | ItipMethod::Cancel | ItipMethod::Add
        );
        if !auto {
            return self.to_inbox(msg, it, from, false).await;
        }
        let existing = find_uid(self.store.as_ref(), scope, &it.meeting.uid).await?;
        let step = match it.method {
            ItipMethod::Request => {
                self.request(scope, it, to, existing.as_ref(), attempt, forced)
                    .await?
            }
            ItipMethod::Reply => self.reply(msg, it, from, to, existing.as_ref()).await?,
            ItipMethod::Add => self.add(msg, it, from, to, existing.as_ref()).await?,
            _ => self.cancel(scope, it, from, existing.as_ref()).await?,
        };
        if self.cfg.schedule_keep_processed
            && matches!(step, Step::Done(ItipOutcome::Applied))
            && let Step::Done(ItipOutcome::Ignored(reason)) =
                self.to_inbox(msg, it, from, true).await?
        {
            tracing::debug!(%reason, "iTIP: применённое сообщение не сохранено во входящих");
        }
        Ok(step)
    }
}

#[async_trait]
impl ItipDelivery for ItipInbox {
    async fn deliver(&self, msg: &ItipMessage) -> Result<ItipOutcome> {
        let refuse = |reason: &str| {
            tracing::debug!(reason, method = %msg.method, "iTIP: сообщение не применено");
            Ok(ItipOutcome::Ignored(reason.to_owned()))
        };
        if msg.validate().is_err() {
            return refuse("недопустимое сообщение");
        }
        let Ok(it) = itip::parse(&msg.ics) else {
            return refuse("неразборчивый iCalendar");
        };
        if it.method != msg.method {
            return refuse("METHOD не совпадает с заявленным");
        }
        if it.method == ItipMethod::Publish {
            return refuse("метод не поддерживается");
        }
        let (from, to) = (itip::address(&msg.from), itip::address(&msg.to));
        let sender_ok = if it.method.from_organizer() {
            it.meeting.organizer.as_deref() == Some(from.as_str())
        } else {
            it.meeting.attendee(&from).is_some()
        };
        if !sender_ok {
            return refuse("отправитель не организатор и не участник");
        }
        let booked = match (&self.booking, it.method) {
            (Some(b), ItipMethod::Request) => {
                b.decide(self.store.as_ref(), &self.cfg, msg, &it, &to)
                    .await?
            }
            _ => None,
        };
        if let Some(b) = &booked
            && matches!(b.decision, BookingDecision::Decline(_))
        {
            self.answer(msg, &it, &to, b).await;
            return Ok(ItipOutcome::Applied);
        }
        let forced = booked.as_ref().map(Booking::partstat);
        for attempt in 0..self.cfg.schedule_write_attempts.max(1) {
            match self
                .apply(msg, &it, &from, &to, attempt, forced.as_ref())
                .await?
            {
                Step::Done(ItipOutcome::Ignored(reason)) => return refuse(&reason),
                Step::Done(outcome) => {
                    if let Some(b) = &booked {
                        self.answer(msg, &it, &to, b).await;
                    }
                    return Ok(outcome);
                }
                Step::Inboxed => return Ok(ItipOutcome::Applied),
                Step::Retry => {}
            }
        }
        Err(Error::UNAVAILABLE)
    }
}

impl ItipInbox {
    /// Ответ ресурса `to` организатору приглашения `it` по решению `b` ([`crate::booking`]): учётной записи
    /// организации — в календарь, внешнему — письмом iMIP. Сбой ответа — в журнал, решение остаётся.
    async fn answer(&self, msg: &ItipMessage, it: &Itip, to: &str, b: &Booking) {
        let Some(deps) = &self.booking else {
            return;
        };
        let Some(organizer) = it.meeting.organizer.clone() else {
            return;
        };
        let now = OffsetDateTime::now_utc();
        let Some(mut reply) = itip::reply(&it.ical, to, Some(b.partstat()), now) else {
            return;
        };
        if let BookingDecision::Decline(reason) = b.decision {
            for c in reply.components.iter_mut().filter(|c| itip::is_sched(c)) {
                c.add_property(ICalendarProperty::Comment, booking::reason_text(reason));
            }
        }
        let ics = reply.to_string();
        let result = match b.organizer {
            Some(account) => {
                let m = ItipMessage {
                    recipient: AccountScope {
                        tenant: msg.recipient.tenant,
                        account,
                    },
                    to: organizer,
                    from: to.to_owned(),
                    method: ItipMethod::Reply,
                    ics,
                    source: ItipSource::Internal,
                };
                self.deliver(&m).await.map(|_| ())
            }
            None => match &deps.sender {
                Some(sender) => {
                    let (subject, text) = crate::imip::describe(ItipMethod::Reply, &reply);
                    let mail = ItipOutgoing {
                        sender: msg.recipient,
                        from: to.to_owned(),
                        to: vec![organizer],
                        method: ItipMethod::Reply,
                        ics,
                        subject,
                        text,
                    };
                    sender.send(&mail).await.map(|_| ())
                }
                None => Ok(()),
            },
        };
        if let Err(e) = result {
            tracing::warn!(error = %e, "iTIP: ответ ресурса организатору не отправлен");
        }
    }
}
