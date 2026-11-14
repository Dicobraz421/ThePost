//! Планирование CalDAV (RFC 6638): рассылка iTIP после изменения объекта, входящие (`schedule-inbox`) и исходящие
//! (`schedule-outbox`) дома календарей, запрос занятости.
//!
//! Неявное планирование (§3.2): после `PUT` и `DELETE` объекта календаря сервер по плану [`itip::plan`] рассылает
//! `REQUEST`, `CANCEL` и `REPLY` ([`ScheduleDeps::dispatch`]): адрес учётной записи организации (основной или
//! псевдоним, `AccountStore::by_address`) — [`ItipDelivery`], прочие — письмом iMIP ([`ItipSender`]). Итог —
//! `SCHEDULE-STATUS` в объекте: `1.2` доставлено, `1.1` письмо в очереди, `3.7` недопустимый адрес, `5.1` не
//! доставлено, `5.3` нет отправки писем. Участники с `SCHEDULE-AGENT=CLIENT` и `NONE` пропускаются. Участник с адресом
//! группы рассылки организации у организатора раскрывается в её учётные записи ([`ScheduleDeps::groups`],
//! [`expand_groups`]) с раскрытием вложенных групп ([`ScheduleDeps::group_store`], DAV-35): сообщения получают
//! участники группы, сама группа — нет. Удаление календаря целиком — как
//! удаление каждого его объекта: организатор рассылает `CANCEL`, участник — `REPLY` с отказом
//! (`after_delete_collection`).
//!
//! `inbox/`: `PROPFIND` (`getctag`, `schedule-default-calendar-URL`; `Depth: 1` — сообщения), `GET` и `DELETE`
//! сообщения, `REPORT` `calendar-query` и `sync-collection` по сообщениям (DAV-31). Сообщения, применённые сервером
//! и сохранённые для ленты уведомлений JMAP (`dav.schedule_keep_processed`, имя с [`deliver::PROCESSED_PREFIX`]), в
//! CalDAV не видны (решение 92). `outbox/`: `POST` с `METHOD:REQUEST` и VFREEBUSY (§5) — `schedule-response` с
//! занятостью ([`FreeBusy`]) участников организации; остальным — `3.7`.

use std::fmt::Write as _;
use std::sync::Arc;

use axum::body::Body;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::Response;
use calcard::common::PartialDateTime;
use calcard::common::timezone::Tz;
use calcard::icalendar::{
    ICalendar, ICalendarComponent, ICalendarComponentType, ICalendarEntry, ICalendarFreeBusyType,
    ICalendarMethod, ICalendarParameter, ICalendarParameterName, ICalendarParticipationStatus,
    ICalendarPeriod, ICalendarProperty, ICalendarUserTypes, ICalendarValue, Uri,
};
use time::OffsetDateTime;
use tp_contracts::dav::{CollectionKind, Component};
use tp_contracts::groups::{GroupMember, GroupStore};
use tp_contracts::meta::{AccountScope, AccountStore, AddressTarget};
use tp_contracts::queue::SubmitStatus;
use tp_contracts::schedule::{
    BusyKind, BusyPeriod, FreeBusy, Inbox, ItipDelivery, ItipMessage, ItipMethod, ItipOutcome,
    ItipOutgoing, ItipSender, ItipSource, Period, valid_address,
};
use tp_contracts::{AccountId, Error, ModSeq, Result, TenantId};

use crate::itip::{self, Message};
use crate::path::{self, Target};
use crate::props::{self, Res, Want};
use crate::query::{self, CompFilter, FilterError};
use crate::server::{Cx, multistatus, not_allowed, precondition, read, status, xml_response};
use crate::sync::{self, SyncError, SyncRequest};
use crate::xml::{self, CALDAV, Element, HEADER, Multistatus, XMLNS};
use crate::{deliver, imip};

/// Зависимости планирования встреч ([`crate::router_shared`]).
#[derive(Clone)]
pub struct ScheduleDeps {
    /// Учётные записи: адрес участника — основной адрес или псевдоним учётной записи организации, адрес группы
    /// рассылки — её участники (`AccountStore::by_address`, CTR-62); адреса вошедшего (`calendar-user-address-set`,
    /// свои при разборе участников) — все адреса его учётной записи (`Account::addresses`: основной и псевдонимы).
    pub accounts: Arc<dyn AccountStore>,
    /// Приём iTIP учётными записями организации ([`crate::ItipInbox`]).
    pub delivery: Arc<dyn ItipDelivery>,
    /// Отправка iMIP внешним участникам ([`crate::ImipSender`]); `None` — внешним не отправляется (`5.3`).
    pub sender: Option<Arc<dyn ItipSender>>,
    /// Занятость учётных записей (хранилище).
    pub freebusy: Arc<dyn FreeBusy>,
    /// Группы рассылки: раскрытие вложенных групп участника-группы с защитой от циклов (`GroupStore::expand`,
    /// DAV-35).
    pub group_store: Arc<dyn GroupStore>,
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
    /// Включённая учётная запись с адресом `addr` (основной или псевдоним): `Ok(None)` — адрес не учётной записи
    /// организации (внешний или группы рассылки), `Ok(Some(None))` — учётная запись отключена.
    async fn account(&self, scope: AccountScope, addr: &str) -> Result<Option<Option<AccountId>>> {
        Ok(match self.accounts.by_address(scope.tenant, addr).await? {
            Some(AddressTarget::Account(a)) => Some(a.enabled.then_some(a.id)),
            Some(AddressTarget::Group(_)) | None => None,
        })
    }

    /// Группы рассылки организации `tenant` среди участников объекта `ical` (RFC 5545 §3.2.11 `MEMBER`; DAV-30):
    /// адрес группы и адреса её включённых учётных записей (имя входа, [`itip::address`]) без адресов `me`.
    /// Участники с `SCHEDULE-AGENT` не `SERVER` не раскрываются. Вложенные группы ([`GroupMember::Group`]) раскрываются
    /// через [`ScheduleDeps::group_store`] (`GroupStore::expand`: каждая группа один раз, циклы не зацикливают;
    /// неизвестная вложенная группа пропускается).
    pub async fn groups(
        &self,
        tenant: TenantId,
        ical: &ICalendar,
        me: &[String],
    ) -> Result<Vec<(String, Vec<String>)>> {
        let Some(m) = itip::meeting(ical) else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for a in m
            .attendees
            .iter()
            .filter(|a| a.server && !me.contains(&a.address) && valid_address(&a.address))
        {
            let Some(AddressTarget::Group(g)) =
                self.accounts.by_address(tenant, &a.address).await?
            else {
                continue;
            };
            let mut ids: Vec<AccountId> = Vec::new();
            for m in &g.members {
                match m {
                    GroupMember::Account(id) => ids.push(*id),
                    GroupMember::Group(nested) => {
                        match self.group_store.expand(tenant, *nested).await {
                            Ok(found) => ids.extend(found),
                            Err(Error::NotFound) => {}
                            Err(e) => return Err(e),
                        }
                    }
                }
            }
            ids.sort_unstable();
            ids.dedup();
            let members = if ids.is_empty() {
                Vec::new()
            } else {
                self.accounts.by_ids(tenant, &ids).await?
            };
            let addrs: Vec<String> = members
                .iter()
                .filter(|acc| acc.enabled)
                .map(|acc| itip::address(&acc.login))
                .filter(|addr| !me.contains(addr))
                .collect();
            out.push((a.address.clone(), addrs));
        }
        Ok(out)
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

/// Добавляет в объект организатора участников групп `groups` ([`ScheduleDeps::groups`]): в каждый компонент с
/// участником-группой — её учётные записи, которых в компоненте ещё нет, с `MEMBER` — адресом группы,
/// `CUTYPE=INDIVIDUAL`, `PARTSTAT=NEEDS-ACTION`, `RSVP=TRUE`; самой группе без `CUTYPE` — `CUTYPE=GROUP`.
pub fn expand_groups(ical: &mut ICalendar, groups: &[(String, Vec<String>)]) {
    for c in ical.components.iter_mut().filter(|c| itip::is_sched(c)) {
        for (group, members) in groups {
            let Some(e) = itip::attendee_entry_mut(c, group) else {
                continue;
            };
            if e.parameter(&ICalendarParameterName::Cutype).is_none() {
                e.params
                    .push(ICalendarParameter::cutype(ICalendarUserTypes::Group));
            }
            for addr in members {
                if itip::attendee_entry(c, addr).is_some() {
                    continue;
                }
                c.entries.push(
                    ICalendarEntry::new(ICalendarProperty::Attendee)
                        .with_param(ICalendarParameter::cutype(ICalendarUserTypes::Individual))
                        .with_param(ICalendarParameter::member(Uri::Location(format!(
                            "mailto:{group}"
                        ))))
                        .with_param(ICalendarParameter::partstat(
                            ICalendarParticipationStatus::NeedsAction,
                        ))
                        .with_param(ICalendarParameter::rsvp(true))
                        .with_value(format!("mailto:{addr}")),
                );
            }
        }
    }
}

/// Итог рассылки для записи в объект: раскрытые группы и `SCHEDULE-STATUS` по адресам.
pub(crate) struct Dispatched {
    /// Новый объект с раскрытыми группами и итогами.
    pub updated: ICalendar,
    groups: Vec<(String, Vec<String>)>,
    statuses: Vec<(String, String)>,
}

impl Dispatched {
    /// Итоги поверх объекта `current`, изменённого после записи (ответ участника, применённый во время рассылки, —
    /// например, ресурса, DAV-19): группы раскрываются, `SCHEDULE-STATUS` — кроме уже полученных ответов (`2.x`).
    pub(crate) fn onto(&self, current: &ICalendar) -> ICalendar {
        let mut out = current.clone();
        expand_groups(&mut out, &self.groups);
        let answered = replied(&out);
        let statuses: Vec<(String, String)> = self
            .statuses
            .iter()
            .filter(|(a, _)| !answered.contains(a))
            .cloned()
            .collect();
        itip::set_schedule_status(&mut out, &statuses);
        out
    }
}

/// Адреса участников с `SCHEDULE-STATUS` `2.x` (ответ получен).
fn replied(ical: &ICalendar) -> Vec<String> {
    let mut out = Vec::new();
    for c in ical.components.iter().filter(|c| itip::is_sched(c)) {
        for e in c.properties(&ICalendarProperty::Attendee) {
            let ok = e.params.iter().any(|p| {
                p.name == ICalendarParameterName::ScheduleStatus
                    && p.value.as_text().is_some_and(|v| v.starts_with("2."))
            });
            if ok && let Some(a) = itip::entry_address(e) {
                out.push(a);
            }
        }
    }
    out
}

/// Рассылка после изменения объекта календаря: `old` → `new`. У организатора группы рассылки среди участников
/// раскрываются в объекте ([`expand_groups`]); самим группам сообщения не отправляются. Возвращает новый объект с
/// раскрытыми группами и `SCHEDULE-STATUS`, если что-то отправлено (`None` — рассылки не было или объект удалён).
pub(crate) async fn after_change(
    cx: &Cx<'_>,
    old: Option<&ICalendar>,
    new: Option<&ICalendar>,
) -> Option<Dispatched> {
    let deps = cx.dav.schedule.as_ref()?;
    let addresses = match cx.addresses().await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "iTIP: адреса учётной записи недоступны, рассылки нет");
            return None;
        }
    };
    let organized = |ical: &&ICalendar| {
        itip::meeting(ical)
            .and_then(|m| m.organizer)
            .is_some_and(|o| addresses.contains(&o))
    };
    let mut groups = Vec::new();
    if let Some(basis) = new.or(old).filter(organized) {
        match deps.groups(cx.scope.tenant, basis, &addresses).await {
            Ok(g) => groups = g,
            Err(e) => tracing::warn!(error = %e, "iTIP: группы среди участников не раскрыты"),
        }
    }
    let mut new = new.cloned();
    if let Some(n) = &mut new {
        expand_groups(n, &groups);
    }
    let plan = itip::plan(old, new.as_ref(), &addresses, OffsetDateTime::now_utc());
    let from = match &plan.role {
        itip::Role::Organizer(a) | itip::Role::Attendee(a) => a.clone(),
        itip::Role::None => return None,
    };
    let mut messages = plan.messages;
    for m in &mut messages {
        m.to.retain(|to| !groups.iter().any(|(g, _)| g == to));
    }
    messages.retain(|m| !m.to.is_empty());
    if messages.is_empty() {
        return None;
    }
    // Представитель планирует от имени владельца (DAV-18): адрес отправителя — владельца, `SENT-BY` — представителя.
    if !cx.own() {
        let by = itip::address(&cx.principal.login);
        for m in &mut messages {
            sent_by(&mut m.ical, &from, &by);
        }
    }
    let statuses = deps.dispatch(cx.scope, &from, &messages).await;
    let mut updated = new?;
    itip::set_schedule_status(&mut updated, &statuses);
    Some(Dispatched {
        updated,
        groups,
        statuses,
    })
}

/// Изменение объекта затрагивает планирование: прежний `old` или новый `new` — объект планирования (организатор и
/// участники). Представителю без права планирования от имени владельца такое изменение запрещено (DAV-18).
pub(crate) fn touches_scheduling(old: Option<&ICalendar>, new: Option<&ICalendar>) -> bool {
    [old, new]
        .into_iter()
        .flatten()
        .any(|i| itip::meeting(i).is_some_and(|m| m.is_scheduling()))
}

/// `SENT-BY` (RFC 5545 §3.2.18) представителя `by` у свойства отправителя `from` (`ORGANIZER` или его `ATTENDEE`)
/// во всех компонентах планирования сообщения.
pub fn sent_by(ical: &mut ICalendar, from: &str, by: &str) {
    for c in ical.components.iter_mut().filter(|c| itip::is_sched(c)) {
        for e in c.entries.iter_mut().filter(|e| {
            matches!(
                e.name,
                ICalendarProperty::Organizer | ICalendarProperty::Attendee
            ) && itip::entry_address(e).as_deref() == Some(from)
        }) {
            e.params
                .retain(|p| p.name != ICalendarParameterName::SentBy);
            e.params
                .push(ICalendarParameter::sent_by(Uri::Location(format!(
                    "mailto:{by}"
                ))));
        }
    }
}

/// Объекты планирования календаря `coll` (с `Schedule-Tag`) — для рассылки после удаления календаря целиком
/// ([`after_delete_collection`]). Без планирования — пусто.
pub(crate) async fn scheduling_objects(
    cx: &Cx<'_>,
    coll: tp_contracts::dav::CollectionId,
) -> Result<Vec<ICalendar>> {
    if cx.dav.schedule.is_none() {
        return Ok(Vec::new());
    }
    let names: Vec<String> = cx
        .store()
        .list_objects(cx.scope, coll)
        .await?
        .into_iter()
        .map(|o| o.name)
        .collect();
    if names.is_empty() {
        return Ok(Vec::new());
    }
    Ok(cx
        .store()
        .multiget(cx.scope, coll, &names)
        .await?
        .into_iter()
        .filter(|o| o.index.schedule_tag.is_some())
        .filter_map(|o| ICalendar::parse(&o.body).ok())
        .collect())
}

/// Рассылка после удаления календаря целиком (DAV-31): как удаление каждого объекта планирования `objects` —
/// организатор рассылает `CANCEL` участникам, участник — `REPLY` с отказом организатору.
pub(crate) async fn after_delete_collection(cx: &Cx<'_>, objects: &[ICalendar]) {
    for ical in objects {
        after_change(cx, Some(ical), None).await;
    }
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
    let list = visible_inbox(cx).await?;
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
    let default =
        deliver::default_calendar(cx.store(), cx.scope, &cx.dav.cfg, Component::Event).await?;
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

/// Входящие планирования, видимые в CalDAV: без сообщений, применённых сервером ([`deliver::processed`]).
async fn visible_inbox(cx: &Cx<'_>) -> Result<Inbox> {
    let mut list = cx.store().inbox_list(cx.scope).await?;
    list.items.retain(|i| !deliver::processed(&i.name));
    Ok(list)
}

fn inbox_href(cx: &Cx<'_>) -> String {
    path::collection(CollectionKind::Calendar, cx.login(), path::INBOX)
}

/// `REPORT` во входящих (DAV-31): `calendar-query` — сообщения, подходящие фильтру (время без пояса — UTC),
/// `sync-collection` — изменения после токена по журналу входящих (`DavStore::inbox_changes`, CTR-65). Имя
/// удалённого сообщения журнал не хранит: токен, после которого сообщения удалялись, — 403 `valid-sync-token`,
/// клиент синхронизируется заново. Прочие отчёты — 403 `supported-report`.
async fn inbox_report(cx: &Cx<'_>, body: Body) -> Result<Response> {
    let el = match cx.xml(body).await {
        Ok(Some(el)) => el,
        Ok(None) => return Ok(status(StatusCode::BAD_REQUEST)),
        Err(code) => return Ok(status(code)),
    };
    if el.is(CALDAV, "calendar-query") {
        return inbox_query(cx, &el).await;
    }
    let req = match SyncRequest::parse(&el) {
        Ok(r) => r,
        Err(SyncError::Report) => return Ok(query::unsupported()),
        Err(SyncError::Token) => return Ok(sync::invalid_token()),
        Err(SyncError::Level) => {
            return Ok(precondition(
                StatusCode::FORBIDDEN,
                "<d:sync-traversal-supported/>",
            ));
        }
        Err(SyncError::Limit) => return Ok(status(StatusCode::BAD_REQUEST)),
    };
    let max = cx.dav.cfg.max_response_items.max(1);
    let limit = req.limit.map_or(max, |l| l.min(max));
    let ch = match cx.store().inbox_changes(cx.scope, req.since, limit).await {
        Ok(ch) => ch,
        Err(Error::Conflict) => return Ok(sync::invalid_token()),
        Err(e) => return Err(e),
    };
    if req.since != ModSeq::ZERO && !ch.destroyed.is_empty() {
        return Ok(sync::invalid_token());
    }
    let list = visible_inbox(cx).await?;
    let want = Want::Props(req.props);
    let need_body = want.needs_body();
    let mut ms = Multistatus::new();
    for id in ch.created.iter().chain(&ch.updated) {
        let Some(i) = list.items.iter().find(|i| i.id == *id) else {
            continue;
        };
        let res = Res::InboxItem {
            name: &i.name,
            etag: &i.etag,
            body: need_body.then_some(i.message.body.as_str()),
        };
        props::emit(cx, &mut ms, &res, &want);
    }
    if ch.has_more {
        props::truncated(&mut ms, &inbox_href(cx));
    }
    ms.sync_token(&props::sync_token(ch.new_state));
    Ok(multistatus(ms))
}

/// `calendar-query` во входящих: фильтр — как у календаря ([`CompFilter`]), тело сообщения — как хранится.
async fn inbox_query(cx: &Cx<'_>, root: &Element) -> Result<Response> {
    let Some(filter) = root.child(CALDAV, "filter") else {
        return Ok(query::filter_error(FilterError::Invalid));
    };
    let filter = match CompFilter::parse(filter) {
        Ok(f) => f,
        Err(e) => return Ok(query::filter_error(e)),
    };
    let want = query::want_of(root);
    let need_body = want.needs_body();
    let max = cx.dav.cfg.max_response_items;
    let list = visible_inbox(cx).await?;
    let mut ms = Multistatus::new();
    let mut sent = 0usize;
    for i in &list.items {
        let Ok(ical) = ICalendar::parse(&i.message.body) else {
            continue;
        };
        if !query::Object::new(&ical, Tz::UTC, cx.dav.cfg.max_instances).matches(&filter) {
            continue;
        }
        if sent == max {
            props::truncated(&mut ms, &inbox_href(cx));
            break;
        }
        sent += 1;
        let res = Res::InboxItem {
            name: &i.name,
            etag: &i.etag,
            body: need_body.then_some(i.message.body.as_str()),
        };
        props::emit(cx, &mut ms, &res, &want);
    }
    Ok(multistatus(ms))
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
            let list = visible_inbox(cx).await?;
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
        ("DELETE", true, Some(name)) if deliver::processed(name) => {
            Ok(status(StatusCode::NOT_FOUND))
        }
        ("DELETE", true, Some(name)) => match cx.store().inbox_delete(cx.scope, name).await {
            Ok(_) => Ok(status(StatusCode::NO_CONTENT)),
            Err(Error::NotFound) => Ok(status(StatusCode::NOT_FOUND)),
            Err(e) => Err(e),
        },
        ("POST", false, None) => post(cx, body).await,
        ("REPORT", true, None) => inbox_report(cx, body).await,
        ("REPORT", false, None) => Ok(query::unsupported()),
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
