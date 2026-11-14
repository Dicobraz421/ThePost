//! Бронирование ресурсов — переговорных и оборудования (DAV-19; SRS 4.4; CONTRACTS.md#Sharing).
//!
//! Ресурс — учётная запись вида [`AccountKind::Resource`] с правилами [`BookingPolicy`] ([`ResourceStore`]). Приглашение
//! (`REQUEST`) на адрес ресурса при [`crate::ItipInbox::with_booking`]: сервер разворачивает вхождения встречи (повторы —
//! не больше `dav.max_instances`; правило без конца — [`BookingRequest::unbounded`]), читает занятость ресурса
//! ([`FreeBusy`]) и решает [`BookingPolicy::decide`]:
//!
//! - принято — событие в календаре ресурса с `PARTSTAT=ACCEPTED`, под вопросом (без автоматического принятия) — с
//!   `TENTATIVE` до решения представителя ресурса (DAV-18); отклонено — события в календаре ресурса нет;
//! - организатору — `REPLY` от адреса ресурса с тем же статусом, у отказа — `COMMENT` с причиной: учётной записи
//!   организации — в календарь ([`crate::ItipInbox`]), внешнему — письмом iMIP (`BookingDeps::sender`).
//!
//! Решение принимается для новой встречи и для перенесённой ([`crate::itip::rescheduled`]): прежняя бронь этой встречи
//! снимается до чтения занятости, чтобы не пересекаться сама с собой. Правка без переноса сохраняет прежний статус, ответ
//! не отправляется. Сообщение только с исключениями ряда, правила не заданы, получатель — не ресурс — обычный приём
//! iTIP. Организатор вне организации бронировать не может ([`BookingPolicy::allows`]); группы организатора для
//! [`Bookers::Only`] раскрываются ([`GroupStore::expand`]).

use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::Arc;

use calcard::common::timezone::Tz;
use calcard::icalendar::{
    ICalendar, ICalendarComponentType, ICalendarParticipationStatus, ICalendarProperty,
    ICalendarValue,
};
use time::{OffsetDateTime, PrimitiveDateTime};
use tp_contracts::dav::{DavStore, Precondition};
use tp_contracts::groups::GroupStore;
use tp_contracts::meta::{AccountKind, AccountScope, AccountStore, AddressTarget};
use tp_contracts::schedule::{FreeBusy, ItipMessage, ItipSender, Period};
use tp_contracts::sharing::{
    Bookers, BookingDecision, BookingPeriod, BookingPolicy, BookingRequest, DeclineReason,
    ResourceStore,
};
use tp_contracts::{AccountId, Result};

use crate::config::DavConfig;
use crate::deliver;
use crate::itip::{self, Itip};

/// Зависимости бронирования ресурсов ([`crate::ItipInbox::with_booking`]).
#[derive(Clone)]
pub struct BookingDeps {
    /// Учётные записи: вид получателя, организатор по адресу.
    pub accounts: Arc<dyn AccountStore>,
    /// Правила бронирования ресурсов.
    pub resources: Arc<dyn ResourceStore>,
    /// Занятость ресурса.
    pub freebusy: Arc<dyn FreeBusy>,
    /// Группы рассылки: группы организатора для [`Bookers::Only`].
    pub group_store: Arc<dyn GroupStore>,
    /// Ответ внешнему организатору письмом iMIP; `None` — внешнему ответ не отправляется.
    pub sender: Option<Arc<dyn ItipSender>>,
}

impl std::fmt::Debug for BookingDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BookingDeps")
            .field("imip", &self.sender.is_some())
            .finish_non_exhaustive()
    }
}

/// Решение ресурса по приглашению.
#[derive(Debug, Clone)]
pub(crate) struct Booking {
    pub decision: BookingDecision,
    /// Учётная запись организатора; `None` — внешний.
    pub organizer: Option<AccountId>,
}

impl Booking {
    /// `PARTSTAT` ответа.
    pub(crate) fn partstat(&self) -> ICalendarParticipationStatus {
        match self.decision {
            BookingDecision::Accept => ICalendarParticipationStatus::Accepted,
            BookingDecision::Tentative => ICalendarParticipationStatus::Tentative,
            BookingDecision::Decline(_) => ICalendarParticipationStatus::Declined,
        }
    }
}

/// Причина отказа для организатора (`COMMENT` ответа).
pub(crate) fn reason_text(reason: DeclineReason) -> &'static str {
    match reason {
        DeclineReason::NotAllowed => "Отклонено: организатору не разрешено бронировать ресурс",
        DeclineReason::Invalid => "Отклонено: у встречи нет допустимого времени",
        DeclineReason::TooLong => "Отклонено: встреча длиннее допустимого",
        DeclineReason::TooFar => "Отклонено: бронирование слишком далеко вперёд",
        DeclineReason::OutsideWorkingHours => "Отклонено: вне рабочего времени ресурса",
        DeclineReason::Conflict => "Отклонено: ресурс занят",
    }
}

impl BookingDeps {
    /// Решение по приглашению `it` ресурсу `msg.recipient` с адресом `to`; `None` — обычный приём (не ресурс, правила
    /// не заданы, только исключения ряда, правка без переноса). Прежняя бронь перенесённой встречи снимается.
    pub(crate) async fn decide(
        &self,
        store: &dyn DavStore,
        cfg: &DavConfig,
        msg: &ItipMessage,
        it: &Itip,
        to: &str,
    ) -> Result<Option<Booking>> {
        let scope = msg.recipient;
        let Some(account) = self.accounts.get(scope.tenant, scope.account).await? else {
            return Ok(None);
        };
        if account.kind != AccountKind::Resource {
            return Ok(None);
        }
        let Some(policy) = self.resources.booking_policy(scope).await? else {
            return Ok(None);
        };
        let master = it
            .ical
            .components
            .iter()
            .any(|c| itip::is_sched(c) && itip::recurrence_id(c).is_none());
        if !master || !it.meeting.event {
            return Ok(None);
        }
        let existing = deliver::find_uid(store, scope, &it.meeting.uid).await?;
        if let Some(o) = &existing
            && let Ok(old) = ICalendar::parse(&o.body)
        {
            let answered = itip::meeting(&old)
                .and_then(|m| m.attendee(to).map(|a| a.partstat != itip::NEEDS_ACTION))
                .unwrap_or(false);
            if answered && !itip::rescheduled(&old, &it.ical) {
                return Ok(None);
            }
        }
        let now = OffsetDateTime::now_utc();
        let organizer = match it.meeting.organizer.as_deref() {
            Some(addr) => match self.accounts.by_address(scope.tenant, addr).await? {
                Some(AddressTarget::Account(a)) if a.enabled => Some(a.id),
                _ => None,
            },
            None => None,
        };
        let organizer_groups = self.groups_of(scope, organizer, &policy).await?;
        let (periods, unbounded) = periods(&it.ical, &policy, cfg.max_instances);
        let request = BookingRequest {
            organizer,
            organizer_groups,
            periods,
            unbounded,
        };
        if let Some(o) = &existing {
            // Прежняя бронь этой встречи не мешает новой: снимается до чтения занятости.
            store
                .delete_object(
                    scope,
                    o.collection,
                    &o.name,
                    Precondition::IfMatch(o.etag.clone()),
                )
                .await?;
        }
        let busy = match range(&request.periods) {
            Some(r) => self
                .freebusy
                .query(scope.tenant, &[scope.account], r)
                .await?
                .into_iter()
                .flat_map(|(_, p)| p)
                .collect(),
            None => Vec::new(),
        };
        let decision = policy.decide(&request, &busy, now);
        tracing::info!(?decision, "DAV: решение ресурса по приглашению");
        Ok(Some(Booking {
            decision,
            organizer,
        }))
    }

    /// Группы из [`Bookers::Only`], в которые входит организатор (с вложенными).
    async fn groups_of(
        &self,
        scope: AccountScope,
        organizer: Option<AccountId>,
        policy: &BookingPolicy,
    ) -> Result<BTreeSet<tp_contracts::GroupId>> {
        let mut out = BTreeSet::new();
        let (Some(organizer), Bookers::Only { groups, .. }) = (organizer, &policy.bookers) else {
            return Ok(out);
        };
        for g in groups {
            match self.group_store.expand(scope.tenant, *g).await {
                Ok(members) if members.contains(&organizer) => {
                    out.insert(*g);
                }
                Ok(_) | Err(tp_contracts::Error::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }
}

/// Интервал, охватывающий все вхождения; нет вхождений — `None`.
fn range(periods: &[BookingPeriod]) -> Option<Period> {
    let start = periods.iter().map(|p| p.period.start).min()?;
    let end = periods.iter().map(|p| p.period.end).max()?;
    (end > start).then_some(Period { start, end })
}

/// Вхождения VEVENT приглашения (не больше `max` развёрнутых) с местным началом в поясе рабочего времени ресурса и
/// признак повторов без конца.
fn periods(ical: &ICalendar, policy: &BookingPolicy, max: usize) -> (Vec<BookingPeriod>, bool) {
    let unbounded = ical.components.iter().any(|c| {
        c.component_type == ICalendarComponentType::VEvent
            && c.properties(&ICalendarProperty::Rrule).any(|e| {
                matches!(e.values.first(), Some(ICalendarValue::RecurrenceRule(r))
                    if r.count.is_none() && r.until.is_none())
            })
    });
    let zone = policy
        .working_hours
        .as_ref()
        .and_then(|h| Tz::from_str(&h.timezone).ok());
    let at = |unix: i64| OffsetDateTime::from_unix_timestamp(unix).ok();
    let mut out = Vec::new();
    let exp = ical.expand_dates(Tz::UTC, max);
    for e in &exp.events {
        let is_event = usize::try_from(e.comp_id)
            .ok()
            .and_then(|i| ical.components.get(i))
            .is_some_and(|c| c.component_type == ICalendarComponentType::VEvent);
        if !is_event {
            continue;
        }
        let (s, end) = e.timestamps();
        let (Some(start), Some(end)) = (at(s), at(end.max(s))) else {
            continue;
        };
        let local_start = zone.and_then(|tz| {
            let local = e
                .start
                .with_timezone(&tz)
                .naive_local()
                .and_utc()
                .timestamp();
            at(local).map(|t| PrimitiveDateTime::new(t.date(), t.time()))
        });
        out.push(BookingPeriod {
            period: Period { start, end },
            local_start,
        });
    }
    (out, unbounded)
}
