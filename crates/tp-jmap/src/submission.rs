//! Identity и EmailSubmission (RFC 8621 §6, §7).
//!
//! - Identity — адрес учётной записи (имя входа) с отображаемым именем из `AccountStore`; адреса и подписи
//!   ведёт каталог, поэтому `Identity/set` отвечает `forbidden`.
//! - `EmailSubmission/set`, создание: письмо отправляется сразу ([`Outbound::submit`](tp_contracts::queue::Outbound::submit),
//!   `undoStatus` = `final`) или, при `sendAt` в будущем (не дальше `maxDelayedSend` — `jmap.submission.max_delayed_send`,
//!   иначе `invalidProperties`), ставится в очередь со сроком выпуска ([`Outbound::schedule`](tp_contracts::queue::Outbound::schedule),
//!   `undoStatus` = `pending`). Адрес `From` проверяет [`Authorizer::send_right`](tp_contracts::auth::Authorizer::send_right)
//!   (свои адреса, Send As, Send on Behalf — с заголовком `Sender`). До очереди — `DecisionHooks::on_submission`
//!   (`Deny` — `forbiddenToSend`; дополнения: заголовки в начало письма, копии в папки и внешним адресам; у отложенного
//!   письма копии внешним адресам выпускаются тем же сроком, копии в папки кладутся при постановке).
//!   Ещё раньше — правила потока почты ([`Jmap::with_mail_flow`](crate::Jmap::with_mail_flow), CTR-24): `Reject` —
//!   `forbiddenToSend` с текстом правила, `Drop` — письмо принято и не отправлено (получатели `queued`), изменённые конверт
//!   и тело — дальше вместо исходных. Заголовок `Bcc` из отправляемого письма удаляется.
//! - Каждая отправка записывается в [`SubmissionStore`](tp_contracts::submission::SubmissionStore): `EmailSubmission/get`,
//!   `changes`, `query`, `queryChanges` читают записи, `state` и `queryState` — состояние хранилища. `deliveryStatus` —
//!   статусы получателей записи (доставлено — `yes`, в очереди или отложено — `queued`, отказ — `no` с ответом SMTP);
//!   у ждущей срока и отменённой отправки — `null`.
//! - Статус доставки (JMAP-16): перед чтением отправки учётной записи сверяются с журналом доставки ([`sync_delivery`]) —
//!   по записи очереди отправки ([`DeliveryLog::by_queue`](tp_contracts::delivery::DeliveryLog::by_queue)): доставка в
//!   ящик и приём внешним сервером — `yes` (как и письмо, принятое и отброшенное правилом: отправителю об этом не
//!   сообщается), отказ — `no` с причиной из журнала, временный отказ, карантин и модерация — `queued`. Новые статусы записываются в хранилище, поэтому меняют `state` и видны в `EmailSubmission/changes` и push.
//!   У письма, отправленного сразу, записи очереди нет ([`Submitted`] её не сообщает): его статусы — ответ
//!   [`Outbound::submit`](tp_contracts::queue::Outbound::submit).
//! - `EmailSubmission/set`, изменение — только отмена отложенной отправки (`undoStatus: canceled`): письмо снимается
//!   с очереди ([`Outbound::cancel_scheduled`](tp_contracts::queue::Outbound::cancel_scheduled)), запись отменяется;
//!   отправленное или уже выпущенное письмо — `cannotUnsend`. Удаление убирает запись, на письмо и очередь не влияет.

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use axum::body::Bytes;
use mail_parser::MessageParser;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use tp_contracts::auth::SendRight;
use tp_contracts::delivery::{DeliveryDecision, DeliveryEvent, DeliveryEventKind};
use tp_contracts::ext::{CopyTo, Decision, SubmissionInfo};
use tp_contracts::mailstore::IngestRequest;
use tp_contracts::meta::{AccountScope, Changes};
use tp_contracts::queue::{SubmitStatus, Submitted};
use tp_contracts::submission::{EmailSubmission, SubmissionFilter, UndoStatus};
use tp_contracts::transport::{BlobRef, FlowDirection, FlowEnvelope, FlowInput, FlowVerdict};
use tp_contracts::{Error, MessageId, ModSeq};

use crate::core::error::{MethodError, MethodResult, changes_error, set_error};
use crate::core::request::{
    ChangesArgs, Comparator, GetArgs, QueryChangesArgs, QueryWindow, SetOutcome, apply_window,
    object, or_null, parse_args, query_changes_response, select,
};
use crate::core::{Ctx, Jmap, digest, lone_thread_id, thread_id};
use crate::email_get::{collect, utc_date};

// ---------- Identity ----------

fn identity_id(login: &str) -> String {
    format!("I{}", digest(login.to_lowercase().as_bytes()))
}

async fn identity(ctx: &Ctx<'_>) -> MethodResult<Map<String, Value>> {
    let p = ctx.principal;
    let name = ctx
        .jmap
        .d
        .accounts
        .get(p.tenant, p.account)
        .await?
        .map(|a| a.display_name)
        .unwrap_or_default();
    Ok(object(json!({
        "id": identity_id(&p.login),
        "name": name,
        "email": p.login,
        "replyTo": null,
        "bcc": null,
        "textSignature": "",
        "htmlSignature": "",
        "mayDelete": false,
    })))
}

fn identity_state(identity: &Map<String, Value>) -> String {
    digest(Value::Object(identity.clone()).to_string().as_bytes())
}

/// `Identity/get`.
pub(crate) async fn identity_get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let me = identity(ctx).await?;
    let state = identity_state(&me);
    let id = identity_id(&ctx.principal.login);
    let (mut list, mut not_found) = (Vec::new(), Vec::new());
    match &a.ids {
        None => list.push(me),
        Some(ids) => {
            for i in ids {
                if *i == id {
                    list.push(me.clone());
                } else {
                    not_found.push(i.clone());
                }
            }
        }
    }
    let list: Vec<Value> = list
        .into_iter()
        .map(|o| Value::Object(select(o, a.properties.as_deref())))
        .collect();
    Ok(object(json!({
        "accountId": a.account_id,
        "state": state,
        "list": list,
        "notFound": not_found,
    })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IdentitySetArgs {
    account_id: String,
    if_in_state: Option<String>,
    create: Option<Map<String, Value>>,
    update: Option<Map<String, Value>>,
    destroy: Option<Vec<String>>,
}

/// `Identity/set`: адреса и подписи ведёт каталог — каждый объект `forbidden`.
pub(crate) async fn identity_set(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: IdentitySetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let state = identity_state(&identity(ctx).await?);
    if a.if_in_state.as_ref().is_some_and(|s| *s != state) {
        return Err(MethodError::new("stateMismatch"));
    }
    let deny = || {
        set_error(
            "forbidden",
            Some("identities are managed by the directory"),
            &[],
        )
    };
    let mut not_created = Map::new();
    for (cid, _) in a.create.unwrap_or_default() {
        not_created.insert(cid, deny());
    }
    let mut not_updated = Map::new();
    for (id, _) in a.update.unwrap_or_default() {
        not_updated.insert(id, deny());
    }
    let mut not_destroyed = Map::new();
    for id in a.destroy.unwrap_or_default() {
        not_destroyed.insert(id, deny());
    }
    Ok(object(json!({
        "accountId": a.account_id,
        "oldState": state,
        "newState": state,
        "created": null,
        "updated": null,
        "destroyed": null,
        "notCreated": or_null(not_created),
        "notUpdated": or_null(not_updated),
        "notDestroyed": or_null(not_destroyed),
    })))
}

// ---------- EmailSubmission ----------

fn undo_name(u: UndoStatus) -> &'static str {
    match u {
        UndoStatus::Pending => "pending",
        UndoStatus::Final => "final",
        UndoStatus::Canceled => "canceled",
    }
}

/// `deliveryStatus` EmailSubmission (RFC 8621 §7): получатель → `smtpReply`, `delivered`, `displayed`; статусов
/// нет (отправка ждёт срока или отменена) — `null`.
fn delivery_status(rcpts: &[(String, SubmitStatus)]) -> Value {
    if rcpts.is_empty() {
        return Value::Null;
    }
    let mut m = Map::new();
    for (rcpt, st) in rcpts {
        let (reply, delivered) = match st {
            SubmitStatus::Delivered => ("250 2.0.0 Delivered".to_owned(), "yes"),
            SubmitStatus::Queued => ("250 2.0.0 Queued".to_owned(), "queued"),
            SubmitStatus::Failed(r) if r.trim().is_empty() => {
                ("550 5.0.0 Not delivered".to_owned(), "no")
            }
            SubmitStatus::Failed(r) => (r.clone(), "no"),
        };
        m.insert(
            rcpt.clone(),
            json!({"smtpReply": reply, "delivered": delivered, "displayed": "unknown"}),
        );
    }
    Value::Object(m)
}

/// Статус получателя по событию журнала доставки; событие не о доставке получателю (приём, уведомление DSN) — `None`.
fn tracked(e: &DeliveryEvent) -> Option<SubmitStatus> {
    if matches!(e.kind, DeliveryEventKind::Receive | DeliveryEventKind::Dsn) {
        return None;
    }
    Some(match e.decision {
        // Доставлено в ящик или принято внешним сервером: дальше письмо ведёт он.
        DeliveryDecision::Accept | DeliveryDecision::Discard => SubmitStatus::Delivered,
        DeliveryDecision::Reject => SubmitStatus::Failed(e.reason.clone().unwrap_or_default()),
        DeliveryDecision::Defer | DeliveryDecision::Quarantine | DeliveryDecision::Hold => {
            SubmitStatus::Queued
        }
    })
}

/// Статусы получателей отправки по событиям её записи очереди (по возрастанию времени): действует последнее
/// событие о получателе; о ком событий нет — в очереди.
fn track(rcpts: &[String], events: &[DeliveryEvent]) -> Vec<(String, SubmitStatus)> {
    let mut out: Vec<(String, SubmitStatus)> = rcpts
        .iter()
        .map(|r| (r.clone(), SubmitStatus::Queued))
        .collect();
    for e in events {
        let Some(status) = tracked(e) else { continue };
        for (rcpt, st) in &mut out {
            if e.recipients.iter().any(|x| x.eq_ignore_ascii_case(rcpt)) {
                *st = status.clone();
            }
        }
    }
    out
}

/// Сверка отправок учётной записи с журналом доставки (JMAP-16): у отправок с записью очереди, срок которых
/// наступил, статусы получателей берутся из [`DeliveryLog::by_queue`](tp_contracts::delivery::DeliveryLog::by_queue)
/// и записываются [`SubmissionStore::finalize`](tp_contracts::submission::SubmissionStore::finalize) — отправка
/// становится `final`, изменение статусов меняет состояние и попадает в `EmailSubmission/changes`. Отправки, у которых
/// все получатели в конечном статусе, и отменённые не перечитываются.
pub(crate) async fn sync_delivery(jmap: &Jmap, scope: AccountScope) -> tp_contracts::Result<()> {
    let now = OffsetDateTime::now_utc();
    let limit = jmap.cfg.submission.max_delivery_events;
    for s in jmap.d.submissions.list(scope).await? {
        let Some(queue) = s.queue else { continue };
        let due = match s.undo_status {
            UndoStatus::Canceled => continue,
            UndoStatus::Pending if s.send_at > now => continue,
            UndoStatus::Pending => true,
            UndoStatus::Final => false,
        };
        let settled =
            !s.delivery.is_empty() && s.delivery.iter().all(|(_, st)| *st != SubmitStatus::Queued);
        if !due && settled {
            continue;
        }
        let events = jmap
            .d
            .delivery_log
            .by_queue(scope.tenant, queue, limit)
            .await?;
        let delivery = track(&s.rcpt_to, &events);
        if !due && delivery == s.delivery {
            continue;
        }
        match jmap
            .d
            .submissions
            .finalize(scope, &s.id, delivery, Some(queue))
            .await
        {
            // Отменена или удалена между чтением и записью — сверять нечего.
            Ok(_) | Err(Error::Conflict | Error::NotFound) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Состояние отправок учётной записи после сверки с журналом доставки ([`sync_delivery`]) — `state` и `queryState`
/// EmailSubmission, состояние типа в push.
pub(crate) async fn synced_state(jmap: &Jmap, scope: AccountScope) -> tp_contracts::Result<String> {
    sync_delivery(jmap, scope).await?;
    Ok(jmap.d.submissions.state(scope).await?.to_string())
}

/// Состояние отправок учётной записи (`state`, `queryState`) — [`synced_state`].
async fn submission_state(ctx: &Ctx<'_>) -> MethodResult<String> {
    Ok(synced_state(ctx.jmap, ctx.scope).await?)
}

/// Состояние отправок из строки клиента; чужая строка — `cannotCalculateChanges`.
fn parse_submission_state(s: &str) -> MethodResult<u64> {
    s.parse()
        .map_err(|_| MethodError::new("cannotCalculateChanges"))
}

/// Цепочки писем учётной записи: письмо → `threadId`.
async fn threads(ctx: &Ctx<'_>) -> MethodResult<HashMap<MessageId, String>> {
    Ok(ctx
        .jmap
        .messages_in(ctx.scope, None)
        .await?
        .iter()
        .map(|m| (m.id, thread_id(m)))
        .collect())
}

/// `threadId` письма отправки; письмо удалено — его собственная цепочка.
async fn thread_of(ctx: &Ctx<'_>, email: MessageId) -> MethodResult<String> {
    Ok(match ctx.jmap.d.messages.get(ctx.scope, email).await? {
        Some(m) => thread_id(&m),
        None => lone_thread_id(email),
    })
}

/// Объект EmailSubmission (RFC 8621 §7). Параметры SMTP конверта и DSN/MDN не ведутся: `parameters` — `null`,
/// `dsnBlobIds` и `mdnBlobIds` пусты.
fn submission_object(s: &EmailSubmission, thread: &str) -> Map<String, Value> {
    let rcpt_to: Vec<Value> = s
        .rcpt_to
        .iter()
        .map(|r| json!({"email": r, "parameters": null}))
        .collect();
    object(json!({
        "id": s.id,
        "identityId": s.identity,
        "emailId": s.email.to_string(),
        "threadId": thread,
        "envelope": {
            "mailFrom": {"email": s.mail_from, "parameters": null},
            "rcptTo": rcpt_to,
        },
        "sendAt": utc_date(s.send_at),
        "undoStatus": undo_name(s.undo_status),
        "deliveryStatus": delivery_status(&s.delivery),
        "dsnBlobIds": [],
        "mdnBlobIds": [],
    }))
}

/// `EmailSubmission/get`.
pub(crate) async fn submission_get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let store = &ctx.jmap.d.submissions;
    let state = submission_state(ctx).await?;
    let (mut found, mut not_found) = (Vec::new(), Vec::new());
    match &a.ids {
        None => found = store.list(ctx.scope).await?,
        Some(ids) => {
            for id in ids {
                match store.get(ctx.scope, ctx.resolve_id(id)).await? {
                    Some(s) => found.push(s),
                    None => not_found.push(id.clone()),
                }
            }
        }
    }
    let mut list = Vec::with_capacity(found.len());
    for s in &found {
        let thread = thread_of(ctx, s.email).await?;
        let obj = submission_object(s, &thread);
        list.push(Value::Object(select(obj, a.properties.as_deref())));
    }
    Ok(object(json!({
        "accountId": a.account_id,
        "state": state,
        "list": list,
        "notFound": not_found,
    })))
}

/// `EmailSubmission/changes` (RFC 8621 §7.2) по журналу [`SubmissionStore::submission_changes`](tp_contracts::submission::SubmissionStore::submission_changes).
pub(crate) async fn submission_changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: ChangesArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let since = parse_submission_state(&a.since_state)?;
    let limit = a.limit(ctx.jmap.cfg.max_changes)?;
    sync_delivery(ctx.jmap, ctx.scope).await?;
    let c = ctx
        .jmap
        .d
        .submissions
        .submission_changes(ctx.scope, since, limit)
        .await
        .map_err(changes_error)?;
    Ok(object(json!({
        "accountId": a.account_id,
        "oldState": a.since_state,
        "newState": c.new_state.to_string(),
        "hasMoreChanges": c.has_more,
        "created": c.created,
        "updated": c.updated,
        "destroyed": c.destroyed,
    })))
}

/// Свойства сортировки `EmailSubmission/query` (RFC 8621 §7.3).
const SUBMISSION_SORT: &[&str] = &["emailId", "threadId", "sentAt"];

/// Фильтр `EmailSubmission/query` (RFC 8621 §7.3): операторы и условия [`SubmissionFilter`].
enum Filter {
    And(Vec<Filter>),
    Or(Vec<Filter>),
    Not(Vec<Filter>),
    Cond(SubmissionFilter),
}

impl Filter {
    fn matches(&self, s: &EmailSubmission) -> bool {
        match self {
            Filter::And(f) => f.iter().all(|f| f.matches(s)),
            Filter::Or(f) => f.iter().any(|f| f.matches(s)),
            Filter::Not(f) => !f.iter().any(|f| f.matches(s)),
            Filter::Cond(c) => c.matches(s),
        }
    }
}

/// Упоминает ли фильтр цепочки (`threadIds`).
fn mentions_threads(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.contains_key("threadIds") || m.values().any(mentions_threads),
        Value::Array(a) => a.iter().any(mentions_threads),
        _ => false,
    }
}

/// Разбирает фильтр: операторы `AND`/`OR`/`NOT` и условия `identityIds`, `emailIds`, `threadIds`, `undoStatus`,
/// `before`, `after` (по `sendAt`). `threadIds` переводится в письма цепочек по `threads`.
fn parse_filter(v: &Value, threads: &HashMap<MessageId, String>) -> MethodResult<Filter> {
    let unsupported = |why: &str| MethodError::with("unsupportedFilter", why);
    let Value::Object(m) = v else {
        return Err(unsupported("filter must be an object"));
    };
    if let Some(op) = m.get("operator") {
        let conds = m
            .get("conditions")
            .and_then(Value::as_array)
            .ok_or_else(|| unsupported("conditions"))?
            .iter()
            .map(|c| parse_filter(c, threads))
            .collect::<MethodResult<Vec<_>>>()?;
        return match op.as_str() {
            Some("AND") => Ok(Filter::And(conds)),
            Some("OR") => Ok(Filter::Or(conds)),
            Some("NOT") => Ok(Filter::Not(conds)),
            _ => Err(unsupported("operator")),
        };
    }
    let mut f = SubmissionFilter::default();
    let mut by_thread: Option<Vec<MessageId>> = None;
    for (k, v) in m {
        let bad = || MethodError::invalid(format!("filter: bad {k}"));
        let strings = || -> MethodResult<Vec<&str>> {
            v.as_array()
                .and_then(|ids| ids.iter().map(Value::as_str).collect())
                .ok_or_else(bad)
        };
        let date = || {
            v.as_str()
                .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
                .ok_or_else(bad)
        };
        match k.as_str() {
            "identityIds" => {
                f.identity_ids = Some(strings()?.into_iter().map(str::to_owned).collect());
            }
            "emailIds" => {
                let ids = strings()?;
                f.email_ids = Some(
                    ids.into_iter()
                        .filter_map(|i| MessageId::from_str(i).ok())
                        .collect(),
                );
            }
            "threadIds" => {
                let ids = strings()?;
                by_thread = Some(
                    threads
                        .iter()
                        .filter(|(_, t)| ids.contains(&t.as_str()))
                        .map(|(m, _)| *m)
                        .collect(),
                );
            }
            "undoStatus" => {
                f.undo_status = Some(match v.as_str() {
                    Some("pending") => UndoStatus::Pending,
                    Some("final") => UndoStatus::Final,
                    Some("canceled") => UndoStatus::Canceled,
                    _ => return Err(bad()),
                });
            }
            "before" => f.before = Some(date()?),
            "after" => f.after = Some(date()?),
            _ => return Err(unsupported(k)),
        }
    }
    if let Some(of_threads) = by_thread {
        f.email_ids = Some(match f.email_ids.take() {
            Some(ids) => ids.into_iter().filter(|i| of_threads.contains(i)).collect(),
            None => of_threads,
        });
    }
    Ok(Filter::Cond(f))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubmissionQueryArgs {
    account_id: String,
    filter: Option<Value>,
    sort: Option<Vec<Comparator>>,
    #[serde(flatten)]
    window: QueryWindow,
}

/// Результаты `EmailSubmission/query`: отправки учётной записи по фильтру и сортировке; без сортировки и при
/// равенстве — по `sendAt`, затем по `id`.
async fn submission_ids(ctx: &Ctx<'_>, a: &SubmissionQueryArgs) -> MethodResult<Vec<String>> {
    let sort = a.sort.as_deref().unwrap_or_default();
    for c in sort {
        if !SUBMISSION_SORT.contains(&c.property.as_str()) {
            return Err(MethodError::with("unsupportedSort", c.property.clone()));
        }
    }
    let by_thread = sort.iter().any(|c| c.property == "threadId");
    let threads = if by_thread || a.filter.as_ref().is_some_and(mentions_threads) {
        threads(ctx).await?
    } else {
        HashMap::new()
    };
    let filter = match &a.filter {
        Some(f) => Some(parse_filter(f, &threads)?),
        None => None,
    };
    let mut list = ctx.jmap.d.submissions.list(ctx.scope).await?;
    list.retain(|s| filter.as_ref().is_none_or(|f| f.matches(s)));
    let thread = |s: &EmailSubmission| {
        threads
            .get(&s.email)
            .cloned()
            .unwrap_or_else(|| lone_thread_id(s.email))
    };
    list.sort_by(|x, y| {
        for c in sort {
            let ord = match c.property.as_str() {
                "emailId" => x.email.to_string().cmp(&y.email.to_string()),
                "threadId" => thread(x).cmp(&thread(y)),
                _ => x.send_at.cmp(&y.send_at),
            };
            let ord = if c.is_ascending { ord } else { ord.reverse() };
            if ord.is_ne() {
                return ord;
            }
        }
        (x.send_at, &x.id).cmp(&(y.send_at, &y.id))
    });
    Ok(list.into_iter().map(|s| s.id).collect())
}

/// `EmailSubmission/query` (RFC 8621 §7.3): `queryState` — состояние отправок.
pub(crate) async fn submission_query(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: SubmissionQueryArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let state = submission_state(ctx).await?;
    let ids = submission_ids(ctx, &a).await?;
    let mut out = object(json!({
        "accountId": a.account_id,
        "queryState": state,
        "canCalculateChanges": true,
    }));
    apply_window(&ids, &a.window, ctx.jmap.cfg.max_query_limit, &mut out)?;
    Ok(out)
}

/// `EmailSubmission/queryChanges` (RFC 8620 §5.6) по журналу изменений отправок: изменённые и удалённые — в
/// `removed`, изменённые и созданные, входящие в результаты, — в `added`. Чужое или устаревшее состояние —
/// `cannotCalculateChanges`.
pub(crate) async fn submission_query_changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let qc: QueryChangesArgs = parse_args(args.clone())?;
    ctx.check_account(&qc.account_id)?;
    let mut q = args;
    q.retain(|k, _| matches!(k.as_str(), "accountId" | "filter" | "sort"));
    let a: SubmissionQueryArgs = parse_args(q)?;
    let since = parse_submission_state(&qc.since_query_state)?;
    let limit = qc.limit(ctx.jmap.cfg.max_changes)?;
    sync_delivery(ctx.jmap, ctx.scope).await?;
    let c = ctx
        .jmap
        .d
        .submissions
        .submission_changes(ctx.scope, since, limit)
        .await
        .map_err(changes_error)?;
    let ids = submission_ids(ctx, &a).await?;
    let changes = Changes {
        created: c.created,
        updated: c.updated,
        destroyed: c.destroyed,
        new_state: ModSeq(c.new_state),
        has_more: c.has_more,
    };
    query_changes_response(&qc, &ids, &changes, limit)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SubmissionSetArgs {
    account_id: String,
    if_in_state: Option<String>,
    create: Option<Map<String, Value>>,
    update: Option<Map<String, Value>>,
    destroy: Option<Vec<String>>,
    on_success_update_email: Option<Map<String, Value>>,
    on_success_destroy_email: Option<Vec<String>>,
}

/// Адрес конверта (`EmailSubmission.envelope`): параметры SMTP не поддерживаются (`submissionExtensions` пуст).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvAddress {
    email: String,
    #[serde(default)]
    parameters: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Envelope {
    mail_from: EnvAddress,
    rcpt_to: Vec<EnvAddress>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Create {
    identity_id: String,
    email_id: String,
    #[serde(default)]
    envelope: Option<Envelope>,
    #[serde(default)]
    undo_status: Option<String>,
    /// Срок отложенной отправки (UTCDate); нет или уже наступил — отправить сейчас.
    #[serde(default)]
    send_at: Option<String>,
}

/// Синтаксически допустимый адрес конверта: `local@domain` без пробелов и угловых скобок.
fn valid_address(a: &str) -> bool {
    a.split_once('@')
        .is_some_and(|(l, d)| !l.is_empty() && !d.is_empty() && !d.contains('@'))
        && !a
            .bytes()
            .any(|b| b.is_ascii_whitespace() || b == b'<' || b == b'>' || b.is_ascii_control())
}

/// Конец заголовков письма (позиция после пустой строки) или длина письма.
fn header_end(raw: &[u8]) -> usize {
    let mut i = 0;
    while i < raw.len() {
        let line_end = raw[i..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(raw.len(), |p| i + p + 1);
        let line = &raw[i..line_end];
        if line == b"\r\n" || line == b"\n" {
            return line_end;
        }
        i = line_end;
    }
    raw.len()
}

/// Письмо без полей заголовка `name` (с продолжениями строк).
fn strip_header(raw: &[u8], name: &str) -> Vec<u8> {
    let end = header_end(raw);
    let mut out = Vec::with_capacity(raw.len());
    let mut skip = false;
    let mut i = 0;
    while i < end {
        let line_end = raw[i..end]
            .iter()
            .position(|&b| b == b'\n')
            .map_or(end, |p| i + p + 1);
        let line = &raw[i..line_end];
        let continuation = matches!(line.first(), Some(b' ' | b'\t'));
        if !continuation {
            skip = line
                .iter()
                .position(|&b| b == b':')
                .is_some_and(|c| line[..c].trim_ascii().eq_ignore_ascii_case(name.as_bytes()));
        }
        if !skip {
            out.extend_from_slice(line);
        }
        i = line_end;
    }
    out.extend_from_slice(&raw[end..]);
    out
}

/// Добавляет поля заголовка в начало письма.
fn prepend_headers(raw: Vec<u8>, headers: &[(String, String)]) -> Vec<u8> {
    if headers.is_empty() {
        return raw;
    }
    let mut out = Vec::with_capacity(raw.len() + 128);
    for (k, v) in headers {
        out.extend_from_slice(format!("{k}: {v}\r\n").as_bytes());
    }
    out.extend_from_slice(&raw);
    out
}

/// Итог правил потока для отправки.
enum Flowed {
    /// Отправить: конверт и тело после действий правил.
    Send {
        mail_from: String,
        rcpts: Vec<String>,
        raw: Vec<u8>,
    },
    /// Принять без отправки и без уведомления (`Drop`, `Accept` без получателей): исходные получатели — `queued`.
    Drop(Vec<String>),
    /// Отказ по объекту (SetError).
    Refuse(Value),
}

/// Правила потока почты (CONTRACTS.md#Transport): один вызов [`MailFlow::apply`](tp_contracts::transport::MailFlow::apply)
/// на письмо до точек принятия решений и очереди. Тело передаётся объектом BLOB организации без ссылок (его, как и
/// изменённое тело модуля, убирает сборка мусора по окну: в очередь уходят байты письма). `Reject` —
/// `forbiddenToSend` с текстом правила; `Moderate` — `forbiddenToSend` (очереди модерации у JMAP нет);
/// `route` не передаётся (контракт [`Outbound`](tp_contracts::queue::Outbound) без маршрута: очередь выбирает политику по домену) — предупреждение
/// в журнале.
async fn mail_flow(
    ctx: &Ctx<'_>,
    mail_from: String,
    rcpts: Vec<String>,
    raw: Vec<u8>,
) -> MethodResult<Flowed> {
    let Some(flow) = &ctx.jmap.mail_flow else {
        return Ok(Flowed::Send {
            mail_from,
            rcpts,
            raw,
        });
    };
    let p = ctx.principal;
    let size = raw.len() as u64;
    let key = ctx
        .jmap
        .put_object(p.tenant, Bytes::copy_from_slice(&raw))
        .await?;
    let input = FlowInput {
        tenant: p.tenant,
        direction: FlowDirection::Submission,
        principal: Some(p.clone()),
        source: None,
        envelope: FlowEnvelope {
            mail_from,
            rcpt_to: rcpts,
        },
        message: BlobRef { key, size },
    };
    let out = flow.apply(&input).await?;
    if !out.matched.is_empty() {
        tracing::info!(rules = ?out.matched, "jmap: сработали правила потока почты");
    }
    match out.verdict {
        FlowVerdict::Reject(text) => {
            tracing::info!(reason = %text, "jmap: отправка отклонена правилом потока почты");
            Ok(Flowed::Refuse(set_error(
                "forbiddenToSend",
                Some(&text),
                &[],
            )))
        }
        FlowVerdict::Moderate { .. } => {
            tracing::warn!(
                "jmap: правило потока требует модерации — у отправки JMAP её нет, отказ"
            );
            Ok(Flowed::Refuse(set_error(
                "forbiddenToSend",
                Some("message requires moderation"),
                &[],
            )))
        }
        FlowVerdict::Drop => Ok(Flowed::Drop(input.envelope.rcpt_to)),
        FlowVerdict::Accept if out.envelope.rcpt_to.is_empty() => {
            Ok(Flowed::Drop(input.envelope.rcpt_to))
        }
        FlowVerdict::Accept => {
            if out.route.is_some() {
                tracing::warn!(
                    "jmap: маршрут правила потока не применён — очередь выбирает его по домену"
                );
            }
            let raw = match out.message {
                None => raw,
                Some(m) => collect(ctx.jmap.d.blobs.get(&m.key, None).await?).await?,
            };
            Ok(Flowed::Send {
                mail_from: out.envelope.mail_from,
                rcpts: out.envelope.rcpt_to,
                raw,
            })
        }
    }
}

/// Срок отложенной отправки из `sendAt`: `None` — отправить сейчас (свойства нет или момент уже наступил);
/// дальше `maxDelayedSend` или не дата — `invalidProperties`.
fn delayed_until(
    send_at: Option<&str>,
    now: OffsetDateTime,
    max_delay: std::time::Duration,
) -> std::result::Result<Option<OffsetDateTime>, Value> {
    let Some(s) = send_at else {
        return Ok(None);
    };
    let Ok(at) = OffsetDateTime::parse(s, &Rfc3339) else {
        return Err(set_error(
            "invalidProperties",
            Some("sendAt must be a date"),
            &["sendAt"],
        ));
    };
    if at <= now {
        return Ok(None);
    }
    let max = time::Duration::try_from(max_delay).unwrap_or(time::Duration::MAX);
    if at - now > max {
        return Err(set_error(
            "invalidProperties",
            Some("sendAt is later than maxDelayedSend allows"),
            &["sendAt"],
        ));
    }
    Ok(Some(at))
}

/// Одна отправка: `Ok(Ok(запись))` — отправлена или поставлена в очередь со сроком, `Ok(Err(SetError))` — отказ
/// по объекту. Запись в [`SubmissionStore`](tp_contracts::submission::SubmissionStore): отправленная сразу —
/// `Final` со статусами получателей, отложенная — `Pending` с записью очереди.
async fn submit_one(
    ctx: &Ctx<'_>,
    obj: Value,
) -> MethodResult<std::result::Result<EmailSubmission, Value>> {
    let c: Create = match serde_json::from_value(obj) {
        Ok(c) => c,
        Err(e) => {
            return Ok(Err(set_error(
                "invalidProperties",
                Some(&e.to_string()),
                &[],
            )));
        }
    };
    let now = OffsetDateTime::now_utc();
    let max_delay = ctx.jmap.cfg.submission.max_delayed_send;
    let release_at = match delayed_until(c.send_at.as_deref(), now, max_delay) {
        Ok(at) => at,
        Err(e) => return Ok(Err(e)),
    };
    // `undoStatus` задаёт сервер: клиент может только повторить его значение.
    let undo = if release_at.is_some() {
        UndoStatus::Pending
    } else {
        UndoStatus::Final
    };
    if c.undo_status
        .as_deref()
        .is_some_and(|u| u != undo_name(undo))
    {
        return Ok(Err(set_error(
            "invalidProperties",
            Some("undoStatus is set by the server"),
            &["undoStatus"],
        )));
    }
    let d = &ctx.jmap.d;
    let Some(outbound) = &d.outbound else {
        return Ok(Err(set_error(
            "forbiddenToSend",
            Some("sending is disabled on this node"),
            &[],
        )));
    };
    let p = ctx.principal;
    let identity = identity_id(&p.login);
    if ctx.resolve_id(&c.identity_id) != identity {
        return Ok(Err(set_error("invalidProperties", None, &["identityId"])));
    }
    let msg = match MessageId::from_str(ctx.resolve_id(&c.email_id)) {
        Ok(id) => d.messages.get(ctx.scope, id).await?,
        Err(_) => None,
    };
    let Some(msg) = msg else {
        return Ok(Err(set_error(
            "invalidProperties",
            Some("email not found"),
            &["emailId"],
        )));
    };
    let mut raw = collect(d.store.fetch_raw(ctx.scope, msg.id, None).await?).await?;

    // Адреса из заголовков.
    let (from, sender, header_rcpts) = {
        let Some(parsed) = MessageParser::default().parse_headers(&raw) else {
            return Ok(Err(set_error("invalidEmail", None, &[])));
        };
        let from = parsed
            .from()
            .and_then(|a| a.first())
            .and_then(|a| a.address())
            .map(str::to_owned);
        let sender = parsed
            .sender()
            .and_then(|a| a.first())
            .and_then(|a| a.address())
            .map(str::to_owned);
        let mut rcpts = Vec::new();
        for list in [parsed.to(), parsed.cc(), parsed.bcc()]
            .into_iter()
            .flatten()
        {
            rcpts.extend(list.iter().filter_map(|a| a.address()).map(str::to_owned));
        }
        (from, sender, rcpts)
    };
    let Some(from) = from else {
        return Ok(Err(set_error("invalidEmail", Some("no From address"), &[])));
    };
    let Some(right) = d.authz.send_right(p, &from).await? else {
        return Ok(Err(set_error("forbiddenFrom", None, &[])));
    };
    let mut add = Vec::new();
    if right == SendRight::SendOnBehalf {
        match &sender {
            None => add.push(("Sender".to_owned(), p.login.clone())),
            Some(s) if s.eq_ignore_ascii_case(&p.login) => {}
            Some(_) => {
                return Ok(Err(set_error(
                    "forbiddenFrom",
                    Some("Sender must be the user"),
                    &[],
                )));
            }
        }
    }

    // Конверт: заданный клиентом или из заголовков.
    let (mail_from, rcpts) = match c.envelope {
        Some(env) => {
            if env
                .mail_from
                .parameters
                .as_ref()
                .is_some_and(|v| !v.is_null())
                || env
                    .rcpt_to
                    .iter()
                    .any(|r| r.parameters.as_ref().is_some_and(|v| !v.is_null()))
            {
                return Ok(Err(set_error(
                    "invalidProperties",
                    Some("SMTP parameters are not supported"),
                    &["envelope"],
                )));
            }
            let mf = env.mail_from.email;
            let allowed = mf.eq_ignore_ascii_case(&p.login)
                || (right == SendRight::SendAs && mf.eq_ignore_ascii_case(&from));
            if !allowed {
                return Ok(Err(set_error("forbiddenMailFrom", None, &[])));
            }
            (
                mf,
                env.rcpt_to.into_iter().map(|r| r.email).collect::<Vec<_>>(),
            )
        }
        None => {
            let mf = if right == SendRight::SendAs {
                from.clone()
            } else {
                p.login.clone()
            };
            (mf, header_rcpts)
        }
    };
    let mut seen = HashSet::new();
    let rcpts: Vec<String> = rcpts
        .into_iter()
        .filter(|r| seen.insert(r.to_lowercase()))
        .collect();
    if rcpts.is_empty() {
        return Ok(Err(set_error("noRecipients", None, &[])));
    }
    if rcpts.len() > ctx.jmap.cfg.max_submission_recipients as usize {
        return Ok(Err(set_error("tooManyRecipients", None, &[])));
    }
    let invalid: Vec<&String> = rcpts.iter().filter(|r| !valid_address(r)).collect();
    if !invalid.is_empty() {
        let mut e = set_error("invalidRecipients", None, &[]);
        e["invalidRecipients"] = json!(invalid);
        return Ok(Err(e));
    }

    let mut record = EmailSubmission {
        id: format!("S{}", MessageId::new()),
        identity,
        email: msg.id,
        mail_from: mail_from.clone(),
        rcpt_to: rcpts.clone(),
        send_at: now,
        undo_status: UndoStatus::Final,
        delivery: Vec::new(),
        queue: None,
    };
    raw = prepend_headers(strip_header(&raw, "Bcc"), &add);
    let (mail_from, rcpts) = match mail_flow(ctx, mail_from, rcpts, raw).await? {
        Flowed::Send {
            mail_from,
            rcpts,
            raw: changed,
        } => {
            raw = changed;
            (mail_from, rcpts)
        }
        Flowed::Drop(rcpts) => {
            // Принято без отправки: запись окончательная, срока выпуска нет.
            record.delivery = Submitted::all(&rcpts, SubmitStatus::Queued).rcpts;
            d.submissions.insert(ctx.scope, record.clone()).await?;
            return Ok(Ok(record));
        }
        Flowed::Refuse(e) => return Ok(Err(e)),
    };
    let info = SubmissionInfo {
        principal: p.clone(),
        mail_from: mail_from.clone(),
        rcpt_to: rcpts.clone(),
        size: raw.len() as u64,
    };
    let mut copies = Vec::new();
    match d.hooks.on_submission(&info).await? {
        Decision::Allow => {}
        Decision::Deny(reason) => {
            tracing::info!(%reason, "jmap: отправка запрещена точкой принятия решений");
            return Ok(Err(set_error(
                "forbiddenToSend",
                Some("rejected by policy"),
                &[],
            )));
        }
        Decision::Augment(a) => {
            raw = prepend_headers(raw, &a.headers);
            copies = a.copies;
        }
    }
    let mut external = Vec::new();
    let mut boxes = Vec::new();
    for c in copies {
        match c {
            CopyTo::Address(a) => external.push(a),
            CopyTo::Mailbox(delivery) => boxes.push(delivery),
        }
    }
    let body = Bytes::from(raw);
    match release_at {
        None => {
            record.delivery = outbound
                .submit(p.tenant, &mail_from, &rcpts, body.clone())
                .await?
                .rcpts;
            if !external.is_empty() {
                outbound
                    .enqueue(p.tenant, &mail_from, &external, body.clone())
                    .await?;
            }
        }
        Some(at) => {
            // Копии внешним адресам выпускаются тем же сроком, что и письмо.
            let queue = outbound
                .schedule(p.tenant, &mail_from, &rcpts, body.clone(), at)
                .await?;
            if !external.is_empty() {
                outbound
                    .schedule(p.tenant, &mail_from, &external, body.clone(), at)
                    .await?;
            }
            record.send_at = at;
            record.undo_status = UndoStatus::Pending;
            record.queue = Some(queue);
        }
    }
    if !boxes.is_empty() {
        d.store
            .ingest(IngestRequest {
                tenant: p.tenant,
                mail_from: mail_from.clone(),
                raw: Box::pin(futures::stream::once(async move { Ok(body) })),
                deliveries: boxes,
                received_at: now,
            })
            .await?;
    }
    if let Err(e) = d.submissions.insert(ctx.scope, record.clone()).await {
        // Письмо уже в очереди: отложенное снимается, отправленное сразу остаётся без записи.
        if let Some(queue) = record.queue
            && let Err(undo) = outbound.cancel_scheduled(p.tenant, queue).await
        {
            tracing::error!(error = %undo, "jmap: отложенное письмо без записи отправки не снято с очереди");
        }
        tracing::error!(error = %e, "jmap: запись отправки не сохранена");
        return Err(e.into());
    }
    Ok(Ok(record))
}

/// Отмена отложенной отправки (`undoStatus: canceled`): письмо снимается с очереди
/// ([`Outbound::cancel_scheduled`](tp_contracts::queue::Outbound::cancel_scheduled)), затем отменяется запись.
/// Уже отменённая — успех; отправленная или выпущенная из очереди — `cannotUnsend`. `Ok(Ok(запись))` — отменена.
async fn cancel_one(
    ctx: &Ctx<'_>,
    id: &str,
    patch: &Value,
) -> MethodResult<std::result::Result<EmailSubmission, Value>> {
    let Value::Object(p) = patch else {
        return Ok(Err(set_error("invalidPatch", None, &[])));
    };
    let bad: Vec<&str> = p
        .iter()
        .filter(|(k, v)| !(*k == "undoStatus" && v.as_str() == Some("canceled")))
        .map(|(k, _)| k.as_str())
        .collect();
    if !bad.is_empty() || p.is_empty() {
        return Ok(Err(set_error(
            "invalidProperties",
            Some("only undoStatus can be changed, to canceled"),
            &bad,
        )));
    }
    let d = &ctx.jmap.d;
    let Some(s) = d.submissions.get(ctx.scope, id).await? else {
        return Ok(Err(set_error("notFound", None, &[])));
    };
    let cannot = || set_error("cannotUnsend", None, &[]);
    match s.undo_status {
        UndoStatus::Canceled => return Ok(Ok(s)),
        UndoStatus::Final => return Ok(Err(cannot())),
        UndoStatus::Pending => {}
    }
    if let Some(queue) = s.queue {
        let Some(outbound) = &d.outbound else {
            return Ok(Err(cannot()));
        };
        match outbound.cancel_scheduled(ctx.scope.tenant, queue).await {
            Ok(()) => {}
            // Письмо уже выпущено (или доставлено и запись очереди удалена): отправка окончательная.
            Err(Error::Conflict | Error::NotFound) => {
                match d
                    .submissions
                    .finalize(ctx.scope, id, Vec::new(), s.queue)
                    .await
                {
                    Ok(_) | Err(Error::Conflict | Error::NotFound) => {}
                    Err(e) => return Err(e.into()),
                }
                return Ok(Err(cannot()));
            }
            Err(e) => return Err(e.into()),
        }
    }
    match d.submissions.cancel(ctx.scope, id).await {
        Ok(s) => Ok(Ok(s)),
        Err(Error::Conflict) => Ok(Err(cannot())),
        Err(Error::NotFound) => Ok(Err(set_error("notFound", None, &[]))),
        Err(e) => Err(e.into()),
    }
}

/// `EmailSubmission/set`: создание — отправка сразу или постановка в очередь со сроком `sendAt`; изменение —
/// отмена отложенной отправки; удаление — удаление записи (на письмо и очередь не влияет). Затем неявный
/// `Email/set` (`onSuccessUpdateEmail`, `onSuccessDestroyEmail`) по письмам успешно созданных, изменённых и
/// удалённых отправок — ответ после ответа метода.
pub(crate) async fn submission_set(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: SubmissionSetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let n = a.create.as_ref().map_or(0, Map::len)
        + a.update.as_ref().map_or(0, Map::len)
        + a.destroy.as_ref().map_or(0, Vec::len);
    if n > ctx.jmap.cfg.max_objects_in_set as usize {
        return Err(MethodError::new("requestTooLarge"));
    }
    let old = submission_state(ctx).await?;
    if a.if_in_state.as_ref().is_some_and(|s| *s != old) {
        return Err(MethodError::new("stateMismatch"));
    }
    let mut out = SetOutcome::default();
    // Успешные отправки: (creation id, id отправки, письмо).
    let mut done: Vec<(Option<String>, String, MessageId)> = Vec::new();
    for (cid, obj) in a.create.unwrap_or_default() {
        match submit_one(ctx, obj).await? {
            Ok(s) => {
                let thread = thread_of(ctx, s.email).await?;
                let mut created = submission_object(&s, &thread);
                created.retain(|k, _| !matches!(k.as_str(), "identityId" | "emailId"));
                out.created.insert(cid.clone(), Value::Object(created));
                ctx.created.insert(cid.clone(), s.id.clone());
                done.push((Some(cid), s.id, s.email));
            }
            Err(e) => {
                out.not_created.insert(cid, e);
            }
        }
    }
    for (key, patch) in a.update.unwrap_or_default() {
        let id = ctx.resolve_id(&key).to_owned();
        match cancel_one(ctx, &id, &patch).await? {
            Ok(s) => {
                out.updated.insert(key, Value::Null);
                done.push((None, s.id, s.email));
            }
            Err(e) => {
                out.not_updated.insert(key, e);
            }
        }
    }
    for key in a.destroy.unwrap_or_default() {
        let id = ctx.resolve_id(&key).to_owned();
        let store = &ctx.jmap.d.submissions;
        let email = store.get(ctx.scope, &id).await?.map(|s| s.email);
        let deleted = match email {
            Some(email) => match store.delete(ctx.scope, &id).await {
                Ok(_) => Some(email),
                Err(Error::NotFound) => None,
                Err(e) => return Err(e.into()),
            },
            None => None,
        };
        match deleted {
            Some(email) => {
                done.push((None, id, email));
                out.destroyed.push(key);
            }
            None => {
                out.not_destroyed
                    .insert(key, set_error("notFound", None, &[]));
            }
        }
    }
    let email_of = |key: &str| {
        done.iter()
            .find(|(cid, id, _)| key.strip_prefix('#') == cid.as_deref() || key == id)
            .map(|(_, _, e)| e.to_string())
    };
    let mut update = Map::new();
    for (key, patch) in a.on_success_update_email.unwrap_or_default() {
        if let Some(e) = email_of(&key) {
            update.insert(e, patch);
        }
    }
    let destroy: Vec<String> = a
        .on_success_destroy_email
        .unwrap_or_default()
        .iter()
        .filter_map(|k| email_of(k))
        .collect();
    let new = submission_state(ctx).await?;
    let out = out.response(&a.account_id, &old, &new);
    if !update.is_empty() || !destroy.is_empty() {
        let args = object(json!({
            "accountId": a.account_id,
            "update": or_null(update),
            "destroy": if destroy.is_empty() { Value::Null } else { json!(destroy) },
        }));
        let r = crate::email_set::set(ctx, args).await?;
        ctx.implicit.push(("Email/set".into(), r));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use tp_contracts::meta::SpecialUse;
    use tp_contracts::queue::Outbound;
    use tp_contracts::submission::SubmissionStore;
    use tp_contracts::{Result, TenantId};

    use crate::JmapConfig;
    use crate::core::{Deps, Jmap};
    use crate::testenv::Env;

    const USING: &[&str] = &[
        "urn:ietf:params:jmap:core",
        "urn:ietf:params:jmap:mail",
        "urn:ietf:params:jmap:submission",
    ];

    async fn calls(env: &Env, jmap: &Jmap, calls: Value) -> Vec<Value> {
        let req = json!({"using": USING, "methodCalls": calls});
        let r = jmap
            .api(&env.principal, req.to_string().as_bytes())
            .await
            .expect("api");
        let v = serde_json::to_value(r).expect("json");
        v["methodResponses"].as_array().cloned().expect("responses")
    }

    fn draft(env: &Env, from: &str) -> Value {
        json!({
            "mailboxIds": {env.mailbox(SpecialUse::Drafts).to_string(): true},
            "keywords": {"$draft": true},
            "from": [{"email": from}],
            "to": [{"email": "rcpt@external.test"}],
            "bcc": [{"email": "hidden@external.test"}],
            "subject": "Отправка",
            "textBody": [{"partId": "t"}],
            "bodyValues": {"t": {"value": "текст"}},
        })
    }

    /// Identity/get — адрес учётной записи; Identity/set — forbidden.
    #[tokio::test]
    async fn identities() {
        let env = Env::new().await;
        let r = calls(
            &env,
            &env.jmap,
            json!([
                ["Identity/get", {"accountId": env.account()}, "a"],
                ["Identity/set", {"accountId": env.account(), "create": {"n": {"email": "x@example.org"}}}, "b"]
            ]),
        )
        .await;
        let list = &r[0][1]["list"];
        assert_eq!(list.as_array().map(Vec::len), Some(1));
        assert_eq!(list[0]["email"], "user@example.org");
        assert_eq!(list[0]["mayDelete"], false);
        assert_eq!(r[1][1]["notCreated"]["n"]["type"], "forbidden");
    }

    /// Отправка черновика попадает в очередь: конверт из заголовков, без Bcc;
    /// неявный Email/set переносит письмо в «Отправленные».
    #[tokio::test]
    async fn submission_enqueues() {
        let env = Env::new().await;
        let acc = env.account();
        let identity = identity_id("user@example.org");
        let drafts = env.mailbox(SpecialUse::Drafts).to_string();
        let sent = env.mailbox(SpecialUse::Sent).to_string();
        let r = calls(
            &env,
            &env.jmap,
            json!([
                ["Email/set", {"accountId": acc, "create": {"d": draft(&env, "user@example.org")}}, "0"],
                ["EmailSubmission/set", {
                    "accountId": acc,
                    "create": {"s": {"identityId": identity, "emailId": "#d"}},
                    "onSuccessUpdateEmail": {"#s": {
                        format!("mailboxIds/{drafts}"): null,
                        format!("mailboxIds/{sent}"): true,
                        "keywords/$draft": null
                    }}
                }, "1"]
            ]),
        )
        .await;
        assert_eq!(r[1][0], "EmailSubmission/set", "{:?}", r[1]);
        assert_eq!(r[1][1]["created"]["s"]["undoStatus"], "final");
        assert_eq!(r[2][0], "Email/set", "неявный Email/set: {r:?}");
        assert_eq!(r[2][2], "1");
        let email = r[0][1]["created"]["d"]["id"].as_str().expect("email");
        // Перенос сохраняет id (CTR-29): `updated` — `null`.
        assert_eq!(r[2][1]["updated"], json!({email: null}), "{r:?}");
        let moved = email;
        let e = env
            .call(
                "Email/get",
                json!({"accountId": acc, "ids": [moved], "properties": ["mailboxIds", "keywords"]}),
                "Email/get",
            )
            .await;
        assert_eq!(e["list"][0]["mailboxIds"], json!({&sent: true}));
        assert_eq!(e["list"][0]["keywords"], json!({}));

        let q = env.outbound.queued();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].tenant, env.scope.tenant);
        assert_eq!(q[0].mail_from, "user@example.org");
        assert_eq!(
            q[0].rcpts,
            vec!["rcpt@external.test", "hidden@external.test"]
        );
        let raw = String::from_utf8_lossy(&q[0].raw);
        assert!(!raw.contains("hidden@external.test"), "{raw}");
        assert!(raw.contains("Subject: "), "{raw}");
    }

    /// Смешанный список получателей: статус каждого из `Outbound::submit` — в `deliveryStatus` ответа.
    #[tokio::test]
    async fn delivery_status_per_recipient() {
        let env = Env::new().await;
        let acc = env.account();
        let identity = identity_id("user@example.org");
        env.outbound
            .set_status("local@example.org", SubmitStatus::Delivered);
        env.outbound.set_status(
            "nobody@example.org",
            SubmitStatus::Failed("550 5.1.1 No such user".into()),
        );
        env.outbound
            .set_status("full@example.org", SubmitStatus::Failed(String::new()));
        let rcpts = [
            "local@example.org",
            "rcpt@external.test",
            "nobody@example.org",
            "full@example.org",
        ];
        let envelope = json!({"mailFrom": {"email": "user@example.org"},
            "rcptTo": rcpts.iter().map(|r| json!({"email": r})).collect::<Vec<_>>()});
        let r = calls(
            &env,
            &env.jmap,
            json!([
                ["Email/set", {"accountId": acc, "create": {"d": draft(&env, "user@example.org")}}, "0"],
                ["EmailSubmission/set", {"accountId": acc, "create": {"s": {
                    "identityId": identity, "emailId": "#d", "envelope": envelope
                }}}, "1"]
            ]),
        )
        .await;
        let ds = &r[1][1]["created"]["s"]["deliveryStatus"];
        assert_eq!(ds.as_object().map(Map::len), Some(4), "{r:?}");
        let st = |rcpt: &str| {
            (
                ds[rcpt]["delivered"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                ds[rcpt]["smtpReply"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
                ds[rcpt]["displayed"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
        };
        assert_eq!(st("local@example.org").0, "yes");
        assert!(st("local@example.org").1.starts_with("250 "));
        assert_eq!(st("rcpt@external.test").0, "queued");
        assert!(st("rcpt@external.test").1.starts_with("250 "));
        assert_eq!(
            st("nobody@example.org"),
            (
                "no".into(),
                "550 5.1.1 No such user".into(),
                "unknown".into()
            )
        );
        assert_eq!(st("full@example.org").0, "no");
        assert!(st("full@example.org").1.starts_with("550 "));
        for rcpt in rcpts {
            assert_eq!(st(rcpt).2, "unknown", "{rcpt}");
        }
        assert_eq!(env.outbound.queued()[0].rcpts, rcpts);
    }

    /// Очередь только с `enqueue` (реализация `submit` по умолчанию): все получатели — `queued`.
    #[tokio::test]
    async fn delivery_status_default_queued() {
        struct Plain(tp_testkit::MemOutbound);
        #[async_trait]
        impl Outbound for Plain {
            async fn enqueue(
                &self,
                tenant: TenantId,
                mail_from: &str,
                rcpts: &[String],
                raw: Bytes,
            ) -> Result<()> {
                self.0.enqueue(tenant, mail_from, rcpts, raw).await
            }
        }
        let env = Env::new().await;
        let plain = Jmap::new(
            Deps {
                outbound: Some(std::sync::Arc::new(Plain(Default::default()))),
                ..env.jmap.d.clone()
            },
            JmapConfig::default(),
        )
        .expect("jmap");
        let acc = env.account();
        let r = calls(
            &env,
            &plain,
            json!([
                ["Email/set", {"accountId": acc, "create": {"d": draft(&env, "user@example.org")}}, "0"],
                ["EmailSubmission/set", {"accountId": acc, "create": {"s": {
                    "identityId": identity_id("user@example.org"), "emailId": "#d"
                }}}, "1"]
            ]),
        )
        .await;
        let ds = &r[1][1]["created"]["s"]["deliveryStatus"];
        assert_eq!(
            *ds,
            json!({
                "rcpt@external.test": {"smtpReply": "250 2.0.0 Queued", "delivered": "queued", "displayed": "unknown"},
                "hidden@external.test": {"smtpReply": "250 2.0.0 Queued", "delivered": "queued", "displayed": "unknown"},
            }),
            "{r:?}"
        );
    }

    /// Чужой адрес From: без права — forbiddenFrom; Send on Behalf — заголовок Sender; Send As — без него;
    /// конверт клиента проверяется; узел без очереди — forbiddenToSend.
    #[tokio::test]
    async fn send_as_and_on_behalf() {
        let env = Env::new().await;
        let acc = env.account();
        let identity = identity_id("user@example.org");
        let submit = |envelope: Value| {
            json!(["EmailSubmission/set", {"accountId": acc, "create": {"s": {
                "identityId": identity, "emailId": "#d", "envelope": envelope
            }}}, "1"])
        };
        let send = |envelope: Value| {
            let calls_json = json!([
                ["Email/set", {"accountId": acc, "create": {"d": draft(&env, "boss@example.org")}}, "0"],
                submit(envelope)
            ]);
            calls(&env, &env.jmap, calls_json)
        };
        let r = send(Value::Null).await;
        assert_eq!(r[1][1]["notCreated"]["s"]["type"], "forbiddenFrom", "{r:?}");

        env.authz
            .0
            .lock()
            .expect("lock")
            .insert("boss@example.org".into(), SendRight::SendOnBehalf);
        let r = send(Value::Null).await;
        assert!(r[1][1]["created"]["s"].is_object(), "{r:?}");
        let q = env.outbound.queued();
        assert_eq!(q[0].mail_from, "user@example.org");
        assert!(String::from_utf8_lossy(&q[0].raw).starts_with("Sender: user@example.org\r\n"));
        let r = send(
            json!({"mailFrom": {"email": "boss@example.org"}, "rcptTo": [{"email": "a@b.test"}]}),
        )
        .await;
        assert_eq!(r[1][1]["notCreated"]["s"]["type"], "forbiddenMailFrom");

        env.authz
            .0
            .lock()
            .expect("lock")
            .insert("boss@example.org".into(), SendRight::SendAs);
        let r = send(
            json!({"mailFrom": {"email": "boss@example.org"}, "rcptTo": [{"email": "a@b.test"}]}),
        )
        .await;
        assert!(r[1][1]["created"]["s"].is_object(), "{r:?}");
        let q = env.outbound.queued();
        assert_eq!(q.len(), 2);
        assert_eq!(q[1].mail_from, "boss@example.org");
        assert_eq!(q[1].rcpts, vec!["a@b.test"]);
        assert!(!String::from_utf8_lossy(&q[1].raw).contains("Sender:"));
        let r = send(json!({"mailFrom": {"email": "boss@example.org"}, "rcptTo": [{"email": "bad address"}]})).await;
        assert_eq!(r[1][1]["notCreated"]["s"]["type"], "invalidRecipients");
        let r = send(json!({"mailFrom": {"email": "boss@example.org"}, "rcptTo": []})).await;
        assert_eq!(r[1][1]["notCreated"]["s"]["type"], "noRecipients");

        let silent = Jmap::new(
            Deps {
                outbound: None,
                ..env.jmap.d.clone()
            },
            JmapConfig::default(),
        )
        .expect("jmap");
        let r = calls(
            &env,
            &silent,
            json!([
                ["Email/set", {"accountId": acc, "create": {"d": draft(&env, "user@example.org")}}, "0"],
                submit(Value::Null)
            ]),
        )
        .await;
        assert_eq!(r[1][1]["notCreated"]["s"]["type"], "forbiddenToSend");
    }

    /// EmailSubmission/query: фильтры и сортировка RFC 8621 §7.3 принимаются, результат пуст;
    /// неподдерживаемое — unsupportedFilter / unsupportedSort / anchorNotFound; без capability — unknownMethod.
    #[tokio::test]
    async fn submission_query_filters() {
        let env = Env::new().await;
        let acc = env.account();
        let q = |id: &str, args: Value| json!(["EmailSubmission/query", args, id]);
        let r = calls(
            &env,
            &env.jmap,
            json!([
                q("a", json!({"accountId": acc, "filter": {"undoStatus": "pending"}, "calculateTotal": true})),
                q("b", json!({"accountId": acc,
                    "filter": {"operator": "AND", "conditions": [
                        {"after": "2026-01-01T00:00:00Z", "before": "2027-01-01T00:00:00Z"},
                        {"operator": "NOT", "conditions": [{"identityIds": ["I1"], "emailIds": ["M1"], "threadIds": ["T1"]}]}
                    ]},
                    "sort": [{"property": "sentAt", "isAscending": false}, {"property": "emailId"}, {"property": "threadId"}],
                    "position": 5, "limit": 10})),
                q("c", json!({"accountId": acc, "filter": {"undoStatus": "sent"}})),
                q("d", json!({"accountId": acc, "filter": {"sendAt": "2026-01-01T00:00:00Z"}})),
                q("e", json!({"accountId": acc, "filter": {"before": "вчера"}})),
                q("f", json!({"accountId": acc, "sort": [{"property": "size"}]})),
                q("g", json!({"accountId": acc, "anchor": "S1"})),
            ]),
        )
        .await;
        assert_eq!(r[0][0], "EmailSubmission/query", "{:?}", r[0]);
        assert_eq!(r[0][1]["ids"], json!([]));
        assert_eq!(r[0][1]["total"], 0);
        assert_eq!(r[0][1]["position"], 0);
        assert_eq!(r[0][1]["canCalculateChanges"], true);
        assert!(r[0][1]["queryState"].is_string());
        assert_eq!(r[1][1]["ids"], json!([]), "{:?}", r[1]);
        assert_eq!(r[1][1]["position"], 0);
        assert_eq!(r[2][1]["type"], "invalidArguments");
        assert_eq!(r[3][1]["type"], "unsupportedFilter");
        assert_eq!(r[4][1]["type"], "invalidArguments");
        assert_eq!(r[5][1]["type"], "unsupportedSort");
        assert_eq!(r[6][1]["type"], "anchorNotFound");

        let req = json!({"using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"],
            "methodCalls": [q("a", json!({"accountId": acc}))]});
        let v = serde_json::to_value(
            env.jmap
                .api(&env.principal, req.to_string().as_bytes())
                .await
                .expect("api"),
        )
        .expect("json");
        assert_eq!(v["methodResponses"][0][1]["type"], "unknownMethod");
    }

    /// queryState меняется после EmailSubmission/set (письмо переносится в «Отправленные»);
    /// queryChanges от старого состояния — без изменений, чужое или будущее состояние — cannotCalculateChanges.
    #[tokio::test]
    async fn submission_query_changes() {
        let env = Env::new().await;
        let acc = env.account();
        let identity = identity_id("user@example.org");
        let drafts = env.mailbox(SpecialUse::Drafts).to_string();
        let sent = env.mailbox(SpecialUse::Sent).to_string();
        let r = calls(
            &env,
            &env.jmap,
            json!([
                ["Email/set", {"accountId": acc, "create": {"d": draft(&env, "user@example.org")}}, "0"],
                ["EmailSubmission/query", {"accountId": acc, "filter": {"undoStatus": "pending"}}, "q0"],
                ["EmailSubmission/set", {
                    "accountId": acc,
                    "create": {"s": {"identityId": identity, "emailId": "#d"}},
                    "onSuccessUpdateEmail": {"#s": {
                        format!("mailboxIds/{drafts}"): null,
                        format!("mailboxIds/{sent}"): true,
                        "keywords/$draft": null
                    }}
                }, "1"],
                ["EmailSubmission/query", {"accountId": acc, "filter": {"undoStatus": "pending"}}, "q1"]
            ]),
        )
        .await;
        assert_eq!(r[3][0], "Email/set", "{r:?}");
        let (q0, q1) = (&r[1][1]["queryState"], &r[4][1]["queryState"]);
        assert_eq!(r[4][1]["ids"], json!([]));
        assert_ne!(q0, q1, "queryState после отправки");

        let qc = |since: &Value, id: &str| {
            json!(["EmailSubmission/queryChanges", {"accountId": acc, "sinceQueryState": since,
                "filter": {"undoStatus": "pending"}, "calculateTotal": true}, id])
        };
        let r = calls(
            &env,
            &env.jmap,
            json!([
                qc(q0, "a"),
                qc(&json!("чужое"), "b"),
                qc(&json!("999999999"), "c"),
                ["EmailSubmission/queryChanges", {"accountId": acc, "sinceQueryState": q0, "maxChanges": 0}, "d"]
            ]),
        )
        .await;
        assert_eq!(r[0][0], "EmailSubmission/queryChanges", "{:?}", r[0]);
        assert_eq!(r[0][1]["oldQueryState"], *q0);
        assert_eq!(r[0][1]["newQueryState"], *q1);
        assert_eq!(r[0][1]["added"], json!([]));
        assert_eq!(r[0][1]["removed"], json!([]));
        assert_eq!(r[0][1]["total"], 0);
        assert_eq!(r[1][1]["type"], "cannotCalculateChanges");
        assert_eq!(r[2][1]["type"], "cannotCalculateChanges");
        assert_eq!(r[3][1]["type"], "invalidArguments");
    }

    /// Создаёт черновик и отправку с `sendAt`; возвращает ответ `EmailSubmission/set`.
    async fn send_at(env: &Env, at: OffsetDateTime) -> Value {
        let acc = env.account();
        let r = calls(
            env,
            &env.jmap,
            json!([
                ["Email/set", {"accountId": acc, "create": {"d": draft(env, "user@example.org")}}, "0"],
                ["EmailSubmission/set", {"accountId": acc, "create": {"s": {
                    "identityId": identity_id("user@example.org"),
                    "emailId": "#d",
                    "sendAt": utc_date(at),
                }}}, "1"]
            ]),
        )
        .await;
        assert_eq!(r[1][0], "EmailSubmission/set", "{:?}", r[1]);
        r[1][1].clone()
    }

    /// Сессия объявляет `maxDelayedSend` из конфигурации.
    #[tokio::test]
    async fn max_delayed_send_in_session() {
        let mut cfg = JmapConfig::default();
        cfg.submission.max_delayed_send = std::time::Duration::from_secs(3600);
        let env = Env::with(cfg, Vec::new()).await;
        let s = env.jmap.session(&env.principal, "https://mail.example.org");
        let caps = &s["accounts"][env.account()]["accountCapabilities"];
        assert_eq!(
            caps["urn:ietf:params:jmap:submission"]["maxDelayedSend"],
            3600
        );
    }

    /// Отправка со сроком: письмо в очереди со сроком выпуска, запись `pending` без статусов; видна в `get`,
    /// `query` и `changes`.
    #[tokio::test]
    async fn delayed_send_is_scheduled() {
        let env = Env::new().await;
        let acc = env.account();
        let at = (OffsetDateTime::now_utc() + time::Duration::hours(2))
            .replace_nanosecond(0)
            .expect("time");
        let state0 = env.submissions.state(env.scope).await.expect("state");
        let set = send_at(&env, at).await;
        let created = &set["created"]["s"];
        assert_eq!(created["undoStatus"], "pending", "{set}");
        assert_eq!(created["sendAt"], utc_date(at));
        assert_eq!(created["deliveryStatus"], Value::Null);
        assert_eq!(set["oldState"], state0.to_string());
        assert_ne!(set["newState"], set["oldState"]);
        let id = created["id"].as_str().expect("id");

        assert!(env.outbound.queued().is_empty(), "сразу не отправлено");
        let scheduled = env.outbound.scheduled();
        assert_eq!(scheduled.len(), 1);
        assert_eq!(scheduled[0].release_at, at);
        assert_eq!(
            scheduled[0].message.rcpts,
            vec!["rcpt@external.test", "hidden@external.test"]
        );
        let stored = env
            .submissions
            .get(env.scope, id)
            .await
            .expect("get")
            .expect("stored");
        assert_eq!(stored.queue, Some(scheduled[0].queue));
        assert_eq!(stored.undo_status, UndoStatus::Pending);

        let r = calls(
            &env,
            &env.jmap,
            json!([
                ["EmailSubmission/get", {"accountId": acc, "ids": [id, "S-нет"]}, "g"],
                ["EmailSubmission/query", {"accountId": acc, "filter": {"undoStatus": "pending"}}, "q"],
                ["EmailSubmission/query", {"accountId": acc, "filter": {"undoStatus": "final"}}, "qf"],
                ["EmailSubmission/changes", {"accountId": acc, "sinceState": set["oldState"]}, "c"],
                ["EmailSubmission/changes", {"accountId": acc, "sinceState": "чужое"}, "cx"],
            ]),
        )
        .await;
        let got = &r[0][1]["list"][0];
        assert_eq!(got["id"], id, "{:?}", r[0]);
        assert_eq!(got["undoStatus"], "pending");
        assert_eq!(got["sendAt"], utc_date(at));
        assert_eq!(got["identityId"], identity_id("user@example.org"));
        assert_eq!(got["envelope"]["mailFrom"]["email"], "user@example.org");
        assert_eq!(got["envelope"]["rcptTo"][0]["email"], "rcpt@external.test");
        assert!(got["threadId"].is_string() && got["emailId"].is_string());
        assert_eq!(r[0][1]["notFound"], json!(["S-нет"]));
        assert_eq!(r[0][1]["state"], set["newState"]);
        assert_eq!(r[1][1]["ids"], json!([id]));
        assert_eq!(r[1][1]["queryState"], set["newState"]);
        assert_eq!(r[2][1]["ids"], json!([]));
        assert_eq!(r[3][1]["created"], json!([id]), "{:?}", r[3]);
        assert_eq!(r[3][1]["newState"], set["newState"]);
        assert_eq!(r[4][1]["type"], "cannotCalculateChanges");
    }

    /// Отмена до срока: письмо снято с очереди, запись `canceled`, неявный `Email/set` возвращает письмо в
    /// черновики; повтор отмены — успех; удаление убирает запись.
    #[tokio::test]
    async fn delayed_send_is_canceled() {
        let env = Env::new().await;
        let acc = env.account();
        let set = send_at(&env, OffsetDateTime::now_utc() + time::Duration::minutes(5)).await;
        let id = set["created"]["s"]["id"].as_str().expect("id").to_owned();
        let cancel = json!({"accountId": acc,
            "update": {&id: {"undoStatus": "canceled"}},
            "onSuccessUpdateEmail": {&id: {"keywords/$flagged": true}}});
        let r = calls(
            &env,
            &env.jmap,
            json!([
                ["EmailSubmission/set", cancel, "u"],
                ["EmailSubmission/get", {"accountId": acc, "ids": [id]}, "g"],
                ["EmailSubmission/queryChanges", {"accountId": acc, "sinceQueryState": set["newState"],
                    "filter": {"undoStatus": "canceled"}}, "qc"],
                ["EmailSubmission/set", {"accountId": acc, "update": {&id: {"undoStatus": "canceled"}}}, "u2"],
                ["EmailSubmission/set", {"accountId": acc, "update": {
                    &id: {"undoStatus": "final"}, "S-нет": {"undoStatus": "canceled"}}}, "u3"],
                ["EmailSubmission/set", {"accountId": acc, "destroy": [id, "S-нет"]}, "d"],
                ["EmailSubmission/get", {"accountId": acc, "ids": [id]}, "g2"],
            ]),
        )
        .await;
        assert_eq!(r[0][1]["updated"], json!({&id: null}), "{:?}", r[0]);
        assert_eq!(r[1][0], "Email/set", "неявный Email/set: {:?}", r[1]);
        assert!(r[1][1]["updated"].is_object(), "{:?}", r[1]);
        assert_eq!(r[2][1]["list"][0]["undoStatus"], "canceled");
        assert_eq!(r[2][1]["list"][0]["deliveryStatus"], Value::Null);
        assert_eq!(r[3][1]["removed"], json!([id]), "{:?}", r[3]);
        assert_eq!(r[3][1]["added"], json!([{"id": id, "index": 0}]));
        assert_eq!(r[4][1]["updated"], json!({&id: null}), "повтор отмены");
        assert_eq!(r[5][1]["notUpdated"][&id]["type"], "invalidProperties");
        assert_eq!(r[5][1]["notUpdated"]["S-нет"]["type"], "notFound");
        assert_eq!(r[6][1]["destroyed"], json!([id]));
        assert_eq!(r[6][1]["notDestroyed"]["S-нет"]["type"], "notFound");
        assert_eq!(r[7][1]["notFound"], json!([id]));
        assert!(
            env.outbound.scheduled().is_empty(),
            "письмо снято с очереди"
        );
        assert!(env.outbound.queued().is_empty());
    }

    /// Отменить нельзя: письмо отправлено сразу или уже выпущено из очереди — `cannotUnsend`, выпущенная
    /// отправка становится `final`.
    #[tokio::test]
    async fn cannot_unsend() {
        let env = Env::new().await;
        let acc = env.account();
        let now = send_at(&env, OffsetDateTime::now_utc() - time::Duration::hours(1)).await;
        assert_eq!(now["created"]["s"]["undoStatus"], "final", "{now}");
        assert_eq!(env.outbound.queued().len(), 1, "срок в прошлом — сразу");
        let sent = now["created"]["s"]["id"].as_str().expect("id");
        let later = send_at(&env, OffsetDateTime::now_utc() + time::Duration::hours(1)).await;
        let released = later["created"]["s"]["id"].as_str().expect("id");
        assert!(env.outbound.release(env.outbound.scheduled()[0].queue));
        let r = calls(
            &env,
            &env.jmap,
            json!([
                ["EmailSubmission/set", {"accountId": acc, "update": {
                    sent: {"undoStatus": "canceled"}, released: {"undoStatus": "canceled"}}}, "u"],
                ["EmailSubmission/get", {"accountId": acc, "ids": [released], "properties": ["undoStatus"]}, "g"],
            ]),
        )
        .await;
        assert_eq!(
            r[0][1]["notUpdated"][sent]["type"], "cannotUnsend",
            "{:?}",
            r[0]
        );
        assert_eq!(r[0][1]["notUpdated"][released]["type"], "cannotUnsend");
        assert_eq!(r[1][1]["list"][0]["undoStatus"], "final");
    }

    /// Срок дальше `maxDelayedSend`, не дата, чужой `undoStatus` — `invalidProperties`; при `maxDelayedSend` = 0
    /// отложенная отправка запрещена. Ничего не отправлено и не записано.
    #[tokio::test]
    async fn delayed_send_limits() {
        let mut cfg = JmapConfig::default();
        cfg.submission.max_delayed_send = std::time::Duration::from_secs(3600);
        let env = Env::with(cfg, Vec::new()).await;
        let acc = env.account();
        let far = send_at(&env, OffsetDateTime::now_utc() + time::Duration::hours(2)).await;
        assert_eq!(far["notCreated"]["s"]["type"], "invalidProperties", "{far}");
        assert_eq!(far["notCreated"]["s"]["properties"], json!(["sendAt"]));
        let identity = identity_id("user@example.org");
        let soon = utc_date(OffsetDateTime::now_utc() + time::Duration::minutes(10));
        let r = calls(
            &env,
            &env.jmap,
            json!([
                ["Email/set", {"accountId": acc, "create": {"d": draft(&env, "user@example.org")}}, "0"],
                ["EmailSubmission/set", {"accountId": acc, "create": {
                    "a": {"identityId": identity, "emailId": "#d", "sendAt": "завтра"},
                    "b": {"identityId": identity, "emailId": "#d", "sendAt": soon, "undoStatus": "final"},
                    "c": {"identityId": identity, "emailId": "#d", "undoStatus": "pending"},
                }}, "1"]
            ]),
        )
        .await;
        for cid in ["a", "b", "c"] {
            assert_eq!(
                r[1][1]["notCreated"][cid]["type"], "invalidProperties",
                "{cid}: {:?}",
                r[1]
            );
        }
        assert!(env.outbound.scheduled().is_empty() && env.outbound.queued().is_empty());
        assert!(
            env.submissions
                .list(env.scope)
                .await
                .expect("list")
                .is_empty()
        );

        let mut cfg = JmapConfig::default();
        cfg.submission.max_delayed_send = std::time::Duration::ZERO;
        let env = Env::with(cfg, Vec::new()).await;
        let off = send_at(&env, OffsetDateTime::now_utc() + time::Duration::minutes(1)).await;
        assert_eq!(off["notCreated"]["s"]["type"], "invalidProperties", "{off}");
    }

    /// Отправленное сразу письмо записано: `final` со статусами получателей; сортировка и фильтры `query`.
    #[tokio::test]
    async fn sent_submissions_are_stored() {
        let env = Env::new().await;
        let acc = env.account();
        let first = send_at(&env, OffsetDateTime::now_utc() + time::Duration::hours(3)).await;
        let second = send_at(&env, OffsetDateTime::now_utc() - time::Duration::seconds(1)).await;
        let (a, b) = (
            first["created"]["s"]["id"].as_str().expect("id"),
            second["created"]["s"]["id"].as_str().expect("id"),
        );
        let r = calls(
            &env,
            &env.jmap,
            json!([
                ["EmailSubmission/get", {"accountId": acc, "ids": null}, "g"],
                ["EmailSubmission/query", {"accountId": acc, "sort": [{"property": "sentAt", "isAscending": false}]}, "q"],
                ["EmailSubmission/query", {"accountId": acc, "filter": {"operator": "NOT",
                    "conditions": [{"undoStatus": "pending"}]}}, "qn"],
                ["EmailSubmission/get", {"accountId": acc, "#ids": {
                    "resultOf": "qn", "name": "EmailSubmission/query", "path": "/ids"}}, "g2"],
            ]),
        )
        .await;
        assert_eq!(
            r[0][1]["list"].as_array().map(Vec::len),
            Some(2),
            "{:?}",
            r[0]
        );
        assert_eq!(r[1][1]["ids"], json!([a, b]));
        assert_eq!(r[2][1]["ids"], json!([b]));
        let sent = &r[3][1]["list"][0];
        assert_eq!(sent["undoStatus"], "final");
        assert_eq!(
            sent["deliveryStatus"]["rcpt@external.test"]["delivered"], "queued",
            "{sent}"
        );
        let thread = sent["threadId"].as_str().expect("thread");
        let by_thread = calls(
            &env,
            &env.jmap,
            json!([["EmailSubmission/query", {"accountId": acc, "filter": {"threadIds": [thread]},
                "sort": [{"property": "threadId"}]}, "t"]]),
        )
        .await;
        assert_eq!(by_thread[0][1]["ids"], json!([b]), "{:?}", by_thread[0]);
    }

    /// Событие журнала доставки записи очереди `queue`.
    fn event(
        env: &Env,
        queue: tp_contracts::QueueId,
        kind: DeliveryEventKind,
        decision: DeliveryDecision,
        rcpt: &str,
        reason: Option<&str>,
    ) -> DeliveryEvent {
        DeliveryEvent {
            tenant: env.scope.tenant,
            at: OffsetDateTime::now_utc(),
            node: tp_contracts::membership::NodeId::try_from("node-1".to_owned()).expect("node"),
            kind,
            sender: "user@example.org".into(),
            recipients: vec![rcpt.to_owned()],
            message_id: None,
            queue_id: Some(queue),
            decision,
            reason: reason.map(str::to_owned),
        }
    }

    /// Статус доставки из журнала: наступившая отложенная отправка становится `final`, статусы получателей — по
    /// событиям её записи очереди; каждое изменение меняет состояние и видно в `changes`; после конечных статусов
    /// состояние не меняется.
    #[tokio::test]
    async fn delivery_status_from_log() {
        use tp_contracts::delivery::DeliveryLog;
        let env = Env::new().await;
        let acc = env.account();
        let queue = tp_contracts::QueueId::new();
        let rcpts = [
            "local@example.org",
            "slow@external.test",
            "Far@external.test",
        ];
        let state0 = env
            .submissions
            .insert(
                env.scope,
                EmailSubmission {
                    id: "S1".into(),
                    identity: identity_id("user@example.org"),
                    email: MessageId::new(),
                    mail_from: "user@example.org".into(),
                    rcpt_to: rcpts.iter().map(|r| (*r).to_owned()).collect(),
                    send_at: OffsetDateTime::now_utc() - time::Duration::minutes(1),
                    undo_status: UndoStatus::Pending,
                    delivery: Vec::new(),
                    queue: Some(queue),
                },
            )
            .await
            .expect("insert");
        let get = || async {
            let r = calls(
                &env,
                &env.jmap,
                json!([["EmailSubmission/get", {"accountId": acc, "ids": ["S1"]}, "g"]]),
            )
            .await;
            r[0][1].clone()
        };
        let delivered =
            |g: &Value, rcpt: &str| g["list"][0]["deliveryStatus"][rcpt]["delivered"].clone();

        // Срок наступил, событий ещё нет: окончательная, все получатели в очереди.
        let g1 = get().await;
        assert_eq!(g1["list"][0]["undoStatus"], "final", "{g1}");
        assert!(rcpts.iter().all(|r| delivered(&g1, r) == "queued"), "{g1}");
        assert_ne!(g1["state"], state0.to_string());
        let cancel = calls(
            &env,
            &env.jmap,
            json!([["EmailSubmission/set", {"accountId": acc, "update": {"S1": {"undoStatus": "canceled"}}}, "u"]]),
        )
        .await;
        assert_eq!(cancel[0][1]["notUpdated"]["S1"]["type"], "cannotUnsend");

        // Доставка в ящик и временный отказ; приём письма и уведомление DSN статус не меняют.
        for e in [
            event(
                &env,
                queue,
                DeliveryEventKind::Receive,
                DeliveryDecision::Accept,
                rcpts[1],
                None,
            ),
            event(
                &env,
                queue,
                DeliveryEventKind::Deliver,
                DeliveryDecision::Accept,
                rcpts[0],
                None,
            ),
            event(
                &env,
                queue,
                DeliveryEventKind::Relay,
                DeliveryDecision::Defer,
                rcpts[1],
                Some("451 4.7.1 Try later"),
            ),
            event(
                &env,
                queue,
                DeliveryEventKind::Dsn,
                DeliveryDecision::Reject,
                rcpts[0],
                None,
            ),
        ] {
            env.delivery_log.record(e).await.expect("record");
        }
        let g2 = get().await;
        assert_eq!(delivered(&g2, rcpts[0]), "yes", "{g2}");
        assert_eq!(delivered(&g2, rcpts[1]), "queued");
        assert_eq!(delivered(&g2, rcpts[2]), "queued");
        assert_ne!(g2["state"], g1["state"]);
        let c = calls(
            &env,
            &env.jmap,
            json!([["EmailSubmission/changes", {"accountId": acc, "sinceState": g1["state"]}, "c"]]),
        )
        .await;
        assert_eq!(c[0][1]["updated"], json!(["S1"]), "{:?}", c[0]);

        // Отказ с ответом сервера и приём внешним сервером (адрес — без учёта регистра).
        for e in [
            event(
                &env,
                queue,
                DeliveryEventKind::Relay,
                DeliveryDecision::Reject,
                rcpts[1],
                Some("550 5.1.1 No such user"),
            ),
            event(
                &env,
                queue,
                DeliveryEventKind::Relay,
                DeliveryDecision::Accept,
                "far@external.test",
                Some("250 2.0.0 OK"),
            ),
        ] {
            env.delivery_log.record(e).await.expect("record");
        }
        // Изменение видно и без чтения объекта: `changes` сверяет сам.
        let c = calls(
            &env,
            &env.jmap,
            json!([["EmailSubmission/changes", {"accountId": acc, "sinceState": g2["state"]}, "c"]]),
        )
        .await;
        assert_eq!(c[0][1]["updated"], json!(["S1"]), "{:?}", c[0]);
        let g3 = get().await;
        let status = &g3["list"][0]["deliveryStatus"];
        assert_eq!(status[rcpts[1]]["delivered"], "no", "{g3}");
        assert_eq!(status[rcpts[1]]["smtpReply"], "550 5.1.1 No such user");
        assert_eq!(status[rcpts[2]]["delivered"], "yes");
        assert_eq!(g3["state"], c[0][1]["newState"]);

        // Все получатели в конечном статусе: журнал больше не читается, состояние не меняется.
        env.delivery_log
            .record(event(
                &env,
                queue,
                DeliveryEventKind::Relay,
                DeliveryDecision::Defer,
                rcpts[0],
                None,
            ))
            .await
            .expect("record");
        let g4 = get().await;
        assert_eq!(g4["state"], g3["state"]);
        assert_eq!(delivered(&g4, rcpts[0]), "yes");
    }

    /// Ждущая срока и отменённая отправки с журналом не сверяются; состояние отправок — в push вместе с папками.
    #[tokio::test]
    async fn pending_is_not_tracked_and_push_state() {
        use tp_contracts::bus::EventKind;
        use tp_contracts::delivery::DeliveryLog;
        let env = Env::new().await;
        let set = send_at(&env, OffsetDateTime::now_utc() + time::Duration::hours(1)).await;
        let queue = env.outbound.scheduled()[0].queue;
        env.delivery_log
            .record(event(
                &env,
                queue,
                DeliveryEventKind::Deliver,
                DeliveryDecision::Accept,
                "rcpt@external.test",
                None,
            ))
            .await
            .expect("record");
        let state = synced_state(&env.jmap, env.scope).await.expect("state");
        assert_eq!(set["newState"], state, "отправка ждёт срока — не сверяется");
        let stored = env.submissions.list(env.scope).await.expect("list");
        assert_eq!(stored[0].undo_status, UndoStatus::Pending);
        let kind = EventKind::MailboxChanged {
            mailbox: env.mailbox(SpecialUse::Sent),
            modseq: ModSeq(1),
        };
        assert!(crate::push::changed_types(&kind).contains(&"EmailSubmission"));
    }

    #[test]
    fn strip_bcc_with_continuation() {
        let raw = b"From: a@x\r\nBcc: b@x,\r\n c@x\r\nTo: d@x\r\n\r\nBcc: body\r\n";
        assert_eq!(
            strip_header(raw, "bcc"),
            b"From: a@x\r\nTo: d@x\r\n\r\nBcc: body\r\n".to_vec()
        );
    }

    #[test]
    fn addresses() {
        assert!(valid_address("a@b.c"));
        assert!(!valid_address("a b@c"));
        assert!(!valid_address("@c"));
        assert!(!valid_address("a@"));
        assert!(!valid_address("<a@b>"));
    }

    /// Заглушка правил потока почты: записывает входы (с телом из BLOB) и возвращает заданный итог;
    /// `rewrite` — изменённые конверт и тело (новый объект BLOB).
    struct StubFlow {
        blobs: crate::testenv::Unpacking,
        verdict: FlowVerdict,
        rewrite: bool,
        inputs: std::sync::Mutex<Vec<(FlowInput, Vec<u8>)>>,
    }

    impl StubFlow {
        fn new(env: &Env, verdict: FlowVerdict, rewrite: bool) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                blobs: env.blobs.clone(),
                verdict,
                rewrite,
                inputs: Default::default(),
            })
        }

        fn inputs(&self) -> Vec<(FlowInput, Vec<u8>)> {
            self.inputs.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl tp_contracts::transport::MailFlow for StubFlow {
        async fn apply(&self, input: &FlowInput) -> Result<tp_contracts::transport::FlowOutcome> {
            use tp_contracts::blob::BlobStore;
            let body = collect(self.blobs.get(&input.message.key, None).await?).await?;
            self.inputs
                .lock()
                .expect("lock")
                .push((input.clone(), body.clone()));
            let mut out = tp_contracts::transport::FlowOutcome::unchanged(input);
            out.verdict = self.verdict.clone();
            out.matched = vec!["rule-1".into()];
            if self.rewrite {
                use sha2::Digest;
                let end = header_end(&body);
                let mut changed = b"X-Flow: disclaimer\r\n".to_vec();
                changed.extend_from_slice(&body[..end]);
                changed.extend_from_slice(b"-- disclaimer --\r\n");
                changed.extend_from_slice(&body[end..]);
                let key = tp_contracts::BlobKey {
                    tenant: input.tenant,
                    hash: tp_contracts::BlobHash(sha2::Sha256::digest(&changed).into()),
                    generation: tp_contracts::Gen(1),
                };
                self.blobs.put(&key, Bytes::from(changed.clone())).await?;
                out.message = Some(BlobRef {
                    key,
                    size: changed.len() as u64,
                });
                out.envelope.rcpt_to = vec!["copy@external.test".into()];
            }
            Ok(out)
        }
    }

    fn with_flow(env: &Env, flow: std::sync::Arc<StubFlow>) -> Jmap {
        Jmap::new(env.jmap.d.clone(), JmapConfig::default())
            .expect("jmap")
            .with_mail_flow(Some(flow))
    }

    async fn submit(env: &Env, jmap: &Jmap) -> Vec<Value> {
        let acc = env.account();
        calls(
            env,
            jmap,
            json!([
                ["Email/set", {"accountId": acc, "create": {"d": draft(env, "user@example.org")}}, "0"],
                ["EmailSubmission/set", {"accountId": acc, "create": {"s": {
                    "identityId": identity_id("user@example.org"), "emailId": "#d"
                }}}, "1"]
            ]),
        )
        .await
    }

    /// Правило потока отклоняет письмо: `forbiddenToSend` с текстом правила, очередь и точки принятия решений
    /// не вызываются; вход правил — отправка пользователя с конвертом и телом без Bcc.
    #[tokio::test]
    async fn mail_flow_reject() {
        let env = Env::new().await;
        let flow = StubFlow::new(
            &env,
            FlowVerdict::Reject("Внешняя отправка запрещена".into()),
            false,
        );
        let jmap = with_flow(&env, flow.clone());
        let r = submit(&env, &jmap).await;
        let e = &r[1][1]["notCreated"]["s"];
        assert_eq!(e["type"], "forbiddenToSend", "{r:?}");
        assert_eq!(e["description"], "Внешняя отправка запрещена");
        assert!(env.outbound.queued().is_empty());
        assert!(
            !env.hook
                .calls()
                .iter()
                .any(|c| matches!(c, tp_testkit::fakes::HookCall::Submission(_))),
            "точки принятия решений — после правил потока"
        );
        let inputs = flow.inputs();
        assert_eq!(inputs.len(), 1, "один вызов на письмо");
        let (input, body) = &inputs[0];
        assert_eq!(input.tenant, env.scope.tenant);
        assert_eq!(input.direction, FlowDirection::Submission);
        assert_eq!(input.principal.as_ref(), Some(&env.principal));
        assert_eq!(input.source, None);
        assert_eq!(input.envelope.mail_from, "user@example.org");
        assert_eq!(
            input.envelope.rcpt_to,
            vec!["rcpt@external.test", "hidden@external.test"]
        );
        assert_eq!(input.message.size, body.len() as u64);
        let text = String::from_utf8_lossy(body);
        assert!(text.contains("Subject: "), "{text}");
        assert!(!text.contains("hidden@external.test"), "{text}");
    }

    /// Изменённые правилом тело и конверт уходят в очередь и в точку принятия решений вместо исходных.
    #[tokio::test]
    async fn mail_flow_changed_body() {
        let env = Env::new().await;
        let flow = StubFlow::new(&env, FlowVerdict::Accept, true);
        let jmap = with_flow(&env, flow.clone());
        let r = submit(&env, &jmap).await;
        assert_eq!(
            r[1][1]["created"]["s"]["deliveryStatus"],
            json!({"copy@external.test": {"smtpReply": "250 2.0.0 Queued", "delivered": "queued", "displayed": "unknown"}}),
            "{r:?}"
        );
        let q = env.outbound.queued();
        assert_eq!(q.len(), 1);
        assert_eq!(q[0].rcpts, vec!["copy@external.test"]);
        let raw = String::from_utf8_lossy(&q[0].raw);
        assert!(raw.starts_with("X-Flow: disclaimer\r\n"), "{raw}");
        assert!(raw.contains("-- disclaimer --\r\n"), "{raw}");
        let sub = env
            .hook
            .calls()
            .into_iter()
            .find_map(|c| match c {
                tp_testkit::fakes::HookCall::Submission(i) => Some(i),
                _ => None,
            })
            .expect("on_submission");
        assert_eq!(sub.rcpt_to, vec!["copy@external.test"]);
        assert_eq!(sub.size, q[0].raw.len() as u64);
        assert_eq!(flow.inputs().len(), 1);
    }

    /// `Drop` — принято без отправки, получатели `queued`; модерация — `forbiddenToSend`.
    #[tokio::test]
    async fn mail_flow_drop_and_moderate() {
        let env = Env::new().await;
        let jmap = with_flow(&env, StubFlow::new(&env, FlowVerdict::Drop, false));
        let r = submit(&env, &jmap).await;
        let ds = &r[1][1]["created"]["s"]["deliveryStatus"];
        assert_eq!(ds["rcpt@external.test"]["delivered"], "queued", "{r:?}");
        assert_eq!(ds["hidden@external.test"]["delivered"], "queued", "{r:?}");
        assert!(env.outbound.queued().is_empty());

        let moderate = FlowVerdict::Moderate {
            moderators: vec![env.scope.account],
        };
        let jmap = with_flow(&env, StubFlow::new(&env, moderate, false));
        let r = submit(&env, &jmap).await;
        assert_eq!(
            r[1][1]["notCreated"]["s"]["type"], "forbiddenToSend",
            "{r:?}"
        );
        assert!(env.outbound.queued().is_empty());
    }
}
