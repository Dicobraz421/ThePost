//! Глобальная адресная книга (GAL, SRS 4.4): книга «Организация» только для чтения в доме адресных книг каждого
//! пользователя — `/dav/addressbooks/<логин>/gal/` (имя — `dav.gal_name`). Есть, если приложение передало каталог
//! ([`crate::Deps::directory`]).
//!
//! Состав — снимок каталога (`Directory`): учётные записи с разрешённым входом и адресом и группы с адресом
//! рассылки; группа, скрытая из GAL (`hidden_from_gal` в `GroupStore`, поиск по идентификатору каталога), не
//! показывается. Карточки строятся на лету (calcard, vCard 3.0; по запросу — 4.0, [`crate::card`]): `UID` —
//! идентификатор в каталоге, `FN`, `EMAIL` (первый — основной), телефоны из атрибутов `telephoneNumber` и `mobile`;
//! группа — `KIND:group` (`X-ADDRESSBOOKSERVER-KIND` в 3.0). Оргструктура (подразделение, должность, руководитель) —
//! DAV-22.
//!
//! Версия снимка — хеш состава и содержимого карточек: она же ETag каждой карточки, `getctag` и токен
//! синхронизации книги. `sync-collection`: пустой токен — все карточки, токен текущего снимка — без изменений, токен
//! прежнего снимка — 403 `valid-sync-token` (клиент повторяет полную синхронизацию и так узнаёт и об удалённых).
//! Снимок хранится в памяти процесса `dav.gal_cache` (каталог не опрашивается на каждый запрос). Запись (`PUT`,
//! `DELETE`, `PROPPATCH`, `MKCOL`) — 403 `DAV:need-privileges`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use axum::http::StatusCode;
use axum::response::Response;
use calcard::vcard::{
    VCard, VCardEntry, VCardKind, VCardParameter, VCardProperty, VCardType, VCardVersion,
};
use tp_contracts::auth::{DirAccount, DirChange, DirCursor, DirGroup, DirPage};
use tp_contracts::dav::{
    Collection, CollectionId, CollectionKind, Component, DavObject, DavObjectId, ObjectIndex,
};
use tp_contracts::{ModSeq, Result, TenantId};

use crate::card;
use crate::path::{self, Target};
use crate::props::{self, Res, Want};
use crate::server::{Cx, multistatus, precondition};
use crate::sync::{SyncRequest, invalid_token};
use crate::xml::{self, Multistatus};

/// Снимок глобальной адресной книги.
#[derive(Debug)]
pub(crate) struct Gal {
    /// Книга: `state` — версия снимка.
    pub coll: Collection,
    /// Карточки по имени ресурса.
    pub cards: Vec<DavObject>,
}

impl Gal {
    /// Карточка по имени ресурса.
    pub(crate) fn card(&self, name: &str) -> Option<&DavObject> {
        self.cards.iter().find(|o| o.name == name)
    }
}

/// Снимки по организациям с моментом построения.
#[derive(Debug, Default)]
pub(crate) struct Cache(Mutex<HashMap<TenantId, (Instant, Arc<Gal>)>>);

/// Коллекция — глобальная адресная книга (имя зарезервировано, [`path::reserved`]).
pub(crate) fn is(c: &Collection) -> bool {
    c.kind == CollectionKind::AddressBook && c.name == path::GAL
}

/// Запрос к глобальной адресной книге или её карточке (каталог подключён).
pub(crate) fn is_target(cx: &Cx<'_>, target: &Target) -> bool {
    cx.dav.d.directory.is_some()
        && matches!(
            target,
            Target::Collection(CollectionKind::AddressBook, _, name)
            | Target::Object(CollectionKind::AddressBook, _, name, _) if name == path::GAL
        )
}

/// 403 `DAV:need-privileges` (RFC 3744 §7.1.1): запись в книгу только для чтения.
pub(crate) fn read_only(cx: &Cx<'_>, target: &Target) -> Response {
    precondition(
        StatusCode::FORBIDDEN,
        &format!(
            "<d:need-privileges><d:resource>{}<d:privilege><d:write/></d:privilege></d:resource></d:need-privileges>",
            xml::href(&target.href(cx.login()))
        ),
    )
}

/// Снимок, если коллекция `name` вида `kind` — глобальная адресная книга и каталог подключён.
pub(crate) async fn find(
    cx: &Cx<'_>,
    kind: CollectionKind,
    name: &str,
) -> Result<Option<Arc<Gal>>> {
    if kind == CollectionKind::AddressBook && name == path::GAL {
        load(cx).await
    } else {
        Ok(None)
    }
}

/// Снимок организации вошедшего: из памяти, если он не старше `dav.gal_cache`, иначе из каталога. Каталог не
/// подключён — `None`.
pub(crate) async fn load(cx: &Cx<'_>) -> Result<Option<Arc<Gal>>> {
    let Some(dir) = cx.dav.d.directory.as_deref() else {
        return Ok(None);
    };
    let tenant = cx.scope.tenant;
    let cfg = &cx.dav.cfg;
    let cached = cx
        .dav
        .gal
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&tenant)
        .filter(|(at, _)| at.elapsed() < cfg.gal_cache)
        .map(|(_, g)| Arc::clone(g));
    if let Some(g) = cached {
        return Ok(Some(g));
    }
    let limit = cfg.gal_page_size.max(1);
    let mut accounts = Dump::new(|a: &DirAccount| a.external_id.clone());
    while accounts.more {
        let page = dir
            .accounts_since(tenant, accounts.cursor.as_ref(), limit)
            .await?;
        accounts.apply(page);
    }
    let mut groups = Dump::new(|g: &DirGroup| g.external_id.clone());
    while groups.more {
        let page = dir
            .groups_since(tenant, groups.cursor.as_ref(), limit)
            .await?;
        groups.apply(page);
    }
    let hidden: HashSet<String> = match cx.dav.d.groups.as_deref() {
        Some(store) => store
            .list(tenant)
            .await?
            .into_iter()
            .filter(|g| g.hidden_from_gal)
            .filter_map(|g| g.external_id)
            .collect(),
        None => HashSet::new(),
    };
    let cards = accounts
        .items
        .values()
        .filter_map(account_card)
        .chain(
            groups
                .items
                .values()
                .filter(|g| !hidden.contains(&g.external_id))
                .filter_map(group_card),
        )
        .collect();
    let gal = Arc::new(build(&cfg.gal_name, cards));
    cx.dav
        .gal
        .0
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(tenant, (Instant::now(), Arc::clone(&gal)));
    Ok(Some(gal))
}

/// Полная выгрузка записей каталога порциями: записи по идентификатору каталога (`id`).
struct Dump<T> {
    items: BTreeMap<String, T>,
    id: fn(&T) -> String,
    cursor: Option<DirCursor>,
    more: bool,
}

impl<T> Dump<T> {
    fn new(id: fn(&T) -> String) -> Self {
        Self {
            items: BTreeMap::new(),
            id,
            cursor: None,
            more: true,
        }
    }

    fn apply(&mut self, page: DirPage<T>) {
        for change in page.changes {
            match change {
                DirChange::Upsert(x) => {
                    self.items.insert((self.id)(&x), x);
                }
                DirChange::Removed { external_id } => {
                    self.items.remove(&external_id);
                }
            }
        }
        self.more = page.has_more;
        self.cursor = Some(page.next);
    }
}

/// Идентификатор каталога и тело карточки (vCard 3.0).
type Card = (String, String);

fn entry(name: VCardProperty, value: &str) -> VCardEntry {
    VCardEntry::new(name).with_value(value.to_owned())
}

fn write(uid: &str, entries: Vec<VCardEntry>) -> Option<Card> {
    let body = card::convert(&VCard { entries }, VCardVersion::V3_0)?;
    Some((uid.to_owned(), body))
}

/// Карточка учётной записи; вход запрещён или нет адреса — `None`.
fn account_card(a: &DirAccount) -> Option<Card> {
    if !a.enabled || a.emails.is_empty() {
        return None;
    }
    let name = if a.display_name.trim().is_empty() {
        &a.login
    } else {
        &a.display_name
    };
    let mut entries = vec![
        entry(VCardProperty::Uid, &a.external_id),
        entry(VCardProperty::Fn, name),
    ];
    for (i, email) in a.emails.iter().enumerate() {
        let mut e =
            entry(VCardProperty::Email, email).with_param(VCardParameter::typ(VCardType::Work));
        if i == 0 {
            e.add_param(VCardParameter::pref(1u32));
        }
        entries.push(e);
    }
    for (attr, typ) in [
        ("telephoneNumber", VCardType::Work),
        ("mobile", VCardType::Cell),
    ] {
        if let Some(tel) = a.attributes.get(attr).filter(|t| !t.trim().is_empty()) {
            entries.push(entry(VCardProperty::Tel, tel).with_param(VCardParameter::typ(typ)));
        }
    }
    write(&a.external_id, entries)
}

/// Карточка группы; нет адреса рассылки — `None`.
fn group_card(g: &DirGroup) -> Option<Card> {
    let email = g.email.as_deref()?;
    let entries = vec![
        entry(VCardProperty::Uid, &g.external_id),
        VCardEntry::new(VCardProperty::Kind).with_value(VCardKind::Group),
        entry(VCardProperty::Fn, &g.name),
        entry(VCardProperty::Email, email).with_param(VCardParameter::typ(VCardType::Work)),
    ];
    write(&g.external_id, entries)
}

/// Имя ресурса карточки: идентификатор каталога (символы вне букв, цифр ASCII и `-_.` заменены на `_`) и `.vcf`.
fn resource_name(uid: &str) -> String {
    let safe: String = uid
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    format!("{safe}.vcf")
}

/// Снимок из карточек: версия — хеш имён и тел.
fn build(display_name: &str, cards: Vec<Card>) -> Gal {
    let mut named: BTreeMap<String, Card> = BTreeMap::new();
    for (uid, body) in cards {
        named.entry(resource_name(&uid)).or_insert((uid, body));
    }
    let mut hasher = DefaultHasher::new();
    for (name, (_, body)) in &named {
        (name, body).hash(&mut hasher);
    }
    // Старший бит свободен: версия помещается в знаковое 64-битное число.
    let version = hasher.finish() >> 1;
    let coll = Collection {
        id: CollectionId::new(),
        display_name: display_name.to_owned(),
        state: ModSeq(version),
        ..Collection::new(CollectionKind::AddressBook, path::GAL)
    };
    let etag = format!("{version:x}");
    let cards = named
        .into_iter()
        .map(|(name, (uid, body))| DavObject {
            id: DavObjectId::new(),
            collection: coll.id,
            name,
            etag: etag.clone(),
            body,
            index: ObjectIndex::new(Component::Card, uid, None),
        })
        .collect();
    Gal { coll, cards }
}

/// `response` карточки книги.
pub(crate) fn emit(cx: &Cx<'_>, ms: &mut Multistatus, g: &Gal, o: &DavObject, want: &Want) {
    let res = Res::Object {
        coll: &g.coll,
        name: &o.name,
        etag: &o.etag,
        body: want.needs_body().then_some(o.body.as_str()),
    };
    props::emit(cx, ms, &res, want);
}

/// `sync-collection` книги (см. описание модуля).
pub(crate) fn sync(cx: &Cx<'_>, g: &Gal, req: SyncRequest) -> Response {
    if req.since != ModSeq::ZERO && req.since != g.coll.state {
        return invalid_token();
    }
    let mut ms = Multistatus::new();
    if req.since == ModSeq::ZERO {
        let want = Want::Props(req.props);
        for o in &g.cards {
            emit(cx, &mut ms, g, o, &want);
        }
    }
    ms.sync_token(&props::sync_token(g.coll.state));
    multistatus(ms)
}
