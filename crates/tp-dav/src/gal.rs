//! Глобальная адресная книга (GAL, SRS 4.4; решение 72): книга «Организация» только для чтения в доме адресных книг
//! каждого пользователя — `/dav/addressbooks/<логин>/gal/`.
//!
//! Книга хранится в [`DavStore`] (CTR-60, `Collection::is_gal`) и отдаётся как обычная адресная книга: ETag
//! карточки, `getctag` и токен синхронизации — от хранилища, одинаково для CardDAV и JMAP. Запись (`PUT`, `DELETE`,
//! `PROPPATCH`, `MKCOL`) — 403 `DAV:need-privileges`; права книги и карточек — только чтение.
//!
//! Состав строит [`sync`] из каталога (`Directory`) и записывает [`DavStore::sync_gal`]: учётные записи с
//! разрешённым входом и адресом, кроме скрытых из GAL (`DirAccount::hidden_from_gal`, CTR-58), и группы с адресом
//! рассылки; группа, скрытая из GAL (`hidden_from_gal` в `GroupStore`, поиск по идентификатору каталога), не
//! показывается. Карточка (calcard, vCard 3.0; по запросу клиента — 4.0): `UID` — идентификатор в
//! каталоге, `FN`, `EMAIL` (первый — основной), телефоны из атрибутов `telephoneNumber` и `mobile`; группа —
//! `KIND:group` (`X-ADDRESSBOOKSERVER-KIND` в 3.0). Имя ресурса — идентификатор каталога с `.vcf`. Неизменная
//! карточка хранилищем не перезаписывается: после изменения каталога новую версию получает только изменённая.
//! Оргструктура (подразделение, должность, руководитель) — DAV-22. Вызывает [`sync`] узел: при старте роли и по
//! событию изменения каталога (APP-49). Пока [`sync`] не вызывался, книги нет.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::Response;
use calcard::vcard::{
    VCard, VCardEntry, VCardKind, VCardParameter, VCardProperty, VCardType, VCardVersion,
};
use tp_contracts::auth::{DirAccount, DirChange, DirCursor, DirGroup, DirPage, Directory};
use tp_contracts::dav::{CollectionKind, DavStore, GAL_COLLECTION_NAME, GalCard, valid_name};
use tp_contracts::groups::GroupStore;
use tp_contracts::{ModSeq, Result, TenantId};

use crate::card;
use crate::config::DavConfig;
use crate::index;
use crate::path::Target;
use crate::server::{Cx, precondition};
use crate::xml;

/// Источники книги GAL ([`sync`]).
#[derive(Clone)]
pub struct GalDeps {
    /// Каталог организации: учётные записи и группы.
    pub directory: Arc<dyn Directory>,
    /// Группы рассылки: скрытые из адресной книги (`hidden_from_gal`) в неё не попадают; `None` — видны все
    /// группы каталога.
    pub groups: Option<Arc<dyn GroupStore>>,
}

impl fmt::Debug for GalDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GalDeps").finish_non_exhaustive()
    }
}

/// Строит книгу GAL организации `tenant` из каталога `source` (порциями по `dav.gal_page_size`) и записывает её в
/// `dav` ([`DavStore::sync_gal`]) с отображаемым именем `dav.gal_name`. Возвращает состояние книги после записи.
/// Ошибка каталога или хранилища — книга прежняя.
pub async fn sync(
    source: &GalDeps,
    dav: &dyn DavStore,
    tenant: TenantId,
    cfg: &DavConfig,
) -> Result<ModSeq> {
    let dir = source.directory.as_ref();
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
    let hidden: HashSet<String> = match source.groups.as_deref() {
        Some(store) => store
            .list(tenant)
            .await?
            .into_iter()
            .filter(|g| g.hidden_from_gal)
            .filter_map(|g| g.external_id)
            .collect(),
        None => HashSet::new(),
    };
    let cards = accounts.items.values().filter_map(account_card).chain(
        groups
            .items
            .values()
            .filter(|g| !hidden.contains(&g.external_id))
            .filter_map(group_card),
    );
    dav.sync_gal(tenant, &cfg.gal_name, gal_cards(cards)).await
}

/// Запрос к книге GAL или её карточке (имя зарезервировано, [`crate::path::reserved`]).
pub(crate) fn is_target(target: &Target) -> bool {
    matches!(
        target,
        Target::Collection(CollectionKind::AddressBook, _, name)
        | Target::Object(CollectionKind::AddressBook, _, name, _) if name == GAL_COLLECTION_NAME
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

/// Карточка учётной записи; вход запрещён, запись скрыта из адресной книги или нет адреса — `None`.
fn account_card(a: &DirAccount) -> Option<Card> {
    if !a.enabled || a.hidden_from_gal || a.emails.is_empty() {
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

/// Набор [`DavStore::sync_gal`]: индекс — [`index::card`] (поисковые строки, как у карточек пользователя). Из
/// карточек с совпавшим именем ресурса или UID остаётся первая; недопустимое имя или индекс — карточка пропущена.
fn gal_cards(cards: impl Iterator<Item = Card>) -> Vec<GalCard> {
    let mut names = BTreeSet::new();
    let mut uids = BTreeSet::new();
    let mut out = Vec::new();
    for (uid, body) in cards {
        let name = resource_name(&uid);
        if !valid_name(&name) || names.contains(&name) || uids.contains(&uid) {
            tracing::warn!(uid = %uid, "GAL: карточка пропущена (имя ресурса или UID)");
            continue;
        }
        let index = match index::card(&body) {
            Ok(index) => index,
            Err(e) => {
                tracing::warn!(uid = %uid, error = %e, "GAL: карточка пропущена (индекс)");
                continue;
            }
        };
        names.insert(name.clone());
        uids.insert(uid);
        out.push(GalCard { name, body, index });
    }
    out
}
