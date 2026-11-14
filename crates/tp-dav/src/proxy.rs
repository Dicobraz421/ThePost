//! Представители — CalendarServer `calendar-proxy` (DAV-18; SRS 4.5; CONTRACTS.md#Sharing).
//!
//! У принципала владельца — группы `calendar-proxy-read/` и `calendar-proxy-write/` (`group-member-set` —
//! представители уровня [`DelegateLevel::Read`] и [`DelegateLevel::Write`], [`DelegateStore`]). Владелец меняет состав
//! `PROPPATCH` `group-member-set` своей группы (Календарь macOS и iOS, «Делегирование»): получатель — ссылка на
//! принципал или `mailto:` адрес учётной записи организации; проверка — [`SharingAuthorizer::check_delegate`]. Новый
//! представитель с записью планирует от имени владельца (`schedule_on_behalf`), с чтением — нет; у прежнего
//! представителя того же уровня признак сохраняется (меняется через JMAP и Admin API).
//!
//! Свой принципал публикует `calendar-proxy-read-for`, `calendar-proxy-write-for` и `group-membership` —
//! владельцев, представителем которых является вошедший ([`SharingAuthorizer::delegations`]): клиент открывает их дома
//! календарей, права на календари — [`SharingAuthorizer::rights`] (представительство в них уже учтено). Приглашения и
//! ответы представителя — от адреса владельца с `SENT-BY` ([`crate::schedule::sent_by`]).
//!
//! [`DelegateLevel::Read`]: tp_contracts::sharing::DelegateLevel::Read
//! [`DelegateLevel::Write`]: tp_contracts::sharing::DelegateLevel::Write
//! [`DelegateStore`]: tp_contracts::sharing::DelegateStore
//! [`SharingAuthorizer::check_delegate`]: tp_contracts::sharing::SharingAuthorizer::check_delegate
//! [`SharingAuthorizer::delegations`]: tp_contracts::sharing::SharingAuthorizer::delegations
//! [`SharingAuthorizer::rights`]: tp_contracts::sharing::SharingAuthorizer::rights

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use tp_contracts::sharing::{Delegate, DelegateLevel};
use tp_contracts::{AccountId, Error, Result};

use crate::path::Target;
use crate::props::{self, Res, Want};
use crate::server::{Cx, multistatus, status};
use crate::share;
use crate::xml::{DAV, Element, Multistatus, PropName};

/// Уровень группы представителей.
fn level(write: bool) -> DelegateLevel {
    if write {
        DelegateLevel::Write
    } else {
        DelegateLevel::Read
    }
}

/// Логины учётных записей `ids` организации вошедшего по идентификатору (отключённые и удалённые пропускаются).
async fn logins(cx: &Cx<'_>, ids: &[AccountId]) -> Result<BTreeMap<AccountId, String>> {
    let Some(sharing) = cx.dav.sharing() else {
        return Ok(BTreeMap::new());
    };
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    Ok(sharing
        .accounts
        .by_ids(cx.scope.tenant, ids)
        .await?
        .into_iter()
        .filter(|a| a.enabled)
        .map(|a| (a.id, a.login))
        .collect())
}

/// Владельцы, представителем которых является вошедший: логин и запись (`calendar-proxy-*-for`).
pub(crate) async fn proxy_for(cx: &Cx<'_>) -> Result<Vec<(String, bool)>> {
    let Some(sharing) = cx.dav.sharing() else {
        return Ok(Vec::new());
    };
    let list = sharing.authz.delegations(cx.principal).await?;
    let ids: Vec<AccountId> = list.iter().map(|d| d.owner).collect();
    let names = logins(cx, &ids).await?;
    Ok(list
        .iter()
        .filter_map(|d| {
            let login = names.get(&d.owner)?;
            Some((login.clone(), d.level == DelegateLevel::Write))
        })
        .collect())
}

/// Логины представителей владельца ресурса запроса уровня `write`.
pub(crate) async fn members(cx: &Cx<'_>, write: bool) -> Result<Vec<String>> {
    let Some(sharing) = cx.dav.sharing() else {
        return Ok(Vec::new());
    };
    let Some(stores) = sharing.stores else {
        return Ok(Vec::new());
    };
    let list = stores.delegates.delegates(cx.scope).await?;
    let ids: Vec<AccountId> = list
        .iter()
        .filter(|d| d.level == level(write))
        .map(|d| d.delegate)
        .collect();
    Ok(logins(cx, &ids).await?.into_values().collect())
}

/// Группа представителей владельца в ответ `PROPFIND`.
pub(crate) async fn emit_group(
    cx: &Cx<'_>,
    ms: &mut Multistatus,
    write: bool,
    want: &Want,
) -> Result<()> {
    let list = members(cx, write).await?;
    let res = Res::Proxy {
        owner: cx.login(),
        write,
        members: &list,
    };
    props::emit(cx, ms, &res, want);
    Ok(())
}

/// `PROPPATCH` группы представителей: `group-member-set` (установка — новый состав группы, удаление — пустой).
/// Неизвестный получатель или сам владелец — 409, назначение не разрешено — 403; ничего не записано.
pub(crate) async fn proppatch(cx: &Cx<'_>, target: &Target, body: Body) -> Result<Response> {
    let Target::Proxy(_, write) = target else {
        return Ok(status(StatusCode::FORBIDDEN));
    };
    if cx.dav.sharing().is_none_or(|s| s.stores.is_none()) {
        return Ok(status(StatusCode::FORBIDDEN));
    }
    let write = *write;
    let el = match cx.xml(body).await {
        Ok(el) => el,
        Err(code) => return Ok(status(code)),
    };
    let Some(ops) = el.as_ref().and_then(props::updates) else {
        return Ok(status(StatusCode::BAD_REQUEST));
    };
    let member_set = PropName::new(DAV, "group-member-set");
    let mut wanted: Option<Vec<AccountId>> = None;
    let mut results: Vec<(PropName, StatusCode)> = Vec::new();
    for (name, value) in &ops {
        if *name != member_set {
            results.push((name.clone(), StatusCode::FORBIDDEN));
            continue;
        }
        match resolve(cx, *value).await? {
            Some(ids) => {
                wanted = Some(ids);
                results.push((name.clone(), StatusCode::OK));
            }
            None => results.push((name.clone(), StatusCode::CONFLICT)),
        }
    }
    if results.iter().all(|(_, s)| *s == StatusCode::OK)
        && let Some(ids) = wanted
    {
        match apply(cx, write, &ids).await {
            Ok(()) => {}
            Err(Error::Forbidden) => {
                for (_, s) in &mut results {
                    *s = StatusCode::FORBIDDEN;
                }
            }
            Err(Error::Invalid(_) | Error::NotFound) => {
                for (_, s) in &mut results {
                    *s = StatusCode::CONFLICT;
                }
            }
            Err(e) => return Err(e),
        }
    } else {
        for (_, s) in &mut results {
            if *s == StatusCode::OK {
                *s = StatusCode::FAILED_DEPENDENCY;
            }
        }
    }
    let mut ms = Multistatus::new();
    ms.props(&target.href(cx.login()), &props::status_groups(results));
    Ok(multistatus(ms))
}

/// Учётные записи из значения `group-member-set` (`None` у удаления — пустой состав); неизвестная ссылка или
/// владелец — `None`.
async fn resolve(cx: &Cx<'_>, value: Option<&Element>) -> Result<Option<Vec<AccountId>>> {
    let Some(sharing) = cx.dav.sharing() else {
        return Ok(None);
    };
    let Some(value) = value else {
        return Ok(Some(Vec::new()));
    };
    let mut ids = Vec::new();
    for h in value.children.iter().filter(|c| c.is(DAV, "href")) {
        let Some(a) = share::login_of(sharing.accounts, cx.scope.tenant, h.trimmed()).await? else {
            return Ok(None);
        };
        if a.id == cx.scope.account || !a.enabled {
            return Ok(None);
        }
        if !ids.contains(&a.id) {
            ids.push(a.id);
        }
    }
    Ok(Some(ids))
}

/// Состав группы уровня `write` — `ids`: новые и сменившие уровень назначаются, прежние этого уровня вне списка
/// отзываются. Права проверяются для всех до записи.
async fn apply(cx: &Cx<'_>, write: bool, ids: &[AccountId]) -> Result<()> {
    let Some(sharing) = cx.dav.sharing() else {
        return Err(Error::Forbidden);
    };
    let lvl = level(write);
    let Some(stores) = sharing.stores else {
        return Err(Error::Forbidden);
    };
    let current = stores.delegates.delegates(cx.scope).await?;
    let mut set = Vec::new();
    for id in ids {
        let prev = current.iter().find(|d| d.delegate == *id);
        if prev.is_some_and(|d| d.level == lvl) {
            continue;
        }
        let d = Delegate {
            delegate: *id,
            level: lvl,
            schedule_on_behalf: write,
        };
        sharing
            .authz
            .check_delegate(cx.principal, cx.scope.account, &d)
            .await?;
        set.push(d);
    }
    let remove: Vec<AccountId> = current
        .iter()
        .filter(|d| d.level == lvl && !ids.contains(&d.delegate))
        .map(|d| d.delegate)
        .collect();
    if !remove.is_empty() && !cx.own() {
        return Err(Error::Forbidden);
    }
    for d in set {
        stores.delegates.set_delegate(cx.scope, d).await?;
    }
    for id in remove {
        stores.delegates.remove_delegate(cx.scope, id).await?;
    }
    tracing::info!(write, "DAV: представители изменены");
    Ok(())
}
