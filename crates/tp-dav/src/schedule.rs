//! Планирование CalDAV (RFC 6638): рассылка iTIP после изменения объекта, входящие (`schedule-inbox`) и исходящие
//! (`schedule-outbox`) дома календарей, запрос занятости.
//!
//! Неявное планирование (§3.2): после `PUT` и `DELETE` объекта календаря сервер по плану [`itip::plan`] рассылает
//! `REQUEST`, `CANCEL` и `REPLY` ([`ScheduleDeps::dispatch`]): адрес — имя входа учётной записи организации —
//! [`ItipDelivery`], прочие — письмом iMIP ([`ItipSender`]). Итог — `SCHEDULE-STATUS` в объекте: `1.2` доставлено,
//! `1.1` письмо в очереди, `3.7` недопустимый адрес, `5.1` не доставлено, `5.3` нет отправки писем. Участники с
//! `SCHEDULE-AGENT=CLIENT` и `NONE` пропускаются. Удаление календаря целиком отмен не рассылает.
//!
//! `inbox/`: `PROPFIND` (`getctag`, `schedule-default-calendar-URL`; `Depth: 1` — сообщения), `GET` и `DELETE`
//! сообщения. `outbox/`: `POST` с `METHOD:REQUEST` и VFREEBUSY (§5) — `schedule-response` с занятостью
//! ([`FreeBusy`]) участников организации; остальным — `3.7`.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use calcard::common::PartialDateTime;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarEntry, ICalendarFreeBusyType,
    ICalendarMethod, ICalendarParameter, ICalendarPeriod, ICalendarProperty, ICalendarValue,
};
use time::OffsetDateTime;
use tp_contracts::meta::{AccountScope, AccountStore};
use tp_contracts::queue::SubmitStatus;
use tp_contracts::schedule::{
    BusyKind, BusyPeriod, FreeBusy, ItipDelivery, ItipMessage, ItipMethod, ItipOutcome,
    ItipOutgoing, ItipSender, ItipSource, Period, valid_address,
};
use tp_contracts::{AccountId, Error, Result};

use crate::itip::{self, Message};
use crate::path::{self, Target};
use crate::props::{self, Res, Want};
use crate::server::{Cx, not_allowed, precondition, read, status, xml_response};
use crate::xml::{self, HEADER, Multistatus, XMLNS};
use crate::{deliver, imip};

/// Зависимости планирования встреч ([`crate::router_with`]).
#[derive(Clone)]
pub struct ScheduleDeps {
    /// Учётные записи: адрес участника — имя входа учётной записи организации; адреса вошедшего
    /// (`calendar-user-address-set`, свои при разборе участников) — все адреса его учётной записи
    /// (`Account::addresses`: основной и псевдонимы).
    pub accounts: Arc<dyn AccountStore>,
    /// Приём iTIP учётными записями организации ([`crate::ItipInbox`]).
    pub delivery: Arc<dyn ItipDelivery>,
    /// Отправка iMIP внешним участникам ([`crate::ImipSender`]); `None` — внешним не отправляется (`5.3`).
    pub sender: Option<Arc<dyn ItipSender>>,
    /// Занятость учётных записей (хранилище).
    pub freebusy: Arc<dyn FreeBusy>,
}

impl std::fmt::Debug for ScheduleDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScheduleDeps")
            .field("imip", &self.sender.is_some())
            .finish_non_exhaustive()
    }
}

/// Доставлено получателю.
pub const STATUS_DELIVERED: &str = "1.2";
/// Письмо поставлено в очередь.
pub const STATUS_PENDING: &str = "1.1";
/// Недопустимый адрес календарного пользователя.
pub const STATUS_INVALID_USER: &str = "3.7";
/// Не доставлено.
pub const STATUS_FAILED: &str = "5.1";
/// Отправка писем iMIP не настроена.
pub const STATUS_NO_IMIP: &str = "5.3";

impl ScheduleDeps {
    /// Включённая учётная запись с именем входа `addr`: `Ok(None)` — адрес не организации (внешний),
    /// `Ok(Some(None))` — учётная запись отключена.
    async fn account(&self, scope: AccountScope, addr: &str) -> Result<Option<Option<AccountId>>> {
        Ok(self
            .accounts
            .by_login(scope.tenant, addr)
            .await?
            .map(|a| a.enabled.then_some(a.id)))
    }

    /// Рассылает сообщения от учётной записи `sender` с адресом `from` (`ORGANIZER` или её `ATTENDEE`):
    /// получателям организации — [`ItipDelivery`], внешним — одним письмом iMIP на сообщение. Возвращает
    /// `SCHEDULE-STATUS` по адресам (последний итог адреса); ошибки доставки — статус `5.1`, не ошибка вызова.
    pub async fn dispatch(
        &self,
        sender: AccountScope,
        from: &str,
        messages: &[Message],
    ) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        let mut set = |addr: &str, st: &str| match out.iter_mut().find(|(a, _)| a == addr) {
            Some(e) => e.1 = st.to_owned(),
            None => out.push((addr.to_owned(), st.to_owned())),
        };
        for m in messages {
            let ics = m.ics();
            let mut external = Vec::new();
            for to in &m.to {
                if !valid_address(to) {
                    set(to, STATUS_INVALID_USER);
                    continue;
                }
                let account = match self.account(sender, to).await {
                    Ok(Some(Some(a))) => a,
                    Ok(Some(None)) => {
                        set(to, STATUS_FAILED);
                        continue;
                    }
                    Ok(None) => {
                        external.push(to.clone());
                        continue;
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "iTIP: учётная запись получателя недоступна");
                        set(to, STATUS_FAILED);
                        continue;
                    }
                };
                let msg = ItipMessage {
                    recipient: AccountScope {
                        tenant: sender.tenant,
                        account,
                    },
                    to: to.clone(),
                    from: from.to_owned(),
                    method: m.method,
                    ics: ics.clone(),
                    source: ItipSource::Internal,
                };
                match self.delivery.deliver(&msg).await {
                    Ok(ItipOutcome::Applied) => set(to, STATUS_DELIVERED),
                    Ok(ItipOutcome::Ignored(reason)) => {
                        tracing::debug!(%reason, method = %m.method, "iTIP: получатель не принял");
                        set(to, STATUS_FAILED);
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, method = %m.method, "iTIP: сбой доставки");
                        set(to, STATUS_FAILED);
                    }
                }
            }
            if external.is_empty() {
                continue;
            }
            let Some(imip) = &self.sender else {
                for to in &external {
                    set(to, STATUS_NO_IMIP);
                }
                continue;
            };
            let (subject, text) = imip::describe(m.method, &m.ical);
            let mail = ItipOutgoing {
                sender,
                from: from.to_owned(),
                to: external.clone(),
                method: m.method,
                ics,
                subject,
                text,
            };
            match imip.send(&mail).await {
                Ok(sub) => {
                    for to in &external {
                        let st = sub.rcpts.iter().find(|(r, _)| r == to).map(|(_, s)| s);
                        set(
                            to,
                            match st {
                                Some(SubmitStatus::Delivered) => STATUS_DELIVERED,
                                Some(SubmitStatus::Queued) => STATUS_PENDING,
                                Some(SubmitStatus::Failed(_)) | None => STATUS_FAILED,
                            },
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, method = %m.method, "iMIP: письмо не поставлено");
                    for to in &external {
                        set(to, STATUS_FAILED);
                    }
                }
            }
        }
        out
    }
}

/// Рассылка после изменения объекта календаря вошедшим: `old` → `new`. Возвращает новый объект с
/// `SCHEDULE-STATUS`, если что-то отправлено (`None` — рассылки не было или объект удалён).
pub(crate) async fn after_change(
    cx: &Cx<'_>,
    old: Option<&ICalendar>,
    new: Option<&ICalendar>,
) -> Option<ICalendar> {
    let deps = cx.dav.schedule.as_ref()?;
    let addresses = match cx.addresses().await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "iTIP: адреса учётной записи недоступны, рассылки нет");
            return None;
        }
    };
    let plan = itip::plan(old, new, &addresses, OffsetDateTime::now_utc());
    let from = match &plan.role {
        itip::Role::Organizer(a) | itip::Role::Attendee(a) => a.clone(),
        itip::Role::None => return None,
    };
    if plan.messages.is_empty() {
        return None;
    }
    let statuses = deps.dispatch(cx.scope, &from, &plan.messages).await;
    let mut updated = new?.clone();
    itip::set_schedule_status(&mut updated, &statuses);
    Some(updated)
}

// ---------- Ресурсы inbox и outbox ----------

/// Ресурс планирования дома календарей: входящие (`true`) или исходящие и имя сообщения.
pub(crate) fn resource(target: &Target) -> Option<(bool, Option<&str>)> {
    use tp_contracts::dav::CollectionKind::Calendar;
    match target {
        Target::Collection(Calendar, _, c) if path::reserved(Calendar, c) => {
            Some((c == path::INBOX, None))
        }
        Target::Object(Calendar, _, c, o) if path::reserved(Calendar, c) => {
            Some((c == path::INBOX, Some(o)))
        }
        _ => None,
    }
}

/// Свойства ресурса планирования (`PROPFIND`): входящие — с `getctag`, календарём по умолчанию и, при `depth1`,
/// сообщениями.
pub(crate) async fn propfind(
    cx: &Cx<'_>,
    ms: &mut Multistatus,
    inbox: bool,
    item: Option<&str>,
    depth1: bool,
    want: &Want,
) -> Result<bool> {
    if !inbox {
        if item.is_some() {
            return Ok(false);
        }
        let res = Res::Schedule {
            inbox,
            ctag: None,
            default: None,
        };
        props::emit(cx, ms, &res, want);
        return Ok(true);
    }
    let list = cx.store().inbox_list(cx.scope).await?;
    if let Some(name) = item {
        let Some(i) = list.items.iter().find(|i| i.name == name) else {
            return Ok(false);
        };
        let res = Res::InboxItem {
            name: &i.name,
            etag: &i.etag,
            body: Some(&i.message.body),
        };
        props::emit(cx, ms, &res, want);
        return Ok(true);
    }
    let default = deliver::default_calendar(cx.store(), cx.scope, &cx.dav.cfg).await?;
    let res = Res::Schedule {
        inbox,
        ctag: Some(list.state),
        default: default.as_ref().map(|c| c.name.as_str()),
    };
    props::emit(cx, ms, &res, want);
    if depth1 {
        let max = cx.dav.cfg.max_response_items;
        for i in list.items.iter().take(max) {
            let res = Res::InboxItem {
                name: &i.name,
                etag: &i.etag,
                body: want.needs_body().then_some(i.message.body.as_str()),
            };
            props::emit(cx, ms, &res, want);
        }
        if list.items.len() > max {
            props::truncated(
                ms,
                &path::collection(
                    tp_contracts::dav::CollectionKind::Calendar,
                    cx.login(),
                    path::INBOX,
                ),
            );
        }
    }
    Ok(true)
}

fn calendar_response(code: StatusCode, etag: Option<&str>, body: String, head: bool) -> Response {
    let len = body.len();
    let mut resp = Response::new(if head {
        Body::empty()
    } else {
        Body::from(body)
    });
    *resp.status_mut() = code;
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/calendar; charset=utf-8"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(len));
    if let Some(e) = etag.and_then(|e| HeaderValue::from_str(&format!("\"{e}\"")).ok()) {
        h.insert(header::ETAG, e);
    }
    resp
}

/// Методы ресурсов планирования, кроме `PROPFIND`.
pub(crate) async fn handle(
    cx: &Cx<'_>,
    method: &str,
    inbox: bool,
    item: Option<&str>,
    body: Body,
) -> Result<Response> {
    match (method, inbox, item) {
        ("GET" | "HEAD", true, Some(name)) => {
            let list = cx.store().inbox_list(cx.scope).await?;
            Ok(match list.items.into_iter().find(|i| i.name == name) {
                Some(i) => calendar_response(
                    StatusCode::OK,
                    Some(&i.etag),
                    i.message.body,
                    method == "HEAD",
                ),
                None => status(StatusCode::NOT_FOUND),
            })
        }
        ("DELETE", true, Some(name)) => match cx.store().inbox_delete(cx.scope, name).await {
            Ok(_) => Ok(status(StatusCode::NO_CONTENT)),
            Err(Error::NotFound) => Ok(status(StatusCode::NOT_FOUND)),
            Err(e) => Err(e),
        },
        ("POST", false, None) => post(cx, body).await,
        ("REPORT", _, None) => Ok(crate::query::unsupported()),
        ("GET" | "HEAD" | "DELETE", false, Some(_)) => Ok(status(StatusCode::NOT_FOUND)),
        ("PUT" | "DELETE" | "PROPPATCH" | "MKCOL" | "MKCALENDAR", ..) => {
            Ok(status(StatusCode::FORBIDDEN))
        }
        _ => Ok(not_allowed()),
    }
}

fn timestamp(c: &ICalendarComponent, p: &ICalendarProperty) -> Option<OffsetDateTime> {
    let t = c
        .property(p)?
        .values
        .first()?
        .as_partial_date_time()?
        .to_timestamp()?;
    OffsetDateTime::from_unix_timestamp(t).ok()
}

/// Ответ VFREEBUSY (`METHOD:REPLY`) с периодами `busy` участника `attendee` запроса `req`.
fn freebusy_reply(
    req: &ICalendarComponent,
    attendee: &ICalendarEntry,
    range: Period,
    busy: &[BusyPeriod],
) -> String {
    let mut root = ICalendarComponent::new(ICalendarComponentType::VCalendar);
    root.add_property(ICalendarProperty::Version, "2.0");
    root.add_property(ICalendarProperty::Prodid, props::PRODID);
    root.add_property(ICalendarProperty::Method, ICalendarMethod::Reply);
    root.component_ids.push(1);
    let utc = |t: OffsetDateTime| PartialDateTime::from_utc_timestamp(t.unix_timestamp());
    let mut fb = ICalendarComponent::new(ICalendarComponentType::VFreebusy);
    fb.entries.extend(
        req.entries
            .iter()
            .filter(|e| {
                matches!(
                    e.name,
                    ICalendarProperty::Uid | ICalendarProperty::Organizer
                )
            })
            .cloned(),
    );
    fb.add_dtstamp(PartialDateTime::now());
    fb.add_property(ICalendarProperty::Dtstart, utc(range.start));
    fb.add_property(ICalendarProperty::Dtend, utc(range.end));
    fb.entries.push(attendee.clone());
    for (kind, fbtype) in [
        (BusyKind::Busy, ICalendarFreeBusyType::Busy),
        (BusyKind::Tentative, ICalendarFreeBusyType::BusyTentative),
        (
            BusyKind::Unavailable,
            ICalendarFreeBusyType::BusyUnavailable,
        ),
    ] {
        let values: Vec<ICalendarValue> = busy
            .iter()
            .filter(|p| p.kind == kind)
            .map(|p| {
                ICalendarValue::Period(ICalendarPeriod::Range {
                    start: utc(p.start),
                    end: utc(p.end),
                })
            })
            .collect();
        if !values.is_empty() {
            fb.entries.push(
                ICalendarEntry::new(ICalendarProperty::Freebusy)
                    .with_param(ICalendarParameter::fbtype(fbtype))
                    .with_values(values),
            );
        }
    }
    ICalendar {
        components: vec![root, fb],
    }
    .to_string()
}

/// `POST` в исходящие: запрос занятости (RFC 6638 §5).
async fn post(cx: &Cx<'_>, body: Body) -> Result<Response> {
    const MESSAGE: &str = "<c:valid-scheduling-message/>";
    let bad = |inner: &str| Ok(precondition(StatusCode::FORBIDDEN, inner));
    let Some(deps) = &cx.dav.schedule else {
        return Ok(status(StatusCode::FORBIDDEN));
    };
    let Ok(bytes) = read(body, cx.dav.cfg.max_request_size).await else {
        return Ok(status(StatusCode::PAYLOAD_TOO_LARGE));
    };
    let Some(ical) = std::str::from_utf8(&bytes)
        .ok()
        .and_then(|t| ICalendar::parse(t).ok())
    else {
        return bad("<c:valid-calendar-data/>");
    };
    let Some(req) = ical
        .components
        .iter()
        .find(|c| c.component_type == ICalendarComponentType::VFreebusy)
        .filter(|_| itip::method(&ical) == Some(ItipMethod::Request))
    else {
        return bad(MESSAGE);
    };
    let (Some(start), Some(end)) = (
        timestamp(req, &ICalendarProperty::Dtstart),
        timestamp(req, &ICalendarProperty::Dtend),
    ) else {
        return bad(MESSAGE);
    };
    let range = Period { start, end };
    if range.validate().is_err() {
        return bad(MESSAGE);
    }
    let addresses = cx.addresses().await?;
    let organizer = req
        .property(&ICalendarProperty::Organizer)
        .and_then(itip::entry_address);
    if !organizer.is_some_and(|o| addresses.contains(&o)) {
        return bad("<c:valid-organizer/>");
    }
    let attendees: Vec<(&ICalendarEntry, String)> = req
        .properties(&ICalendarProperty::Attendee)
        .filter_map(|e| Some((e, itip::entry_address(e)?)))
        .collect();
    if attendees.is_empty() {
        return bad(MESSAGE);
    }
    if attendees.len() > cx.dav.cfg.max_freebusy_attendees {
        return bad("<c:max-attendees-per-instance/>");
    }
    let mut accounts: Vec<Option<AccountId>> = Vec::with_capacity(attendees.len());
    for (_, addr) in &attendees {
        accounts.push(deps.account(cx.scope, addr).await?.flatten());
    }
    let ids: Vec<AccountId> = accounts.iter().flatten().copied().collect();
    let busy = if ids.is_empty() {
        Vec::new()
    } else {
        deps.freebusy.query(cx.scope.tenant, &ids, range).await?
    };
    let mut out = format!("{HEADER}<c:schedule-response{XMLNS}>");
    for ((entry, addr), account) in attendees.iter().zip(&accounts) {
        let _ = write!(
            out,
            "<c:response><c:recipient>{}</c:recipient>",
            xml::href(&format!("mailto:{addr}"))
        );
        match account {
            Some(id) => {
                let periods = busy
                    .iter()
                    .find(|(a, _)| a == id)
                    .map_or(&[][..], |(_, p)| p.as_slice());
                let data = freebusy_reply(req, entry, range, periods);
                let _ = write!(
                    out,
                    "<c:request-status>2.0;Success</c:request-status><c:calendar-data>{}</c:calendar-data>",
                    xml::text(&data)
                );
            }
            None => out.push_str("<c:request-status>3.7;Invalid calendar user</c:request-status>"),
        }
        out.push_str("</c:response>");
    }
    out.push_str("</c:schedule-response>");
    Ok(xml_response(StatusCode::OK, out))
}
