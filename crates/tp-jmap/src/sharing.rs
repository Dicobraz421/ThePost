//! JMAP — общий доступ к календарям и адресным книгам (JMAP-32; RFC 9670, RFC 9610 §2, draft-ietf-jmap-calendars)
//! поверх единого слоя прав [`SharingAuthorizer`] (CTR-84) и записей [`CollectionAcl`] (CTR-48),
//! [`Jmap::with_sharing`].
//!
//! - Чужие коллекции, на которые у субъекта есть права ([`SharingAuthorizer::shared_with`]: записи по имени входа,
//!   группам и [`ANYONE`] с ограничением политикой организации), — в аккаунтах их владельцев: в сессии аккаунт с
//!   `isPersonal` = `false` и capability календарей, задач и контактов; методы `Calendar`, `CalendarEvent`,
//!   `TaskList`, `Task`, `AddressBook`, `ContactCard` с `accountId` владельца видят только эти коллекции
//!   ([`Access`]). Почта чужих ящиков через JMAP не открывается.
//! - `myRights` — действующие права субъекта ([`CollectionRights`]): свой аккаунт — все, чужой — по
//!   [`SharingAuthorizer`]. `shareWith` — записи прав учётных записей организации по идентификатору `Principal`;
//!   виден и меняется при праве `admin` (`mayShare`). Записи групп и [`ANYONE`] JMAP не показывает и не меняет (их
//!   выдаёт CalDAV, CardDAV, Admin API).
//! - Выдачу прав проверяет [`SharingAuthorizer::check_grant`]: владелец или субъект с `admin`, в пределах политики
//!   организации. Отказ — `forbidden`.
//! - Изменение прав учётной записи записывает ей `ShareNotification` (RFC 9670 §5): `/get`, `/changes`, `/query`,
//!   `/set` (только `destroy`); `/queryChanges` — `cannotCalculateChanges`.
//!
//! Отличия:
//! - `ShareNotification` хранятся в памяти узла (контракта хранилища уведомлений нет): не переживают перезапуск и
//!   не видны на других узлах; на учётную запись — не больше [`MAX_NOTICES`], старшие вытесняются;
//! - в чужом аккаунте свойства коллекции (имя, цвет, подписка) меняет только субъект с правом `admin`, создать
//!   коллекцию нельзя (`mayCreateCalendar`, `mayCreateAddressBook` — `false`), отправка приглашений
//!   (`sendSchedulingMessages`) — `forbidden`;
//! - календари по представительству ([`SharingAuthorizer::delegations`]) в аккаунтах JMAP не показываются —
//!   представитель работает с ними через CalDAV (`calendar-proxy-*`, DAV-18).

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};

use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use tp_contracts::auth::Principal;
use tp_contracts::dav::{
    Collection, CollectionId, CollectionKind, DavObject, DavObjectId, DavStore,
};
use tp_contracts::ids::{AccountId, TenantId};
use tp_contracts::meta::{AccountScope, Changes};
use tp_contracts::sharing::{
    ANYONE, CollectionAcl, CollectionRights, SharedCollection, SharingAuthorizer,
};
use tp_contracts::{Error, ModSeq, Result};

use crate::core::error::{MethodError, MethodResult, changes_error, set_error};
use crate::core::request::{
    ChangesArgs, GetArgs, QueryWindow, SetArgs, SetOutcome, apply_window, changes_response, object,
    parse_args, parse_state, select, state_string,
};
use crate::core::{Ctx, Jmap};
use crate::email_get::utc_date;

/// Наибольшее число `ShareNotification` учётной записи; старшие вытесняются.
pub(crate) const MAX_NOTICES: usize = 1000;

/// Зависимости общего доступа ([`Jmap::with_sharing`]).
#[derive(Clone)]
pub struct SharingDeps {
    /// Единый слой прав на коллекции: действующие права, общие коллекции субъекта, проверка выдачи.
    pub authz: Arc<dyn SharingAuthorizer>,
    /// Записи прав на коллекции: `shareWith` и выдача прав, прошедшая [`SharingAuthorizer::check_grant`].
    pub acl: Arc<dyn CollectionAcl>,
}

impl std::fmt::Debug for SharingDeps {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharingDeps").finish_non_exhaustive()
    }
}

/// Доступ вызова к коллекциям аккаунта: свой — все права, чужой — права на общие коллекции.
#[derive(Debug, Clone, Default)]
pub(crate) enum Access {
    /// Свой аккаунт.
    #[default]
    Own,
    /// Аккаунт владельца общих коллекций: действующие права по коллекциям.
    Shared(HashMap<CollectionId, CollectionRights>),
}

impl Access {
    /// Свой аккаунт.
    pub(crate) fn is_own(&self) -> bool {
        matches!(self, Self::Own)
    }

    /// Действующие права на коллекцию.
    pub(crate) fn rights(&self, c: CollectionId) -> CollectionRights {
        match self {
            Self::Own => CollectionRights::ALL,
            Self::Shared(m) => m.get(&c).copied().unwrap_or(CollectionRights::NONE),
        }
    }

    /// Коллекция видна (есть хоть какое-то право).
    pub(crate) fn sees(&self, c: CollectionId) -> bool {
        !self.rights(c).is_empty()
    }

    /// Объекты коллекции читаются.
    pub(crate) fn reads(&self, c: CollectionId) -> bool {
        self.rights(c).contains(CollectionRights::READ)
    }

    /// Объекты коллекции меняются.
    pub(crate) fn writes(&self, c: CollectionId) -> bool {
        self.rights(c).contains(CollectionRights::WRITE)
    }

    /// Свойства и права коллекции меняются, коллекция удаляется.
    pub(crate) fn admins(&self, c: CollectionId) -> bool {
        self.rights(c).contains(CollectionRights {
            admin: true,
            ..CollectionRights::NONE
        })
    }
}

/// Чужие коллекции субъекта с действующими правами ([`SharingAuthorizer::shared_with`]); общий доступ или DAV не
/// подключены — пусто.
pub(crate) async fn shared(jmap: &Jmap, p: &Principal) -> Result<Vec<SharedCollection>> {
    let (Some(s), Some(_)) = (&jmap.sharing, &jmap.dav) else {
        return Ok(Vec::new());
    };
    let mut all = s.authz.shared_with(p).await?;
    all.retain(|c| c.owner != p.account && !c.rights.is_empty());
    Ok(all)
}

/// Права на коллекции аккаунта `owner`, открытого субъектом; нет общих коллекций — `accountNotFound`.
pub(crate) async fn open(
    jmap: &Jmap,
    p: &Principal,
    owner: &str,
) -> MethodResult<HashMap<CollectionId, CollectionRights>> {
    let not_found = || MethodError::new("accountNotFound");
    let owner = AccountId::from_str(owner).map_err(|_| not_found())?;
    let rights: HashMap<_, _> = shared(jmap, p)
        .await?
        .into_iter()
        .filter(|c| c.owner == owner)
        .map(|c| (c.collection, c.rights))
        .collect();
    if rights.is_empty() {
        return Err(not_found());
    }
    Ok(rights)
}

/// Чужой аккаунт в сессии: владелец и вид его общих коллекций.
pub(crate) struct SharedAccount {
    /// Владелец.
    pub owner: AccountId,
    /// Имя аккаунта — имя входа владельца.
    pub name: String,
    /// Есть общие календари (и списки задач).
    pub calendars: bool,
    /// Есть общие адресные книги.
    pub books: bool,
    /// Ни на одну коллекцию нет права записи.
    pub read_only: bool,
}

/// Чужие аккаунты субъекта для сессии (по владельцам); ошибка хранилища — пусто с записью в журнал.
pub(crate) async fn session_accounts(jmap: &Jmap, p: &Principal) -> Vec<SharedAccount> {
    match load_accounts(jmap, p).await {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(error = %e, "jmap: общие коллекции для сессии не прочитаны");
            Vec::new()
        }
    }
}

async fn load_accounts(jmap: &Jmap, p: &Principal) -> Result<Vec<SharedAccount>> {
    let Some(store) = &jmap.dav else {
        return Ok(Vec::new());
    };
    let mut by_owner: BTreeMap<AccountId, Vec<SharedCollection>> = BTreeMap::new();
    for c in shared(jmap, p).await? {
        by_owner.entry(c.owner).or_default().push(c);
    }
    let mut out = Vec::new();
    for (owner, cols) in by_owner {
        let Some(account) = jmap.d.accounts.get(p.tenant, owner).await? else {
            continue;
        };
        let scope = AccountScope {
            tenant: p.tenant,
            account: owner,
        };
        let (mut calendars, mut books, mut read_only) = (false, false, true);
        for c in &cols {
            match store.get_collection(scope, c.collection).await? {
                Some(col) if col.kind == CollectionKind::Calendar => calendars = true,
                Some(_) => books = true,
                None => continue,
            }
            read_only &= !c.rights.contains(CollectionRights::WRITE);
        }
        if calendars || books {
            out.push(SharedAccount {
                owner,
                name: account.login,
                calendars,
                books,
                read_only,
            });
        }
    }
    Ok(out)
}

// ---------- Права в JMAP ----------

/// `CalendarRights` (draft-ietf-jmap-calendars) по действующим правам; `shares` — общий доступ подключён.
pub(crate) fn calendar_rights(r: CollectionRights, shares: bool) -> Value {
    let r = r.effective();
    json!({
        "mayReadFreeBusy": r.free_busy, "mayReadItems": r.read, "mayWriteAll": r.write,
        "mayWriteOwn": r.write, "mayUpdatePrivate": r.write, "mayRSVP": r.schedule,
        "mayShare": r.admin && shares, "mayDelete": r.admin,
    })
}

/// `AddressBookRights` (RFC 9610 §2) по действующим правам; книга GAL — только чтение.
pub(crate) fn book_rights(r: CollectionRights, shares: bool, gal: bool) -> Value {
    let r = r.effective();
    json!({
        "mayRead": r.read, "mayWrite": r.write && !gal, "mayShare": r.admin && shares && !gal,
        "mayDelete": r.admin && !gal,
    })
}

/// `TaskListRights` (draft-ietf-jmap-tasks) по действующим правам.
pub(crate) fn task_list_rights(r: CollectionRights, shares: bool) -> Value {
    let r = r.effective();
    json!({
        "mayReadItems": r.read, "mayWriteAll": r.write, "mayWriteOwn": r.write,
        "mayUpdatePrivate": r.write, "mayRSVP": r.schedule, "mayAdmin": r.admin && shares,
        "mayDelete": r.admin,
    })
}

/// Вид общего объекта JMAP: календарь и список задач — коллекции календаря, адресная книга — коллекция книги.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ObjectType {
    /// `Calendar`.
    Calendar,
    /// `TaskList`.
    TaskList,
    /// `AddressBook`.
    AddressBook,
}

impl ObjectType {
    /// Имя типа (`ShareNotification.objectType`).
    fn name(self) -> &'static str {
        match self {
            Self::Calendar => "Calendar",
            Self::TaskList => "TaskList",
            Self::AddressBook => "AddressBook",
        }
    }

    /// Объект прав JMAP по правам как выданы.
    fn rights(self, r: CollectionRights) -> Value {
        match self {
            Self::Calendar => calendar_rights(r, true),
            Self::TaskList => task_list_rights(r, true),
            Self::AddressBook => book_rights(r, true, false),
        }
    }

    /// Права из объекта прав JMAP; неизвестный ключ или не логическое значение — `None`.
    fn parse(self, v: &Value) -> Option<CollectionRights> {
        let m = v.as_object()?;
        let mut r = CollectionRights::NONE;
        let book = self == Self::AddressBook;
        for (k, v) in m {
            let on = v.as_bool()?;
            let slot = match k.as_str() {
                "mayReadFreeBusy" if self == Self::Calendar => &mut r.free_busy,
                "mayReadItems" if !book => &mut r.read,
                "mayRead" if book => &mut r.read,
                "mayWriteAll" | "mayWriteOwn" | "mayUpdatePrivate" if !book => &mut r.write,
                "mayWrite" if book => &mut r.write,
                "mayRSVP" if !book => &mut r.schedule,
                "mayShare" if self != Self::TaskList => &mut r.admin,
                "mayAdmin" if self == Self::TaskList => &mut r.admin,
                "mayDelete" => &mut r.admin,
                _ => return None,
            };
            *slot |= on;
        }
        Some(r)
    }
}

/// Записи прав учётных записей на коллекцию: (учётная запись, имя входа, права как выданы).
async fn account_entries(
    ctx: &Ctx<'_>,
    s: &SharingDeps,
    c: CollectionId,
) -> MethodResult<Vec<(AccountId, String, CollectionRights)>> {
    let entries = match s.acl.collection_acl(ctx.scope, c).await {
        Ok(e) => e,
        Err(Error::NotFound) => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    let mut out = Vec::new();
    for e in entries {
        if e.identifier == ANYONE {
            continue;
        }
        if let Some(a) = ctx
            .jmap
            .d
            .accounts
            .by_login(ctx.scope.tenant, &e.identifier)
            .await?
        {
            out.push((a.id, a.login, e.rights));
        }
    }
    Ok(out)
}

/// `shareWith` коллекции как объекта `ty`: права учётных записей по `Principal.id`; без права `admin` или без общего
/// доступа — `null`.
pub(crate) async fn share_with(
    ctx: &Ctx<'_>,
    ty: ObjectType,
    c: &Collection,
) -> MethodResult<Value> {
    let Some(s) = &ctx.jmap.sharing else {
        return Ok(Value::Null);
    };
    if !ctx.access.admins(c.id) || c.is_gal() {
        return Ok(Value::Null);
    }
    let mut m = Map::new();
    for (id, _, r) in account_entries(ctx, s, c.id).await? {
        m.insert(id.to_string(), ty.rights(r));
    }
    Ok(Value::Object(m))
}

/// Подготовленная выдача прав учётной записи.
pub(crate) struct Grant {
    account: AccountId,
    login: String,
    old: CollectionRights,
    new: CollectionRights,
}

/// Изменения `shareWith` из патча `/set`: `shareWith` целиком или `shareWith/<id>`; прочие ключи — в `rest`.
pub(crate) fn split_share(v: &Value) -> (Map<String, Value>, Vec<(String, Value)>) {
    let Value::Object(m) = v else {
        return (Map::new(), Vec::new());
    };
    let (mut rest, mut share) = (Map::new(), Vec::new());
    for (k, v) in m {
        if k == "shareWith" || k.starts_with("shareWith/") {
            share.push((k.clone(), v.clone()));
        } else {
            rest.insert(k.clone(), v.clone());
        }
    }
    (rest, share)
}

/// Проверяет изменения `shareWith` коллекции `c` как объекта `ty` (в том числе создаваемой): получатели — учётные
/// записи организации, выдачу каждого изменения — [`SharingAuthorizer::check_grant`] от имени субъекта (владелец или
/// право `admin`, политика организации). Ошибка — `SetError`.
pub(crate) async fn prepare_share(
    ctx: &Ctx<'_>,
    ty: ObjectType,
    c: CollectionId,
    changes: &[(String, Value)],
) -> MethodResult<std::result::Result<Vec<Grant>, Value>> {
    let bad = |why: &str| {
        Ok(Err(set_error(
            "invalidProperties",
            Some(why),
            &["shareWith"],
        )))
    };
    let Some(s) = &ctx.jmap.sharing else {
        // Без общего доступа допустим только `shareWith: null`.
        if changes.iter().all(|(k, v)| k == "shareWith" && v.is_null()) {
            return Ok(Ok(Vec::new()));
        }
        return bad("sharing is not available");
    };
    if changes.is_empty() {
        return Ok(Ok(Vec::new()));
    }
    let current = account_entries(ctx, s, c).await?;
    let mut want: BTreeMap<String, CollectionRights> = BTreeMap::new();
    for (k, v) in changes {
        match k.strip_prefix("shareWith/") {
            Some(id) => {
                let r = match v {
                    Value::Null => CollectionRights::NONE,
                    v => match ty.parse(v) {
                        Some(r) => r,
                        None => return bad("invalid rights"),
                    },
                };
                want.insert(id.to_owned(), r);
            }
            None => {
                let m = match v {
                    Value::Null => Map::new(),
                    Value::Object(m) => m.clone(),
                    _ => return bad("shareWith must be an object or null"),
                };
                for (id, _, _) in &current {
                    want.entry(id.to_string()).or_insert(CollectionRights::NONE);
                }
                for (id, v) in m {
                    let Some(r) = ty.parse(&v) else {
                        return bad("invalid rights");
                    };
                    want.insert(id, r);
                }
            }
        }
    }
    let mut grants = Vec::new();
    for (id, new) in want {
        let account = AccountId::from_str(&id).ok();
        let found = match account {
            Some(a) if a != ctx.scope.account => {
                ctx.jmap.d.accounts.get(ctx.scope.tenant, a).await?
            }
            _ => None,
        };
        let Some(a) = found else {
            return bad("shareWith: unknown principal");
        };
        let old = current
            .iter()
            .find(|(cur, _, _)| *cur == a.id)
            .map_or(CollectionRights::NONE, |(_, _, r)| *r);
        if old == new {
            continue;
        }
        match s
            .authz
            .check_grant(ctx.principal, ctx.scope.account, c, &a.login, new)
            .await
        {
            Ok(()) => {}
            Err(Error::Forbidden) => {
                return Ok(Err(set_error(
                    "forbidden",
                    Some(
                        "mayShare is required and the organization sharing policy must allow these rights",
                    ),
                    &["shareWith"],
                )));
            }
            Err(Error::Invalid(_)) => return bad("shareWith: invalid principal"),
            Err(e) => return Err(e.into()),
        }
        grants.push(Grant {
            account: a.id,
            login: a.login,
            old,
            new,
        });
    }
    Ok(Ok(grants))
}

/// Записывает подготовленные права на коллекцию `c` (объект `ty`) и уведомления получателям.
pub(crate) async fn commit_share(
    ctx: &Ctx<'_>,
    ty: ObjectType,
    c: &Collection,
    grants: Vec<Grant>,
) -> MethodResult<()> {
    let Some(s) = &ctx.jmap.sharing else {
        return Ok(());
    };
    for g in grants {
        s.acl
            .set_collection_acl(ctx.scope, c.id, &g.login, g.new)
            .await?;
        let notice = object(json!({
            "created": utc_date(OffsetDateTime::now_utc()),
            "changedBy": {
                "name": ctx.principal.login,
                "email": ctx.principal.login,
                "principalId": ctx.principal.account.to_string(),
            },
            "objectType": ty.name(),
            "objectAccountId": ctx.scope.account.to_string(),
            "objectId": c.id.to_string(),
            "oldRights": (!g.old.is_empty()).then(|| ty.rights(g.old)),
            "newRights": (!g.new.is_empty()).then(|| ty.rights(g.new)),
            "name": c.display_name,
        }));
        ctx.jmap.notices.push(ctx.scope.tenant, g.account, notice);
    }
    Ok(())
}

// ---------- ShareNotification ----------

/// Уведомление в журнале учётной записи.
#[derive(Debug, Clone)]
struct Notice {
    id: String,
    created: OffsetDateTime,
    object: Map<String, Value>,
}

/// Уведомления учётной записи и журнал их изменений.
#[derive(Debug, Default)]
struct Log {
    /// Состояние: номер последнего изменения.
    state: u64,
    /// Самое старое состояние, от которого считаются изменения.
    floor: u64,
    notices: VecDeque<(u64, Notice)>,
    /// Удалённые: (номер изменения, id).
    destroyed: VecDeque<(u64, String)>,
}

/// `ShareNotification` учётных записей в памяти узла.
#[derive(Debug, Default)]
pub(crate) struct Notices(Mutex<HashMap<(TenantId, AccountId), Log>>);

impl Notices {
    fn with<T>(&self, tenant: TenantId, account: AccountId, f: impl FnOnce(&mut Log) -> T) -> T {
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        f(map.entry((tenant, account)).or_default())
    }

    /// Записывает уведомление учётной записи; сверх [`MAX_NOTICES`] старшее вытесняется (как удаление).
    pub(crate) fn push(&self, tenant: TenantId, account: AccountId, object: Map<String, Value>) {
        self.with(tenant, account, |log| {
            log.state += 1;
            let id = format!("S{}", log.state);
            let notice = Notice {
                id,
                created: OffsetDateTime::now_utc(),
                object,
            };
            log.notices.push_back((log.state, notice));
            if log.notices.len() > MAX_NOTICES
                && let Some((_, old)) = log.notices.pop_front()
            {
                log.destroyed.push_back((log.state, old.id));
            }
            while log.destroyed.len() > MAX_NOTICES {
                if let Some((seq, _)) = log.destroyed.pop_front() {
                    log.floor = seq;
                }
            }
        });
    }
}

fn notice_json(n: &Notice) -> Map<String, Value> {
    let mut m = n.object.clone();
    m.insert("id".into(), json!(n.id));
    m
}

/// `ShareNotification/get`.
pub(crate) async fn notification_get(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: GetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    ctx.check_get_limit(a.ids.as_ref())?;
    let props = a.properties.as_deref();
    let (state, list, not_found) =
        ctx.jmap
            .notices
            .with(ctx.scope.tenant, ctx.scope.account, |log| {
                let (mut list, mut not_found) = (Vec::new(), Vec::new());
                match &a.ids {
                    None => list.extend(log.notices.iter().map(|(_, n)| notice_json(n))),
                    Some(ids) => {
                        for id in ids {
                            match log.notices.iter().find(|(_, n)| n.id == *id) {
                                Some((_, n)) => list.push(notice_json(n)),
                                None => not_found.push(id.clone()),
                            }
                        }
                    }
                }
                (log.state, list, not_found)
            });
    let list: Vec<_> = list.into_iter().map(|n| select(n, props)).collect();
    Ok(object(json!({
        "accountId": a.account_id,
        "state": state_string(ModSeq(state)),
        "list": list,
        "notFound": not_found,
    })))
}

/// `ShareNotification/changes`: уведомления не меняются — только создание и удаление.
pub(crate) async fn notification_changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: ChangesArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let limit = a.limit(ctx.jmap.cfg.max_changes)?;
    let since = parse_state(&a.since_state)?.0;
    let c = ctx
        .jmap
        .notices
        .with(ctx.scope.tenant, ctx.scope.account, |log| {
            if since > log.state || since < log.floor {
                return Err(changes_error(Error::Conflict));
            }
            let mut ev: Vec<(u64, String, bool)> = log
                .notices
                .iter()
                .filter(|(seq, _)| *seq > since)
                .map(|(seq, n)| (*seq, n.id.clone(), true))
                .chain(
                    log.destroyed
                        .iter()
                        .filter(|(seq, _)| *seq > since)
                        .map(|(seq, id)| (*seq, id.clone(), false)),
                )
                .collect();
            ev.sort();
            let has_more = ev.len() > limit;
            ev.truncate(limit);
            let new_state = if has_more {
                ev.last().map_or(log.state, |e| e.0)
            } else {
                log.state
            };
            let mut c = Changes {
                created: Vec::new(),
                updated: Vec::new(),
                destroyed: Vec::new(),
                new_state: ModSeq(new_state),
                has_more,
            };
            for (_, id, created) in ev {
                if created {
                    c.created.push(id);
                } else if !c.created.contains(&id) {
                    c.destroyed.push(id);
                } else {
                    c.created.retain(|x| *x != id);
                }
            }
            Ok(c)
        })?;
    Ok(changes_response(&a.account_id, &a.since_state, &c))
}

/// `ShareNotification/set`: только удаление; создание и изменение — `forbidden`.
pub(crate) async fn notification_set(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: SetArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    a.check_limit(ctx.jmap.cfg.max_objects_in_set)?;
    let (tenant, account) = (ctx.scope.tenant, ctx.scope.account);
    let old = ctx.jmap.notices.with(tenant, account, |log| log.state);
    let old = state_string(ModSeq(old));
    if a.if_in_state.as_ref().is_some_and(|s| *s != old) {
        return Err(MethodError::new("stateMismatch"));
    }
    let mut out = SetOutcome::default();
    for (cid, _) in a.create.unwrap_or_default() {
        out.not_created
            .insert(cid, set_error("forbidden", None, &[]));
    }
    for (id, _) in a.update.unwrap_or_default() {
        out.not_updated
            .insert(id, set_error("forbidden", None, &[]));
    }
    let destroy = a.destroy.unwrap_or_default();
    let new = ctx.jmap.notices.with(tenant, account, |log| {
        for id in destroy {
            match log.notices.iter().position(|(_, n)| n.id == id) {
                Some(i) => {
                    log.notices.remove(i);
                    log.state += 1;
                    log.destroyed.push_back((log.state, id.clone()));
                    out.destroyed.push(id);
                }
                None => {
                    out.not_destroyed
                        .insert(id, set_error("notFound", None, &[]));
                }
            }
        }
        log.state
    });
    Ok(out.response(&a.account_id, &old, &state_string(ModSeq(new))))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NoticeQueryArgs {
    account_id: String,
    filter: Option<NoticeFilter>,
    sort: Option<Vec<crate::core::request::Comparator>>,
    #[serde(flatten)]
    window: QueryWindow,
}

/// Фильтр `ShareNotification/query` (RFC 9670 §5.4).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NoticeFilter {
    after: Option<String>,
    before: Option<String>,
    object_type: Option<String>,
    object_account_id: Option<String>,
}

fn utc(s: &str) -> MethodResult<OffsetDateTime> {
    OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
        .map_err(|_| MethodError::new("unsupportedFilter"))
}

/// `ShareNotification/query`: фильтр по времени, виду и аккаунту объекта; сортировка — `created`.
pub(crate) async fn notification_query(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let a: NoticeQueryArgs = parse_args(args)?;
    ctx.check_account(&a.account_id)?;
    let f = a.filter.unwrap_or_default();
    let after = f.after.as_deref().map(utc).transpose()?;
    let before = f.before.as_deref().map(utc).transpose()?;
    let sort = a.sort.unwrap_or_default();
    if sort.iter().any(|c| c.property != "created") {
        return Err(MethodError::new("unsupportedSort"));
    }
    let descending = sort.first().is_some_and(|c| !c.is_ascending);
    let (state, mut ids) = ctx
        .jmap
        .notices
        .with(ctx.scope.tenant, ctx.scope.account, |log| {
            let ids: Vec<String> = log
                .notices
                .iter()
                .map(|(_, n)| n)
                .filter(|n| after.is_none_or(|t| n.created >= t))
                .filter(|n| before.is_none_or(|t| n.created < t))
                .filter(|n| {
                    f.object_type
                        .as_ref()
                        .is_none_or(|t| n.object["objectType"] == t.as_str())
                })
                .filter(|n| {
                    f.object_account_id
                        .as_ref()
                        .is_none_or(|t| n.object["objectAccountId"] == t.as_str())
                })
                .map(|n| n.id.clone())
                .collect();
            (log.state, ids)
        });
    if descending {
        ids.reverse();
    }
    let mut out = object(json!({
        "accountId": a.account_id,
        "queryState": state_string(ModSeq(state)),
        "canCalculateChanges": false,
    }));
    apply_window(&ids, &a.window, ctx.jmap.cfg.max_query_limit, &mut out)?;
    Ok(out)
}

/// `ShareNotification/queryChanges` — `cannotCalculateChanges`.
pub(crate) async fn notification_query_changes(
    ctx: &mut Ctx<'_>,
    args: Map<String, Value>,
) -> MethodResult<Map<String, Value>> {
    let account = args.get("accountId").and_then(Value::as_str).unwrap_or("");
    ctx.check_account(account)?;
    Err(MethodError::new("cannotCalculateChanges"))
}

// ---------- Изменения в чужом аккаунте ----------

/// Изменения коллекций: созданные и изменённые — только существующие видимые коллекции, для которых `keep` (вид
/// компонентов); удалённые — как есть (права удалённой коллекции не хранятся).
pub(crate) async fn retain_collections(
    ctx: &Ctx<'_>,
    store: &dyn DavStore,
    c: &mut Changes<String>,
    keep: impl Fn(&Collection) -> bool,
) -> MethodResult<()> {
    let mut kept = HashSet::new();
    for id in c.created.iter().chain(&c.updated) {
        let Ok(cid) = CollectionId::from_str(id) else {
            continue;
        };
        if ctx.access.sees(cid)
            && store
                .get_collection(ctx.scope, cid)
                .await?
                .is_some_and(|col| keep(&col))
        {
            kept.insert(id.clone());
        }
    }
    c.created.retain(|i| kept.contains(i));
    c.updated.retain(|i| kept.contains(i));
    Ok(())
}

/// Изменения объектов: созданные и изменённые — только существующие объекты, для которых `keep` (компонент и право
/// чтения коллекции); удалённые — как есть.
pub(crate) async fn retain_objects(
    ctx: &Ctx<'_>,
    store: &dyn DavStore,
    c: &mut Changes<String>,
    keep: impl Fn(&DavObject) -> bool,
) -> MethodResult<()> {
    let ids: Vec<DavObjectId> = c
        .created
        .iter()
        .chain(&c.updated)
        .filter_map(|i| DavObjectId::from_str(i).ok())
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let kept: HashSet<String> = store
        .objects_by_id(ctx.scope, &ids)
        .await?
        .iter()
        .filter(|o| keep(o))
        .map(|o| o.id.to_string())
        .collect();
    c.created.retain(|i| kept.contains(i));
    c.updated.retain(|i| kept.contains(i));
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tp_contracts::auth::Principal;
    use tp_contracts::dav::CollectionId;
    use tp_contracts::meta::AccountScope;
    use tp_contracts::sharing::{CollectionAcl, CollectionRights as R};

    use crate::calendars::tests::create_calendar;
    use crate::contacts::tests::create_book;
    use crate::testenv::Env;

    const USING: &[&str] = &[
        "urn:ietf:params:jmap:core",
        "urn:ietf:params:jmap:calendars",
        "urn:ietf:params:jmap:contacts",
        "urn:ietf:params:jmap:principals",
    ];

    /// Один вызов от имени `who`: аргументы ответа (или ошибки метода).
    async fn call(env: &Env, who: &Principal, method: &str, args: Value) -> Value {
        let req = json!({"using": USING, "methodCalls": [[method, args, "c0"]]});
        let resp = env
            .jmap
            .api(who, req.to_string().as_bytes())
            .await
            .map(|r| serde_json::to_value(r).expect("serialize"))
            .unwrap_or_else(|p| p.to_json());
        resp["methodResponses"][0][1].clone()
    }

    fn id(p: &Principal) -> String {
        p.account.to_string()
    }

    /// Действующие права `who` на коллекцию `c` владельца `owner` — как их посчитал бы единый слой прав по записям
    /// (`FakeSharingAuthorizer` записи `shareWith` не читает).
    fn allow(env: &Env, who: &Principal, owner: &Principal, c: &str, rights: R) {
        let c: CollectionId = c.parse().expect("collection id");
        env.collections
            .set_rights(who.account, owner.account, c, rights);
    }

    /// Записи прав коллекции `c` владельца в хранилище: (имя входа, действующие права записи).
    async fn entries(env: &Env, owner: &Principal, c: &str) -> Vec<(String, R)> {
        let scope = AccountScope {
            tenant: owner.tenant,
            account: owner.account,
        };
        let c: CollectionId = c.parse().expect("collection id");
        env.dav
            .collection_acl(scope, c)
            .await
            .expect("collection_acl")
            .into_iter()
            .map(|e| (e.identifier, e.rights.effective()))
            .collect()
    }

    /// Выдача и отзыв прав на календарь: аккаунт владельца в сессии, чтение, запись без права — `forbidden`,
    /// уведомления получателю.
    #[tokio::test]
    async fn calendar_grant_and_revoke() {
        let env = Env::sharing().await;
        let owner = env.principal.clone();
        let acc = id(&owner);
        let anna = env.user("anna@example.org").await;
        let ivan = env.user("ivan@example.org").await;
        let cal = create_calendar(&env, "Команда").await;
        let r = call(
            &env,
            &owner,
            "CalendarEvent/set",
            json!({"accountId": acc, "create": {"e": {"calendarIds": {&cal: true}, "title": "Планёрка",
                "start": "2026-03-02T10:00:00", "duration": "PT1H"}}}),
        )
        .await;
        let event = r["created"]["e"]["id"].as_str().expect("event").to_owned();

        // До выдачи чужого аккаунта нет.
        let r = call(&env, &anna, "Calendar/get", json!({"accountId": acc})).await;
        assert_eq!(r["type"], "accountNotFound", "{r}");

        let read = json!({"mayReadItems": true});
        let r = call(
            &env,
            &owner,
            "Calendar/set",
            json!({"accountId": acc, "update": {&cal: {"shareWith": {id(&anna): read}}}}),
        )
        .await;
        assert!(r["notUpdated"].is_null(), "{r}");
        let r = call(
            &env,
            &owner,
            "Calendar/get",
            json!({"accountId": acc, "ids": [&cal]}),
        )
        .await;
        let share = &r["list"][0]["shareWith"][id(&anna)];
        assert_eq!(share["mayReadItems"], true, "{r}");
        assert_eq!(share["mayReadFreeBusy"], true, "{r}");
        assert_eq!(share["mayWriteAll"], false, "{r}");
        assert_eq!(r["list"][0]["myRights"]["mayShare"], true, "{r}");
        assert_eq!(
            entries(&env, &owner, &cal).await,
            vec![("anna@example.org".to_owned(), R::READ)]
        );
        allow(&env, &anna, &owner, &cal, R::READ);

        // Получатель видит аккаунт владельца: только чтение, без создания календарей.
        let s = env.jmap.session(&anna, "").await;
        let shared = &s["accounts"][&acc];
        assert_eq!(shared["isPersonal"], false, "{s}");
        assert_eq!(shared["isReadOnly"], true, "{s}");
        assert_eq!(shared["name"], "user@example.org");
        assert_eq!(
            shared["accountCapabilities"]["urn:ietf:params:jmap:calendars"]["mayCreateCalendar"],
            false,
            "{s}"
        );
        assert!(
            shared["accountCapabilities"]["urn:ietf:params:jmap:contacts"].is_null(),
            "{s}"
        );
        let r = call(&env, &anna, "Calendar/get", json!({"accountId": acc})).await;
        assert_eq!(r["list"].as_array().map(Vec::len), Some(1), "{r}");
        assert_eq!(r["list"][0]["myRights"]["mayReadItems"], true, "{r}");
        assert_eq!(r["list"][0]["myRights"]["mayWriteAll"], false, "{r}");
        assert_eq!(r["list"][0]["shareWith"], Value::Null, "{r}");
        let r = call(&env, &anna, "CalendarEvent/get", json!({"accountId": acc})).await;
        assert_eq!(r["list"][0]["title"], "Планёрка", "{r}");

        // Запись без права — `forbidden`.
        let r = call(
            &env,
            &anna,
            "CalendarEvent/set",
            json!({"accountId": acc,
                "create": {"n": {"calendarIds": {&cal: true}, "start": "2026-03-03T10:00:00"}},
                "update": {&event: {"title": "Чужая правка"}}, "destroy": [&event]}),
        )
        .await;
        assert_eq!(r["notCreated"]["n"]["type"], "forbidden", "{r}");
        assert_eq!(r["notUpdated"][&event]["type"], "forbidden", "{r}");
        assert_eq!(r["notDestroyed"][&event]["type"], "forbidden", "{r}");
        let r = call(
            &env,
            &anna,
            "Calendar/set",
            json!({"accountId": acc, "create": {"c": {"name": "Своё"}},
                "update": {&cal: {"name": "Переименован"}},
                "destroy": [&cal]}),
        )
        .await;
        assert_eq!(r["notCreated"]["c"]["type"], "forbidden", "{r}");
        assert_eq!(r["notUpdated"][&cal]["type"], "forbidden", "{r}");
        assert_eq!(r["notDestroyed"][&cal]["type"], "forbidden", "{r}");
        // Субъект без прав аккаунта не видит.
        let r = call(&env, &ivan, "CalendarEvent/get", json!({"accountId": acc})).await;
        assert_eq!(r["type"], "accountNotFound", "{r}");

        // Право записи (`shareWith/<id>`): событие меняется.
        let write = json!({"mayReadItems": true, "mayWriteAll": true});
        let path = format!("shareWith/{}", id(&anna));
        let r = call(
            &env,
            &owner,
            "Calendar/set",
            json!({"accountId": acc, "update": {&cal: {path.clone(): write}}}),
        )
        .await;
        assert!(r["notUpdated"].is_null(), "{r}");
        allow(&env, &anna, &owner, &cal, R::WRITE);
        let r = call(
            &env,
            &anna,
            "CalendarEvent/set",
            json!({"accountId": acc, "update": {&event: {"title": "Перенесена"}},
                "sendSchedulingMessages": false}),
        )
        .await;
        assert!(r["notUpdated"].is_null(), "{r}");
        let r = call(
            &env,
            &owner,
            "CalendarEvent/get",
            json!({"accountId": acc, "ids": [&event]}),
        )
        .await;
        assert_eq!(r["list"][0]["title"], "Перенесена", "{r}");
        let s = env.jmap.session(&anna, "").await;
        assert_eq!(s["accounts"][&acc]["isReadOnly"], false, "{s}");

        // Уведомления получателю: выдача и изменение прав.
        let anna_acc = id(&anna);
        let r = call(
            &env,
            &anna,
            "ShareNotification/get",
            json!({"accountId": anna_acc}),
        )
        .await;
        let list = r["list"].as_array().cloned().unwrap_or_default();
        assert_eq!(list.len(), 2, "{r}");
        assert_eq!(list[0]["objectType"], "Calendar");
        assert_eq!(list[0]["objectAccountId"], acc);
        assert_eq!(list[0]["objectId"], cal);
        assert_eq!(list[0]["name"], "Команда");
        assert_eq!(list[0]["oldRights"], Value::Null);
        assert_eq!(list[0]["newRights"]["mayReadItems"], true);
        assert_eq!(list[1]["oldRights"]["mayWriteAll"], false);
        assert_eq!(list[1]["newRights"]["mayWriteAll"], true);
        assert_eq!(list[0]["changedBy"]["principalId"], acc);
        let state = r["state"].clone();

        // Отзыв: аккаунт владельца исчезает.
        let r = call(
            &env,
            &owner,
            "Calendar/set",
            json!({"accountId": acc, "update": {&cal: {path: null}}}),
        )
        .await;
        assert!(r["notUpdated"].is_null(), "{r}");
        assert_eq!(entries(&env, &owner, &cal).await, Vec::new());
        allow(&env, &anna, &owner, &cal, R::NONE);
        let r = call(&env, &anna, "CalendarEvent/get", json!({"accountId": acc})).await;
        assert_eq!(r["type"], "accountNotFound", "{r}");
        let s = env.jmap.session(&anna, "").await;
        assert!(s["accounts"][&acc].is_null(), "{s}");
        let r = call(
            &env,
            &owner,
            "Calendar/get",
            json!({"accountId": acc, "ids": [&cal]}),
        )
        .await;
        assert_eq!(r["list"][0]["shareWith"], json!({}), "{r}");

        let r = call(
            &env,
            &anna,
            "ShareNotification/changes",
            json!({"accountId": anna_acc, "sinceState": state}),
        )
        .await;
        assert_eq!(r["created"].as_array().map(Vec::len), Some(1), "{r}");
        let last = r["created"][0].as_str().expect("id").to_owned();
        let r = call(
            &env,
            &anna,
            "ShareNotification/query",
            json!({"accountId": anna_acc, "filter": {"objectType": "Calendar"},
                "sort": [{"property": "created", "isAscending": false}]}),
        )
        .await;
        assert_eq!(r["ids"][0], last, "{r}");
        assert_eq!(r["ids"].as_array().map(Vec::len), Some(3), "{r}");
        let r = call(
            &env,
            &anna,
            "ShareNotification/set",
            json!({"accountId": anna_acc, "destroy": [&last], "create": {"x": {}}}),
        )
        .await;
        assert_eq!(r["destroyed"], json!([last]), "{r}");
        assert_eq!(r["notCreated"]["x"]["type"], "forbidden", "{r}");
        let r = call(
            &env,
            &anna,
            "ShareNotification/get",
            json!({"accountId": anna_acc, "ids": [&last]}),
        )
        .await;
        assert_eq!(r["notFound"], json!([last]), "{r}");
    }

    /// Адресная книга: запись без права — `forbidden`, с правом — карточка видна владельцу; выдачу проверяет единый
    /// слой прав (`check_grant`: владелец или право `admin`); получатель — только учётная запись организации.
    #[tokio::test]
    async fn address_book_rights_and_grants() {
        let env = Env::sharing().await;
        let owner = env.principal.clone();
        let acc = id(&owner);
        let anna = env.user("anna@example.org").await;
        let ivan = env.user("ivan@example.org").await;
        let book = create_book(&env, "Клиенты").await;
        let card = json!({"addressBookIds": {&book: true}, "name": {"full": "Пётр Иванов"}});
        let grant = |rights: Value| json!({"accountId": acc, "update": {&book: {"shareWith": {id(&anna): rights}}}});

        let r = call(
            &env,
            &owner,
            "AddressBook/set",
            json!({"accountId": acc, "update": {&book: {"shareWith": {"nobody": {"mayRead": true}}}}}),
        )
        .await;
        assert_eq!(r["notUpdated"][&book]["type"], "invalidProperties", "{r}");
        let bad = grant(json!({"mayRead": true, "mayFly": true}));
        let r = call(&env, &owner, "AddressBook/set", bad).await;
        assert_eq!(r["notUpdated"][&book]["type"], "invalidProperties", "{r}");

        // Только чтение: создание карточки — `forbidden`.
        let r = call(
            &env,
            &owner,
            "AddressBook/set",
            grant(json!({"mayRead": true})),
        )
        .await;
        assert!(r["notUpdated"].is_null(), "{r}");
        allow(&env, &anna, &owner, &book, R::READ);
        let s = env.jmap.session(&anna, "").await;
        let caps = &s["accounts"][&acc]["accountCapabilities"];
        assert_eq!(
            caps["urn:ietf:params:jmap:contacts"]["mayCreateAddressBook"], false,
            "{s}"
        );
        assert!(caps["urn:ietf:params:jmap:calendars"].is_null(), "{s}");
        let create = json!({"accountId": acc, "create": {"c": card}});
        let r = call(&env, &anna, "ContactCard/set", create.clone()).await;
        assert_eq!(r["notCreated"]["c"]["type"], "forbidden", "{r}");
        let r = call(&env, &anna, "AddressBook/get", json!({"accountId": acc})).await;
        assert_eq!(r["list"].as_array().map(Vec::len), Some(1), "{r}");
        assert_eq!(
            r["list"][0]["myRights"],
            json!({"mayRead": true, "mayWrite": false, "mayShare": false, "mayDelete": false})
        );
        // Без права `admin` выдавать нельзя.
        let to_ivan = json!({"accountId": acc,
            "update": {&book: {format!("shareWith/{}", id(&ivan)): {"mayRead": true}}}});
        let r = call(&env, &anna, "AddressBook/set", to_ivan.clone()).await;
        assert_eq!(r["notUpdated"][&book]["type"], "forbidden", "{r}");

        // Право записи: карточка создаётся и видна владельцу.
        let rw = grant(json!({"mayRead": true, "mayWrite": true}));
        let r = call(&env, &owner, "AddressBook/set", rw).await;
        assert!(r["notUpdated"].is_null(), "{r}");
        allow(&env, &anna, &owner, &book, R::WRITE);
        let r = call(&env, &anna, "ContactCard/set", create).await;
        let created = r["created"]["c"]["id"].as_str().expect("card").to_owned();
        let r = call(
            &env,
            &owner,
            "ContactCard/get",
            json!({"accountId": acc, "ids": [&created]}),
        )
        .await;
        assert_eq!(r["list"][0]["name"]["full"], "Пётр Иванов", "{r}");
        let r = call(
            &env,
            &anna,
            "ContactCard/query",
            json!({"accountId": acc, "filter": {"text": "пётр"}}),
        )
        .await;
        assert_eq!(r["ids"], json!([created]), "{r}");

        // С правом `admin` получатель сам выдаёт права на книгу владельца — от своего имени, запись — у владельца.
        let admin = R {
            admin: true,
            ..R::NONE
        };
        allow(&env, &anna, &owner, &book, admin);
        let r = call(&env, &anna, "AddressBook/set", to_ivan).await;
        assert!(r["notUpdated"].is_null(), "{r}");
        assert!(
            entries(&env, &owner, &book)
                .await
                .contains(&("ivan@example.org".to_owned(), R::READ)),
            "{r}"
        );
        let r = call(
            &env,
            &ivan,
            "ShareNotification/get",
            json!({"accountId": id(&ivan)}),
        )
        .await;
        assert_eq!(r["list"][0]["changedBy"]["principalId"], id(&anna), "{r}");
        assert_eq!(r["list"][0]["objectAccountId"], acc, "{r}");
    }

    /// Без общего доступа `shareWith` — `null`, задать его нельзя.
    #[tokio::test]
    async fn without_sharing() {
        let env = Env::new().await;
        let acc = env.account();
        let anna = env.user("anna@example.org").await;
        let cal = create_calendar(&env, "Личное").await;
        let share = json!({id(&anna): {"mayReadItems": true}});
        let r = call(
            &env,
            &env.principal,
            "Calendar/set",
            json!({"accountId": acc, "update": {&cal: {"shareWith": share}}}),
        )
        .await;
        assert_eq!(r["notUpdated"][&cal]["type"], "invalidProperties", "{r}");
        let r = call(
            &env,
            &env.principal,
            "Calendar/set",
            json!({"accountId": acc, "update": {&cal: {"shareWith": null}}}),
        )
        .await;
        assert!(r["notUpdated"].is_null(), "{r}");
        let r = call(
            &env,
            &env.principal,
            "Calendar/get",
            json!({"accountId": acc}),
        )
        .await;
        assert_eq!(r["list"][0]["shareWith"], Value::Null, "{r}");
        assert_eq!(r["list"][0]["myRights"]["mayShare"], false, "{r}");
    }
}
