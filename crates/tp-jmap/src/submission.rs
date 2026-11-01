//! Identity и EmailSubmission (RFC 8621 §6, §7).
//!
//! - Identity — адрес учётной записи (имя входа) с отображаемым именем из `AccountStore`; адреса и подписи
//!   ведёт каталог, поэтому `Identity/set` отвечает `forbidden`.
//! - `EmailSubmission/set` ставит письмо в очередь исходящей почты ([`Outbound`]) сразу (`undoStatus` = `final`,
//!   `maxDelayedSend` = 0). Адрес `From` проверяет [`Authorizer::send_right`](tp_contracts::auth::Authorizer::send_right)
//!   (свои адреса, Send As, Send on Behalf — с заголовком `Sender`). До очереди — `DecisionHooks::on_submission`
//!   (`Deny` — `forbiddenToSend`; дополнения: заголовки в начало письма, копии в папки и внешним адресам).
//!   Заголовок `Bcc` из отправляемого письма удаляется. Объекты EmailSubmission после ответа не хранятся:
//!   `EmailSubmission/get` их не находит, отменить отправку нельзя (`cannotUnsend`).

use std::collections::HashSet;
use std::str::FromStr;

use async_trait::async_trait;
use axum::body::Bytes;
use mail_parser::MessageParser;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use tp_contracts::auth::SendRight;
use tp_contracts::ext::{CopyTo, Decision, SubmissionInfo};
use tp_contracts::mailstore::IngestRequest;
use tp_contracts::{MessageId, Result, TenantId};

use crate::core::error::{MethodError, MethodResult, set_error};
use crate::core::request::{GetArgs, object, parse_args, select, state_string};
use crate::core::{Ctx, digest};
use crate::email_get::{collect, utc_date};

/// Очередь исходящей почты для `EmailSubmission/set`. Сигнатура совпадает с `tp_smtp::deliver::Outbound`:
/// приложение подключает очередь smtp (`tp_smtp::queue::Queue`, подпись DKIM при постановке) через обёртку.
#[async_trait]
pub trait Outbound: Send + Sync + 'static {
    /// Ставит письмо `raw` от `mail_from` получателям `rcpts` организации `tenant`.
    async fn enqueue(
        &self,
        tenant: TenantId,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<()>;
}

fn or_null(m: Map<String, Value>) -> Value {
    if m.is_empty() {
        Value::Null
    } else {
        Value::Object(m)
    }
}

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

/// `EmailSubmission/get`: отправки после ответа `EmailSubmission/set` не хранятся.
pub(crate) async fn submission_get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let state = state_string(ctx.state().await?);
    Ok(object(json!({
        "accountId": a.account_id,
        "state": state,
        "list": [],
        "notFound": a.ids.unwrap_or_default(),
    })))
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

/// Внешние адреса пересылки (`Ingested::forward`, копии `CopyTo::Address`) — в очередь исходящей почты.
pub(crate) async fn forward(
    ctx: &Ctx<'_>,
    mail_from: &str,
    raw: &[u8],
    to: &[String],
) -> MethodResult<()> {
    if to.is_empty() {
        return Ok(());
    }
    let Some(out) = &ctx.jmap.d.outbound else {
        tracing::warn!(
            count = to.len(),
            "jmap: копии внешним адресам не отправлены — узел без очереди"
        );
        return Ok(());
    };
    out.enqueue(ctx.scope.tenant, mail_from, to, Bytes::copy_from_slice(raw))
        .await?;
    Ok(())
}

/// Одна отправка: `Ok(Ok((id, письмо)))` — в очереди, `Ok(Err(SetError))` — отказ по объекту.
async fn submit_one(
    ctx: &Ctx<'_>,
    obj: Value,
) -> MethodResult<std::result::Result<(String, MessageId), Value>> {
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
    if c.undo_status.as_deref().is_some_and(|u| u != "final") {
        return Ok(Err(set_error(
            "invalidProperties",
            Some("delayed send is not supported"),
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
    if ctx.resolve_id(&c.identity_id) != identity_id(&p.login) {
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

    raw = prepend_headers(strip_header(&raw, "Bcc"), &add);
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
    outbound
        .enqueue(p.tenant, &mail_from, &rcpts, Bytes::copy_from_slice(&raw))
        .await?;
    let mut external = Vec::new();
    let mut boxes = Vec::new();
    for c in copies {
        match c {
            CopyTo::Address(a) => external.push(a),
            CopyTo::Mailbox(delivery) => boxes.push(delivery),
        }
    }
    forward(ctx, &mail_from, &raw, &external).await?;
    if !boxes.is_empty() {
        let data = Bytes::from(raw);
        d.store
            .ingest(IngestRequest {
                tenant: p.tenant,
                mail_from: mail_from.clone(),
                raw: Box::pin(futures::stream::once(async move { Ok(data) })),
                deliveries: boxes,
                received_at: OffsetDateTime::now_utc(),
            })
            .await?;
    }
    Ok(Ok((format!("S{}", MessageId::new()), msg.id)))
}

/// `EmailSubmission/set`: создание — постановка в очередь; затем неявный `Email/set`
/// (`onSuccessUpdateEmail`, `onSuccessDestroyEmail`) — ответ после ответа метода.
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
    let old = state_string(ctx.state().await?);
    if a.if_in_state.as_ref().is_some_and(|s| *s != old) {
        return Err(MethodError::new("stateMismatch"));
    }
    let (mut created, mut not_created) = (Map::new(), Map::new());
    // Успешные отправки: (creation id, id отправки, письмо).
    let mut sent: Vec<(String, String, MessageId)> = Vec::new();
    for (cid, obj) in a.create.unwrap_or_default() {
        match submit_one(ctx, obj).await? {
            Ok((id, email)) => {
                created.insert(
                    cid.clone(),
                    json!({
                        "id": id,
                        "undoStatus": "final",
                        "sendAt": utc_date(OffsetDateTime::now_utc()),
                    }),
                );
                ctx.created.insert(cid.clone(), id.clone());
                sent.push((cid, id, email));
            }
            Err(e) => {
                not_created.insert(cid, e);
            }
        }
    }
    let mut not_updated = Map::new();
    for (id, _) in a.update.unwrap_or_default() {
        not_updated.insert(id, set_error("cannotUnsend", None, &[]));
    }
    let mut not_destroyed = Map::new();
    for id in a.destroy.unwrap_or_default() {
        not_destroyed.insert(id, set_error("notFound", None, &[]));
    }
    let email_of = |key: &str| {
        sent.iter()
            .find(|(cid, id, _)| key.strip_prefix('#') == Some(cid.as_str()) || key == id)
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
    let new = state_string(ctx.state().await?);
    let out = object(json!({
        "accountId": a.account_id,
        "oldState": old,
        "newState": new,
        "created": or_null(created),
        "updated": null,
        "destroyed": null,
        "notCreated": or_null(not_created),
        "notUpdated": or_null(not_updated),
        "notDestroyed": or_null(not_destroyed),
    }));
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

    use tp_contracts::meta::SpecialUse;

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
        let moved = r[2][1]["updated"][email]["id"].as_str().expect("moved");
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
}
