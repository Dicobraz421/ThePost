//! Общий доступ к календарям и адресным книгам (DAV-17; SRS 4.4, 4.5; CONTRACTS.md#Sharing).
//!
//! Права на чужие коллекции проверяет только [`SharingAuthorizer`] (CTR-84): CalDAV и CardDAV права не вычисляют.
//! Без [`crate::Deps::sharing`] (или без планирования — учётных записей) доступ — только к своим ресурсам, как до
//! общего доступа; выдача прав `POST` и `cs:invite` — при хранилищах [`SharingStores`] ([`crate::router_shared`]).
//!
//! - Доступ к ресурсу чужой учётной записи (логин в URL — не вошедший; [`access`]): принципалы организации — чтение
//!   свойств и отчёты; дом коллекций владельца — `PROPFIND` с коллекциями, на которые есть права; коллекция и объекты —
//!   по действующим правам: свойства коллекции и отчёт занятости — любое право, `GET` и данные объектов — `read`,
//!   `PUT` и `DELETE` объекта — `write`, `PROPPATCH` и `DELETE` коллекции — `admin`; создание коллекций и ресурсы
//!   планирования (`inbox/`, `outbox/`) — только владелец. Нет прав — 403 `need-privileges`.
//! - Права в ответах: `current-user-privilege-set` по действующим правам ([`privileges`]).
//! - Общие коллекции — в доме получателя (`calendar-home-set`, `addressbook-home-set`) ссылками на коллекцию владельца
//!   (`resourcetype` с `cs:shared`, `owner` — принципал владельца, имя — «имя (владелец)»; [`shared`]).
//! - Выдача и отзыв прав — `POST` на коллекцию (CalendarServer sharing: `cs:share` с `cs:set` и `cs:remove` — Календарь
//!   macOS и iOS; WebDAV `share-resource` с `share-access`): получатель — `mailto:` адрес учётной записи или ссылка на
//!   её принципал; проверка — [`SharingAuthorizer::check_grant`], запись — [`CollectionAcl::set_collection_acl`].
//!   Выданные права — свойство `cs:invite` коллекции владельца. Другие клиенты выдают права через JMAP `shareWith` и
//!   Admin API.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Arc;

use axum::body::Body;
use axum::http::StatusCode;
use axum::response::Response;
use tp_contracts::auth::Principal;
use tp_contracts::dav::{Collection, CollectionId, CollectionKind};
use tp_contracts::meta::{Account, AccountScope, AccountStore, AddressTarget};
use tp_contracts::sharing::{
    ANYONE, CollectionAcl, CollectionAclEntry, CollectionRights, DelegateStore, SharingAuthorizer,
};
use tp_contracts::{AccountId, Error, Result};

use crate::path::{self, Target};
use crate::server::{Cx, Dav, precondition, status};
use crate::xml::{CS, DAV, Element, href, text};

/// Хранилища общего доступа ([`crate::router_shared`]): записи прав и представители. Права проверяет только
/// [`crate::Deps::sharing`]; хранилища нужны для записи (`POST` sharing, `PROPPATCH` групп представителей) и
/// списков (`cs:invite`, `group-member-set`).
#[derive(Clone)]
pub struct SharingStores {
    /// Записи прав на коллекции: выдача и отзыв (`POST` sharing), список выданных (`cs:invite`).
    pub acl: Arc<dyn CollectionAcl>,
    /// Представители учётных записей (`calendar-proxy-read`, `calendar-proxy-write`; DAV-18).
    pub delegates: Arc<dyn DelegateStore>,
}

impl std::fmt::Debug for SharingStores {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharingStores").finish_non_exhaustive()
    }
}

/// Общий доступ запроса ([`Dav::sharing`]): права, учётные записи и, если подключены, хранилища.
#[derive(Clone, Copy)]
pub(crate) struct Sharing<'a> {
    pub authz: &'a dyn SharingAuthorizer,
    pub accounts: &'a dyn AccountStore,
    pub stores: Option<&'a SharingStores>,
}

/// Доступ запроса к ресурсу: владелец и права вошедшего.
pub(crate) struct Access {
    /// Логин владельца (ссылки ответов).
    pub owner: String,
    /// Учётная запись владельца.
    pub scope: AccountScope,
    /// Права на коллекцию запроса: свои ресурсы — все, чужой дом и принципалы — нет.
    pub rights: CollectionRights,
}

/// 403 `need-privileges` для ресурса `target` владельца `owner`.
pub(crate) fn need_privileges(target: &Target, owner: &str, privilege: &str) -> Response {
    precondition(
        StatusCode::FORBIDDEN,
        &format!(
            "<d:need-privileges><d:resource>{}<d:privilege>{privilege}</d:privilege></d:resource></d:need-privileges>",
            href(&target.href(owner))
        ),
    )
}

/// Доступ вошедшего `principal` к ресурсу `target` методом `method`; отказ — готовый ответ.
pub(crate) async fn access(
    dav: &Dav,
    principal: &Principal,
    target: &Target,
    method: &str,
) -> std::result::Result<Access, Box<Response>> {
    let own = AccountScope {
        tenant: principal.tenant,
        account: principal.account,
    };
    let Some(login) = target
        .login()
        .filter(|l| !l.eq_ignore_ascii_case(&principal.login))
    else {
        return Ok(Access {
            owner: principal.login.clone(),
            scope: own,
            rights: CollectionRights::ALL,
        });
    };
    let forbidden = || Box::new(status(StatusCode::FORBIDDEN));
    let Some(sharing) = dav.sharing() else {
        return Err(forbidden());
    };
    let owner = match sharing.accounts.by_login(principal.tenant, login).await {
        Ok(Some(a)) if a.enabled => a,
        Ok(_) => return Err(forbidden()),
        Err(e) => return Err(Box::new(crate::server::store_error(&e))),
    };
    let scope = AccountScope {
        tenant: principal.tenant,
        account: owner.id,
    };
    let none = |rights| {
        Ok(Access {
            owner: owner.login.clone(),
            scope,
            rights,
        })
    };
    let (kind, name) = match target {
        Target::Root | Target::Principals => return none(CollectionRights::ALL),
        // Принципалы организации и их группы представителей видны всем; менять группы — только владельцу.
        Target::Principal(_) | Target::Proxy(..) => {
            return if matches!(method, "PROPFIND" | "REPORT") {
                none(CollectionRights::NONE)
            } else {
                Err(forbidden())
            };
        }
        Target::Home(..) => {
            return if method == "PROPFIND" {
                none(CollectionRights::NONE)
            } else {
                Err(forbidden())
            };
        }
        Target::Collection(kind, _, name) | Target::Object(kind, _, name, _) => (*kind, name),
    };
    if path::reserved(kind, name) && kind == CollectionKind::Calendar {
        return Err(forbidden());
    }
    let coll = match dav.d.store.collection_by_name(scope, kind, name).await {
        Ok(Some(c)) => c,
        Ok(None) => return Err(forbidden()),
        Err(e) => return Err(Box::new(crate::server::store_error(&e))),
    };
    let rights = match sharing.authz.rights(principal, owner.id, coll.id).await {
        Ok(r) => r.effective(),
        Err(e) => return Err(Box::new(crate::server::store_error(&e))),
    };
    let object = matches!(target, Target::Object(..));
    let (need, privilege) = match method {
        "PROPFIND" if object => (CollectionRights::READ, "<d:read/>"),
        "PROPFIND" | "REPORT" | "POST" => (CollectionRights::NONE, "<d:read/>"),
        "GET" | "HEAD" => (CollectionRights::READ, "<d:read/>"),
        "PUT" | "DELETE" if object => (CollectionRights::WRITE, "<d:write-content/>"),
        "PROPPATCH" | "DELETE" => (ADMIN, "<d:write-properties/>"),
        _ => return Err(forbidden()),
    };
    if rights.is_empty() || !rights.contains(need) {
        return Err(Box::new(need_privileges(target, &owner.login, privilege)));
    }
    Ok(Access {
        owner: owner.login,
        scope,
        rights,
    })
}

/// Право `admin`.
pub(crate) const ADMIN: CollectionRights = CollectionRights {
    admin: true,
    ..CollectionRights::NONE
};

/// Права на свои ресурсы (RFC 3744 §5.4) — как были до общего доступа.
const PRIV_OWNER: &str = concat!(
    "<d:privilege><d:all/></d:privilege><d:privilege><d:read/></d:privilege>",
    "<d:privilege><d:write/></d:privilege><d:privilege><d:write-properties/></d:privilege>",
    "<d:privilege><d:write-content/></d:privilege><d:privilege><d:bind/></d:privilege>",
    "<d:privilege><d:unbind/></d:privilege>",
    "<d:privilege><d:read-current-user-privilege-set/></d:privilege>"
);

/// `current-user-privilege-set` по действующим правам `rights` (RFC 3744 §5.4, RFC 4791 §6.1.1 `read-free-busy`,
/// RFC 6638 §6 `schedule-send`).
pub(crate) fn privileges(rights: CollectionRights) -> String {
    let r = rights.effective();
    if r == CollectionRights::ALL {
        return PRIV_OWNER.to_owned();
    }
    let mut out = String::new();
    let mut add = |p: &str| {
        let _ = write!(out, "<d:privilege>{p}</d:privilege>");
    };
    if r.read {
        add("<d:read/>");
    }
    if r.free_busy {
        add("<c:read-free-busy/>");
    }
    if r.write {
        add("<d:write-content/>");
        add("<d:bind/>");
        add("<d:unbind/>");
    }
    if r.schedule {
        add("<c:schedule-send/>");
    }
    if r.read {
        add("<d:read-current-user-privilege-set/>");
    }
    out
}

// ---------- Общие коллекции в доме получателя ----------

/// Чужая коллекция в ответе: владелец и права вошедшего.
pub(crate) struct Shared {
    pub coll: Collection,
    /// Логин владельца.
    pub owner: String,
    /// Отображаемое имя владельца — в имени коллекции в доме получателя; в доме владельца — `None`.
    pub owner_name: Option<String>,
    pub rights: CollectionRights,
}

/// Общие с вошедшим коллекции вида `kind` для его дома ([`SharingAuthorizer::shared_with`]) по владельцу и имени.
/// Без общего доступа — пусто; удалённые коллекции и отключённые владельцы пропускаются.
pub(crate) async fn shared(cx: &Cx<'_>, kind: CollectionKind) -> Result<Vec<Shared>> {
    let Some(sharing) = cx.dav.sharing() else {
        return Ok(Vec::new());
    };
    let list = sharing.authz.shared_with(cx.principal).await?;
    let mut ids: Vec<AccountId> = list.iter().map(|s| s.owner).collect();
    ids.sort_unstable();
    ids.dedup();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let owners: HashMap<AccountId, Account> = sharing
        .accounts
        .by_ids(cx.scope.tenant, &ids)
        .await?
        .into_iter()
        .filter(|a| a.enabled)
        .map(|a| (a.id, a))
        .collect();
    let mut out = Vec::new();
    for s in list.into_iter().filter(|s| !s.rights.is_empty()) {
        let Some(owner) = owners.get(&s.owner) else {
            continue;
        };
        let scope = AccountScope {
            tenant: cx.scope.tenant,
            account: s.owner,
        };
        let Some(coll) = cx.store().get_collection(scope, s.collection).await? else {
            continue;
        };
        if coll.kind != kind || coll.is_gal() {
            continue;
        }
        out.push(Shared {
            coll,
            owner: owner.login.clone(),
            owner_name: Some(display_name(owner)),
            rights: s.rights.effective(),
        });
    }
    out.sort_by(|a, b| (&a.owner, &a.coll.name).cmp(&(&b.owner, &b.coll.name)));
    Ok(out)
}

/// Отображаемое имя учётной записи: задано — оно, иначе логин.
pub(crate) fn display_name(a: &Account) -> String {
    if a.display_name.trim().is_empty() {
        a.login.clone()
    } else {
        a.display_name.clone()
    }
}

/// Коллекции `list` владельца в его доме, открытом чужому вошедшему: только с правами, с правами по каждой.
pub(crate) async fn foreign_home(cx: &Cx<'_>, list: Vec<Collection>) -> Result<Vec<Shared>> {
    let Some(sharing) = cx.dav.sharing() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for c in list.into_iter().filter(|c| !c.is_gal()) {
        let rights = sharing
            .authz
            .rights(cx.principal, cx.scope.account, c.id)
            .await?
            .effective();
        if !rights.is_empty() {
            out.push(Shared {
                coll: c,
                owner: cx.login().to_owned(),
                owner_name: None,
                rights,
            });
        }
    }
    Ok(out)
}

// ---------- Выданные права: cs:invite ----------

/// Значение `cs:invite` коллекции `coll` владельца: получатели прав (имя входа — `mailto:`, группа и `anyone` — как
/// есть) с уровнем `cs:read` или `cs:read-write`. Без общего доступа или не владельцу — `None`.
pub(crate) async fn invite(cx: &Cx<'_>, coll: CollectionId) -> Result<Option<String>> {
    let Some(sharing) = cx.dav.sharing() else {
        return Ok(None);
    };
    if !cx.own() {
        return Ok(None);
    }
    let Some(stores) = sharing.stores else {
        return Ok(None);
    };
    let entries = stores.acl.collection_acl(cx.scope, coll).await?;
    Ok(Some(invite_xml(&entries)))
}

fn invite_xml(entries: &[CollectionAclEntry]) -> String {
    let mut out = String::new();
    for e in entries.iter().filter(|e| !e.rights.is_empty()) {
        let r = e.rights.effective();
        let access = if r.write {
            "<cs:read-write/>"
        } else {
            "<cs:read/>"
        };
        let who = if e.identifier == ANYONE || !e.identifier.contains('@') {
            e.identifier.clone()
        } else {
            format!("mailto:{}", e.identifier)
        };
        let _ = write!(
            out,
            "<cs:user>{}<cs:invite-accepted/><cs:access>{access}</cs:access></cs:user>",
            href(&who)
        );
    }
    out
}

// ---------- POST: выдача и отзыв прав ----------

/// Получатель и права из тела запроса sharing.
struct Sharee {
    href: String,
    rights: CollectionRights,
}

/// Получатели тела `cs:share` (CalendarServer) или `share-resource` (WebDAV resource sharing); неверное тело — `None`.
fn sharees(root: &Element) -> Option<Vec<Sharee>> {
    let href_of = |el: &Element| el.child(DAV, "href").map(|h| h.trimmed().to_owned());
    let mut out = Vec::new();
    if root.is(CS, "share") {
        for op in &root.children {
            let rights = if op.is(CS, "set") {
                if op.child(CS, "read-write").is_some() {
                    CollectionRights::WRITE
                } else {
                    CollectionRights::READ
                }
            } else if op.is(CS, "remove") {
                CollectionRights::NONE
            } else {
                continue;
            };
            out.push(Sharee {
                href: href_of(op)?,
                rights,
            });
        }
    } else if root.is(DAV, "share-resource") {
        for s in root.children.iter().filter(|c| c.is(DAV, "sharee")) {
            let access = s.child(DAV, "share-access")?;
            let rights = if access.child(DAV, "read-write").is_some() {
                CollectionRights::WRITE
            } else if access.child(DAV, "read").is_some() {
                CollectionRights::READ
            } else if access.child(DAV, "no-access").is_some() {
                CollectionRights::NONE
            } else {
                return None;
            };
            out.push(Sharee {
                href: href_of(s)?,
                rights,
            });
        }
    } else {
        return None;
    }
    (!out.is_empty()).then_some(out)
}

/// Имя входа учётной записи организации по ссылке получателя (`mailto:` — основной адрес или псевдоним, ссылка на
/// принципал); не учётная запись организации — `None`.
pub(crate) async fn login_of(
    accounts: &dyn AccountStore,
    tenant: tp_contracts::TenantId,
    h: &str,
) -> Result<Option<Account>> {
    let lower = h.to_ascii_lowercase();
    if lower.starts_with("mailto:") {
        let addr = crate::itip::address(h);
        return Ok(match accounts.by_address(tenant, &addr).await? {
            Some(AddressTarget::Account(a)) => Some(a),
            _ => None,
        });
    }
    let Some(login) = path::principal_login(h) else {
        return Ok(None);
    };
    accounts.by_login(tenant, &login).await
}

/// `POST` на коллекцию: выдача и отзыв прав. Все получатели проверяются до записи: неизвестный — 409, нет права
/// выдавать или политика не допускает — 403; ничего не записано.
pub(crate) async fn post(cx: &Cx<'_>, target: &Target, body: Body) -> Result<Response> {
    let Some(sharing) = cx.dav.sharing() else {
        return Ok(crate::server::not_allowed());
    };
    let Some(stores) = sharing.stores else {
        return Ok(crate::server::not_allowed());
    };
    let Target::Collection(kind, _, name) = target else {
        return Ok(crate::server::not_allowed());
    };
    let el = match cx.xml(body).await {
        Ok(Some(el)) => el,
        Ok(None) => return Ok(status(StatusCode::BAD_REQUEST)),
        Err(code) => return Ok(status(code)),
    };
    let Some(list) = sharees(&el) else {
        return Ok(status(StatusCode::BAD_REQUEST));
    };
    let Some(coll) = cx.store().collection_by_name(cx.scope, *kind, name).await? else {
        return Ok(status(StatusCode::NOT_FOUND));
    };
    if coll.is_gal() {
        return Ok(need_privileges(target, cx.login(), "<d:write-acl/>"));
    }
    let mut grants = Vec::with_capacity(list.len());
    for s in list {
        let Some(account) = login_of(sharing.accounts, cx.scope.tenant, &s.href).await? else {
            return Ok(status(StatusCode::CONFLICT));
        };
        if account.id == cx.scope.account {
            return Ok(status(StatusCode::CONFLICT));
        }
        match sharing
            .authz
            .check_grant(
                cx.principal,
                cx.scope.account,
                coll.id,
                &account.login,
                s.rights,
            )
            .await
        {
            Ok(()) => {}
            Err(Error::Forbidden) => {
                return Ok(need_privileges(target, cx.login(), "<d:write-acl/>"));
            }
            Err(Error::Invalid(_)) => return Ok(status(StatusCode::CONFLICT)),
            Err(e) => return Err(e),
        }
        grants.push((account.login, s.rights));
    }
    for (login, rights) in grants {
        stores
            .acl
            .set_collection_acl(cx.scope, coll.id, &login, rights)
            .await?;
        tracing::info!(collection = %coll.id, "DAV: права на коллекцию изменены");
    }
    Ok(status(StatusCode::OK))
}

/// Отображаемое имя общей коллекции в доме получателя (XML): «имя (владелец)»; в доме владельца — `None`.
pub(crate) fn shared_name(s: &Shared) -> Option<String> {
    let owner = s.owner_name.as_ref()?;
    Some(text(&format!("{} ({owner})", s.coll.display_name)).into_owned())
}
