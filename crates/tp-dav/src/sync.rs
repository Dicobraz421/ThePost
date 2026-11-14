//! `REPORT sync-collection` (RFC 6578): изменения коллекции после токена.
//!
//! Токен — URI `urn:thepost:sync:<ModSeq>` (состояние коллекции, оно же `getctag`); пустой — начальная
//! синхронизация (все ресурсы). Изменённый ресурс — `propstat` с запрошенными свойствами, удалённый — `404`.
//! Изменений больше `limit` (`nresults` клиента, не больше `dav.max_response_items`) — `507` с
//! `number-of-matches-within-limits` для коллекции и токен продолжения. Неизвестный, чужой или устаревший токен
//! (надгробия очищены: `Conflict` хранилища) — 403 `valid-sync-token`; `sync-level` кроме `1` — 403
//! `sync-traversal-supported`. Отчёты CalDAV — [`crate::query`], CardDAV — [`crate::card`]; остальные — 403
//! `supported-report`.

use std::collections::HashMap;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use tp_contracts::dav::DavObject;
use tp_contracts::{Error, ModSeq, Result};

use crate::card;
use crate::path::{self, Target};
use crate::props::{self, Res, SYNC_PREFIX, Want};
use crate::query::{self, unsupported};
use crate::server::{Cx, multistatus, precondition, status};
use crate::xml::{CALDAV, CARDDAV, DAV, Element, Multistatus, PropName};

/// Разобранный запрос `sync-collection`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SyncRequest {
    /// Состояние после токена; `ModSeq::ZERO` — начальная синхронизация.
    pub since: ModSeq,
    /// `nresults` клиента.
    pub limit: Option<usize>,
    /// Запрошенные свойства.
    pub props: Vec<PropName>,
}

/// Ошибка запроса `sync-collection`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncError {
    /// Не `sync-collection`.
    Report,
    /// Неверный токен.
    Token,
    /// `sync-level` не `1`.
    Level,
    /// Неверный `limit`.
    Limit,
}

impl SyncRequest {
    pub(crate) fn parse(root: &Element) -> std::result::Result<SyncRequest, SyncError> {
        if !root.is(DAV, "sync-collection") {
            return Err(SyncError::Report);
        }
        let token = root.child(DAV, "sync-token").map_or("", Element::trimmed);
        let since = if token.is_empty() {
            ModSeq::ZERO
        } else {
            token
                .strip_prefix(SYNC_PREFIX)
                .and_then(|n| n.parse::<u64>().ok())
                .map(ModSeq)
                .ok_or(SyncError::Token)?
        };
        if root
            .child(DAV, "sync-level")
            .is_some_and(|l| l.trimmed() != "1")
        {
            return Err(SyncError::Level);
        }
        let limit = root
            .child(DAV, "limit")
            .and_then(|l| l.child(DAV, "nresults"))
            .map(|n| n.trimmed().parse::<usize>().map(|v| v.max(1)))
            .transpose()
            .map_err(|_| SyncError::Limit)?;
        let props = root.child(DAV, "prop").map(props::keys).unwrap_or_default();
        Ok(SyncRequest {
            since,
            limit,
            props,
        })
    }
}

/// `REPORT`.
pub(crate) async fn report(cx: &Cx<'_>, target: &Target, body: Body) -> Result<Response> {
    let el = match cx.xml(body).await {
        Ok(Some(el)) => el,
        Ok(None) => return Ok(status(StatusCode::BAD_REQUEST)),
        Err(code) => return Ok(status(code)),
    };
    if el.ns == CALDAV {
        return query::report(cx, target, &el).await;
    }
    if el.ns == CARDDAV {
        return card::report(cx, target, &el).await;
    }
    let req = match SyncRequest::parse(&el) {
        Ok(r) => r,
        Err(SyncError::Report) => return Ok(unsupported()),
        Err(SyncError::Token) => return Ok(invalid_token()),
        Err(SyncError::Level) => {
            return Ok(precondition(
                StatusCode::FORBIDDEN,
                "<d:sync-traversal-supported/>",
            ));
        }
        Err(SyncError::Limit) => return Ok(status(StatusCode::BAD_REQUEST)),
    };
    let Target::Collection(kind, _, name) = target else {
        return Ok(unsupported());
    };
    if !cx.rights.read {
        return Ok(crate::share::need_privileges(
            target,
            cx.login(),
            "<d:read/>",
        ));
    }
    let Some(c) = cx.store().collection_by_name(cx.scope, *kind, name).await? else {
        return Ok(status(StatusCode::NOT_FOUND));
    };
    let max = cx.dav.cfg.max_response_items.max(1);
    let limit = req.limit.map_or(max, |l| l.min(max));
    let ch = match cx.store().changes(cx.scope, c.id, req.since, limit).await {
        Ok(ch) => ch,
        Err(Error::Conflict) => return Ok(invalid_token()),
        Err(e) => return Err(e),
    };
    let names: Vec<String> = ch.created.iter().chain(&ch.updated).cloned().collect();
    let objects = if names.is_empty() {
        Vec::new()
    } else {
        cx.store().multiget(cx.scope, c.id, &names).await?
    };
    let by_name: HashMap<&str, &DavObject> = objects.iter().map(|o| (o.name.as_str(), o)).collect();
    let want = Want::Props(req.props);
    let need_body = want.needs_body();
    let login = cx.login();
    let mut ms = Multistatus::new();
    for n in &names {
        match by_name.get(n.as_str()) {
            Some(o) => {
                let res = Res::Object {
                    coll: &c,
                    name: &o.name,
                    etag: &o.etag,
                    body: need_body.then_some(o.body.as_str()),
                    tag: o.index.schedule_tag.as_deref(),
                };
                props::emit(cx, &mut ms, &res, &want);
            }
            None => ms.status(
                &path::object(c.kind, login, &c.name, n),
                StatusCode::NOT_FOUND,
                None,
            ),
        }
    }
    for n in &ch.destroyed {
        ms.status(
            &path::object(c.kind, login, &c.name, n),
            StatusCode::NOT_FOUND,
            None,
        );
    }
    if ch.has_more {
        props::truncated(&mut ms, &path::collection(c.kind, login, &c.name));
    }
    ms.sync_token(&props::sync_token(ch.new_state));
    Ok(multistatus(ms))
}

pub(crate) fn invalid_token() -> Response {
    precondition(StatusCode::FORBIDDEN, "<d:valid-sync-token/>")
}
