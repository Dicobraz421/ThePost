//! [`DavStore`] и [`FreeBusy`] в памяти (CONTRACTS.md#Dav, CONTRACTS.md#Schedule; CTR-45, CTR-47).
//!
//! `ModSeq` — свой счётчик учётной записи (у `PgMeta` — общий с письмами), ETag — `ModSeq` записи. С шиной
//! ([`MemDav::with_bus`]) после каждого изменения публикуется `DavChanged`, после изменения входящих планирования —
//! `ScheduleInboxChanged`. [`MemDav::purge_tombstones`] имитирует очистку надгробий: прежние `since` после неё —
//! `Conflict`. Книга GAL (CTR-60) — своя «учётная запись» организации с одной коллекцией и своим счётчиком; ETag
//! карточки — `ModSeq` её записи (неизменная карточка при `sync_gal` не перезаписывается), сигнал — `GalChanged`.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::bus::EventBus;
use tp_contracts::dav::{
    Collection, CollectionId, CollectionKind, DavLimits, DavObject, DavObjectId, DavStore,
    DeleteOutcome, GAL_COLLECTION_NAME, GalCard, MoveOutcome, Moved, ObjectIndex, ObjectRef,
    Precondition, Put, PutOutcome, TimeRange, dav_changed, gal_changed, valid_name, validate_gal,
};
use tp_contracts::meta::{AccountScope, Changes};
use tp_contracts::schedule::{
    BusyPeriod, FreeBusy, Inbox, InboxItem, InboxMessage, Period, merge_busy,
    schedule_inbox_changed,
};
use tp_contracts::sharing::{
    CollectionAcl, CollectionAclEntry, CollectionRights, SharedCollection, collection_rights,
    validate_identifier,
};
use tp_contracts::{AccountId, Error, ModSeq, Result, TenantId};

use crate::infra::lock;

#[derive(Debug, Clone)]
struct Obj {
    id: DavObjectId,
    etag: String,
    body: String,
    index: ObjectIndex,
    /// Создание объекта (для изменений учётной записи: перенос идентификатор не меняет).
    created: ModSeq,
    /// Появление в нынешней коллекции (создание или перенос).
    placed: ModSeq,
    modseq: ModSeq,
}

#[derive(Debug)]
struct Coll {
    meta: Collection,
    created: ModSeq,
    props: ModSeq,
    objects: BTreeMap<String, Obj>,
    /// Права на коллекцию по субъектам (CTR-48), как выданы; удаляются с коллекцией.
    acl: BTreeMap<String, CollectionRights>,
}

#[derive(Debug, Clone)]
struct Tomb {
    collection: CollectionId,
    kind: CollectionKind,
    id: DavObjectId,
    name: String,
    modseq: ModSeq,
}

#[derive(Debug, Default)]
struct Account {
    counter: ModSeq,
    /// Надгробия с `ModSeq` не больше границы очищены.
    horizon: ModSeq,
    collections: HashMap<CollectionId, Coll>,
    objects: Vec<Tomb>,
    deleted: Vec<(CollectionId, CollectionKind, ModSeq)>,
    /// Входящие планирования по имени.
    inbox: BTreeMap<String, InboxItem>,
    /// `ModSeq` последнего изменения входящих.
    inbox_state: ModSeq,
    /// `ModSeq` создания живых ресурсов входящих (CTR-65: `created` или `updated` в `inbox_changes`).
    inbox_created: HashMap<DavObjectId, ModSeq>,
    /// Надгробия входящих: идентификатор и `ModSeq` удаления.
    inbox_gone: Vec<(DavObjectId, ModSeq)>,
}

impl Account {
    fn bump(&mut self) -> ModSeq {
        self.counter = self.counter.next();
        self.counter
    }

    fn coll(&self, id: CollectionId) -> Result<&Coll> {
        self.collections.get(&id).ok_or(Error::NotFound)
    }

    /// Снимает признак коллекции по умолчанию с коллекции вида `kind`, кроме `keep`: её изменение свойств со своим
    /// `ModSeq`.
    fn clear_default(
        &mut self,
        kind: CollectionKind,
        keep: CollectionId,
    ) -> Option<(CollectionId, ModSeq)> {
        let prev = self
            .collections
            .values()
            .find(|c| c.meta.kind == kind && c.meta.is_default && c.meta.id != keep)
            .map(|c| c.meta.id)?;
        let modseq = self.bump();
        let c = self.collections.get_mut(&prev)?;
        c.meta.is_default = false;
        c.meta.state = modseq;
        c.props = modseq;
        Some((prev, modseq))
    }

    fn check_since(&self, since: ModSeq) -> Result<()> {
        if since > self.counter || (since != ModSeq::ZERO && since < self.horizon) {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    fn state(&self, kind: CollectionKind) -> ModSeq {
        let live = self
            .collections
            .values()
            .filter(|c| c.meta.kind == kind)
            .map(|c| c.meta.state);
        let gone = self
            .deleted
            .iter()
            .filter(|d| d.1 == kind)
            .map(|d| d.2)
            .chain(
                self.objects
                    .iter()
                    .filter(|t| t.kind == kind)
                    .map(|t| t.modseq),
            );
        live.chain(gone).max().unwrap_or(ModSeq::ZERO)
    }
}

#[derive(Debug, Default)]
struct State {
    accounts: HashMap<AccountScope, Account>,
    /// Книги GAL по организациям: одна коллекция, свой счётчик и надгробия.
    gal: HashMap<TenantId, Account>,
}

impl State {
    /// Коллекция `id`, видимая учётной записи `scope` (своя или книга GAL организации), с её владельцем: счётчик и
    /// надгробия.
    fn find(&self, scope: AccountScope, id: CollectionId) -> Result<(&Account, &Coll)> {
        if let Some(acc) = self.accounts.get(&scope)
            && let Some(c) = acc.collections.get(&id)
        {
            return Ok((acc, c));
        }
        let gal = self.gal.get(&scope.tenant).ok_or(Error::NotFound)?;
        Ok((gal, gal.coll(id)?))
    }

    /// `id` — книга GAL организации `tenant`.
    fn is_gal(&self, tenant: TenantId, id: CollectionId) -> bool {
        self.gal
            .get(&tenant)
            .is_some_and(|g| g.collections.contains_key(&id))
    }

    /// Книга GAL организации `tenant`, если есть.
    fn gal(&self, tenant: TenantId) -> Option<&Coll> {
        self.gal
            .get(&tenant)
            .and_then(|g| g.collections.values().next())
    }
}

fn object(collection: CollectionId, name: &str, o: &Obj) -> DavObject {
    DavObject {
        id: o.id,
        collection,
        name: name.to_owned(),
        etag: o.etag.clone(),
        body: o.body.clone(),
        index: o.index.clone(),
    }
}

/// Изменение для страницы `/changes`.
enum Change<T> {
    Created(T),
    Updated(T),
    Destroyed(T),
}

/// Страница изменений после `since`: по `ModSeq` (у каждого изменения свой), не больше `limit`.
fn page<T>(
    mut all: Vec<(ModSeq, Change<T>)>,
    since: ModSeq,
    limit: usize,
    complete: ModSeq,
) -> Changes<T> {
    all.sort_by_key(|(m, _)| *m);
    let has_more = all.len() > limit;
    all.truncate(limit);
    let new_state = if has_more {
        all.last().map_or(since, |(m, _)| *m)
    } else {
        complete.max(since)
    };
    let mut out = Changes {
        created: Vec::new(),
        updated: Vec::new(),
        destroyed: Vec::new(),
        new_state,
        has_more,
    };
    for (_, c) in all {
        match c {
            Change::Created(t) => out.created.push(t),
            Change::Updated(t) => out.updated.push(t),
            Change::Destroyed(t) => out.destroyed.push(t),
        }
    }
    out
}

fn live<T>(created: ModSeq, since: ModSeq, t: T) -> Change<T> {
    if created > since {
        Change::Created(t)
    } else {
        Change::Updated(t)
    }
}

/// Календари и контакты в памяти. Клоны разделяют данные.
#[derive(Clone, Default)]
pub struct MemDav {
    state: Arc<Mutex<State>>,
    limits: DavLimits,
    bus: Option<Arc<dyn EventBus>>,
}

impl fmt::Debug for MemDav {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemDav")
            .field("state", &self.state)
            .field("limits", &self.limits)
            .field("bus", &self.bus.is_some())
            .finish()
    }
}

impl MemDav {
    /// Пустое хранилище с лимитами по умолчанию.
    pub fn new() -> Self {
        Self::default()
    }

    /// Лимиты `limits`.
    pub fn with_limits(mut self, limits: DavLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Публиковать `DavChanged` в `bus` после каждого изменения.
    pub fn with_bus(mut self, bus: Arc<dyn EventBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Очищает надгробия учётной записи: `since` меньше текущего счётчика (кроме `ModSeq::ZERO`) — `Conflict`.
    pub fn purge_tombstones(&self, scope: AccountScope) {
        let mut st = lock(&self.state);
        let acc = st.accounts.entry(scope).or_default();
        acc.horizon = acc.counter;
        acc.objects.clear();
        acc.deleted.clear();
        acc.inbox_gone.clear();
    }

    async fn inbox_signal(&self, scope: AccountScope, modseq: ModSeq) {
        if let Some(bus) = &self.bus {
            let _ = bus.publish(schedule_inbox_changed(scope, modseq)).await;
        }
    }

    async fn signal(
        &self,
        scope: AccountScope,
        kind: CollectionKind,
        id: CollectionId,
        modseq: ModSeq,
    ) {
        if let Some(bus) = &self.bus {
            // Сигнал после «фиксации»; доставка at-most-once, ошибка шины не отменяет изменение.
            let _ = bus.publish(dav_changed(scope, kind, id, modseq)).await;
        }
    }
}

#[async_trait]
impl DavStore for MemDav {
    fn limits(&self) -> DavLimits {
        self.limits
    }

    async fn list_collections(
        &self,
        scope: AccountScope,
        kind: CollectionKind,
    ) -> Result<Vec<Collection>> {
        let st = lock(&self.state);
        let mut out: Vec<Collection> = st
            .accounts
            .get(&scope)
            .map(|a| {
                a.collections
                    .values()
                    .filter(|c| c.meta.kind == kind)
                    .map(|c| c.meta.clone())
                    .collect()
            })
            .unwrap_or_default();
        out.extend(
            st.gal(scope.tenant)
                .filter(|g| g.meta.kind == kind)
                .map(|g| g.meta.clone()),
        );
        out.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }

    async fn get_collection(
        &self,
        scope: AccountScope,
        id: CollectionId,
    ) -> Result<Option<Collection>> {
        Ok(lock(&self.state)
            .find(scope, id)
            .ok()
            .map(|(_, c)| c.meta.clone()))
    }

    async fn collection_by_name(
        &self,
        scope: AccountScope,
        kind: CollectionKind,
        name: &str,
    ) -> Result<Option<Collection>> {
        let st = lock(&self.state);
        let own = st.accounts.get(&scope).and_then(|a| {
            a.collections
                .values()
                .find(|c| c.meta.kind == kind && c.meta.name == name)
        });
        Ok(own
            .or_else(|| {
                st.gal(scope.tenant)
                    .filter(|g| g.meta.kind == kind && g.meta.name == name)
            })
            .map(|c| c.meta.clone()))
    }

    async fn create_collection(
        &self,
        scope: AccountScope,
        mut collection: Collection,
    ) -> Result<ModSeq> {
        collection.validate()?;
        if collection.is_gal() {
            return Err(Error::Conflict);
        }
        let (kind, id) = (collection.kind, collection.id);
        let (cleared, modseq) = {
            let mut st = lock(&self.state);
            let acc = st.accounts.entry(scope).or_default();
            let taken = acc.collections.contains_key(&id)
                || acc.deleted.iter().any(|d| d.0 == id)
                || acc
                    .collections
                    .values()
                    .any(|c| c.meta.kind == kind && c.meta.name == collection.name);
            if taken {
                return Err(Error::Conflict);
            }
            if acc.collections.len() >= self.limits.max_collections {
                return Err(Error::QuotaExceeded);
            }
            let cleared = collection
                .is_default
                .then(|| acc.clear_default(kind, id))
                .flatten();
            let modseq = acc.bump();
            collection.state = modseq;
            acc.collections.insert(
                id,
                Coll {
                    meta: collection,
                    created: modseq,
                    props: modseq,
                    objects: BTreeMap::new(),
                    acl: BTreeMap::new(),
                },
            );
            (cleared, modseq)
        };
        if let Some((prev, m)) = cleared {
            self.signal(scope, kind, prev, m).await;
        }
        self.signal(scope, kind, id, modseq).await;
        Ok(modseq)
    }

    async fn update_collection(
        &self,
        scope: AccountScope,
        collection: Collection,
    ) -> Result<ModSeq> {
        collection.validate()?;
        let (kind, id) = (collection.kind, collection.id);
        let (cleared, modseq) = {
            let mut st = lock(&self.state);
            if st.is_gal(scope.tenant, id) {
                return Err(Error::Forbidden);
            }
            let acc = st.accounts.get_mut(&scope).ok_or(Error::NotFound)?;
            let cur = acc.coll(id)?;
            if cur.meta.kind != kind || cur.meta.name != collection.name {
                return Err(Error::invalid("вид и имя коллекции не меняются"));
            }
            let cleared = collection
                .is_default
                .then(|| acc.clear_default(kind, id))
                .flatten();
            let modseq = acc.bump();
            let c = acc.collections.get_mut(&id).ok_or(Error::NotFound)?;
            c.meta = Collection {
                state: modseq,
                ..collection
            };
            c.props = modseq;
            (cleared, modseq)
        };
        if let Some((prev, m)) = cleared {
            self.signal(scope, kind, prev, m).await;
        }
        self.signal(scope, kind, id, modseq).await;
        Ok(modseq)
    }

    async fn delete_collection(&self, scope: AccountScope, id: CollectionId) -> Result<ModSeq> {
        let (kind, modseq) = {
            let mut st = lock(&self.state);
            if st.is_gal(scope.tenant, id) {
                return Err(Error::Forbidden);
            }
            let acc = st.accounts.get_mut(&scope).ok_or(Error::NotFound)?;
            let c = acc.collections.remove(&id).ok_or(Error::NotFound)?;
            let kind = c.meta.kind;
            for (name, o) in c.objects {
                let modseq = acc.bump();
                acc.objects.push(Tomb {
                    collection: id,
                    kind,
                    id: o.id,
                    name,
                    modseq,
                });
            }
            let modseq = acc.bump();
            acc.deleted.push((id, kind, modseq));
            (kind, modseq)
        };
        self.signal(scope, kind, id, modseq).await;
        Ok(modseq)
    }

    async fn get_object(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        name: &str,
    ) -> Result<Option<DavObject>> {
        let st = lock(&self.state);
        let (_, c) = st.find(scope, collection)?;
        Ok(c.objects.get(name).map(|o| object(collection, name, o)))
    }

    async fn object_by_uid(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        uid: &str,
    ) -> Result<Option<DavObject>> {
        let st = lock(&self.state);
        let (_, c) = st.find(scope, collection)?;
        Ok(c.objects
            .iter()
            .find(|(_, o)| o.index.uid == uid)
            .map(|(n, o)| object(collection, n, o)))
    }

    async fn list_objects(
        &self,
        scope: AccountScope,
        collection: CollectionId,
    ) -> Result<Vec<ObjectRef>> {
        let st = lock(&self.state);
        let (_, c) = st.find(scope, collection)?;
        Ok(c.objects
            .iter()
            .map(|(name, o)| ObjectRef {
                id: o.id,
                name: name.clone(),
                etag: o.etag.clone(),
            })
            .collect())
    }

    async fn multiget(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        names: &[String],
    ) -> Result<Vec<DavObject>> {
        let st = lock(&self.state);
        let (_, c) = st.find(scope, collection)?;
        Ok(names
            .iter()
            .filter_map(|n| c.objects.get(n).map(|o| object(collection, n, o)))
            .collect())
    }

    async fn objects_by_id(
        &self,
        scope: AccountScope,
        ids: &[DavObjectId],
    ) -> Result<Vec<DavObject>> {
        let st = lock(&self.state);
        let colls: Vec<(&CollectionId, &Coll)> = st
            .accounts
            .get(&scope)
            .into_iter()
            .chain(st.gal.get(&scope.tenant))
            .flat_map(|a| a.collections.iter())
            .collect();
        Ok(ids
            .iter()
            .filter_map(|id| {
                colls.iter().find_map(|(cid, c)| {
                    c.objects
                        .iter()
                        .find(|(_, o)| o.id == *id)
                        .map(|(n, o)| object(**cid, n, o))
                })
            })
            .collect())
    }

    async fn in_range(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        range: TimeRange,
    ) -> Result<Vec<DavObject>> {
        let st = lock(&self.state);
        let (_, c) = st.find(scope, collection)?;
        Ok(c.objects
            .iter()
            .filter(|(_, o)| o.index.may_overlap(&range))
            .map(|(n, o)| object(collection, n, o))
            .collect())
    }

    async fn put_object(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        name: &str,
        body: String,
        index: ObjectIndex,
        pre: Precondition,
    ) -> Result<PutOutcome> {
        let (kind, put) = {
            let mut st = lock(&self.state);
            if st.is_gal(scope.tenant, collection) {
                return Err(Error::Forbidden);
            }
            let acc = st.accounts.get_mut(&scope).ok_or(Error::NotFound)?;
            let c = acc.coll(collection)?;
            if !valid_name(name) {
                return Err(Error::invalid(format!(
                    "недопустимое имя ресурса: {name:?}"
                )));
            }
            index.validate()?;
            if !c.meta.components.contains(&index.component) {
                return Err(Error::invalid(format!(
                    "компонент {} не поддерживается коллекцией",
                    index.component
                )));
            }
            if body.len() > self.limits.max_object_size {
                return Err(Error::QuotaExceeded);
            }
            let cur = c.objects.get(name);
            let ok = match &pre {
                Precondition::Any => true,
                Precondition::IfMatch(etag) => cur.is_some_and(|o| o.etag == *etag),
                Precondition::IfNoneMatch => cur.is_none(),
            };
            if !ok {
                return Ok(PutOutcome::PreconditionFailed);
            }
            if let Some((other, _)) = c
                .objects
                .iter()
                .find(|(n, o)| n.as_str() != name && o.index.uid == index.uid)
            {
                return Ok(PutOutcome::UidConflict {
                    name: other.clone(),
                });
            }
            if cur.is_none() && c.objects.len() >= self.limits.max_objects {
                return Err(Error::QuotaExceeded);
            }
            let kind = c.meta.kind;
            let prev = cur.map(|o| (o.id, o.created, o.placed));
            let modseq = acc.bump();
            let (id, created, placed) = prev.unwrap_or((DavObjectId::new(), modseq, modseq));
            let etag = modseq.to_string();
            let c = acc
                .collections
                .get_mut(&collection)
                .ok_or(Error::NotFound)?;
            c.objects.insert(
                name.to_owned(),
                Obj {
                    id,
                    etag: etag.clone(),
                    body,
                    index,
                    created,
                    placed,
                    modseq,
                },
            );
            c.meta.state = modseq;
            (
                kind,
                Put {
                    id,
                    etag,
                    modseq,
                    created: prev.is_none(),
                },
            )
        };
        self.signal(scope, kind, collection, put.modseq).await;
        Ok(PutOutcome::Stored(put))
    }

    async fn delete_object(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        name: &str,
        pre: Precondition,
    ) -> Result<DeleteOutcome> {
        let (kind, modseq) = {
            let mut st = lock(&self.state);
            if st.is_gal(scope.tenant, collection) {
                return Err(Error::Forbidden);
            }
            let acc = st.accounts.get_mut(&scope).ok_or(Error::NotFound)?;
            let c = acc.coll(collection)?;
            let cur = c.objects.get(name);
            match (&pre, cur) {
                (Precondition::Any, None) => return Err(Error::NotFound),
                (Precondition::IfMatch(etag), Some(o)) if o.etag == *etag => {}
                (Precondition::IfMatch(_), _) | (Precondition::IfNoneMatch, Some(_)) => {
                    return Ok(DeleteOutcome::PreconditionFailed);
                }
                (Precondition::IfNoneMatch, None) => return Err(Error::NotFound),
                (Precondition::Any, Some(_)) => {}
            }
            let kind = c.meta.kind;
            let modseq = acc.bump();
            let c = acc
                .collections
                .get_mut(&collection)
                .ok_or(Error::NotFound)?;
            let o = c.objects.remove(name).ok_or(Error::NotFound)?;
            c.meta.state = modseq;
            acc.objects.push(Tomb {
                collection,
                kind,
                id: o.id,
                name: name.to_owned(),
                modseq,
            });
            (kind, modseq)
        };
        self.signal(scope, kind, collection, modseq).await;
        Ok(DeleteOutcome::Deleted(modseq))
    }

    async fn move_object(
        &self,
        scope: AccountScope,
        from: CollectionId,
        name: &str,
        to: CollectionId,
        pre: Precondition,
    ) -> Result<MoveOutcome> {
        let (kind, moved) = {
            let mut st = lock(&self.state);
            st.find(scope, from)?;
            st.find(scope, to)?;
            if st.is_gal(scope.tenant, from) || st.is_gal(scope.tenant, to) {
                return Err(Error::Forbidden);
            }
            let acc = st.accounts.get_mut(&scope).ok_or(Error::NotFound)?;
            let (src, dst) = (acc.coll(from)?, acc.coll(to)?);
            let cur = src.objects.get(name);
            if cur.is_none() && !matches!(pre, Precondition::IfMatch(_)) {
                return Err(Error::NotFound);
            }
            if from == to || src.meta.kind != dst.meta.kind {
                return Err(Error::invalid(
                    "перенос — между разными коллекциями одного вида",
                ));
            }
            if let Some(o) = cur
                && !dst.meta.components.contains(&o.index.component)
            {
                return Err(Error::invalid(format!(
                    "компонент {} не поддерживается коллекцией",
                    o.index.component
                )));
            }
            let Some(o) = cur.filter(|o| match &pre {
                Precondition::IfMatch(etag) => o.etag == *etag,
                Precondition::Any | Precondition::IfNoneMatch => true,
            }) else {
                return Ok(MoveOutcome::PreconditionFailed);
            };
            if dst.objects.contains_key(name) {
                return Ok(MoveOutcome::PreconditionFailed);
            }
            if let Some((other, _)) = dst.objects.iter().find(|(_, d)| d.index.uid == o.index.uid) {
                return Ok(MoveOutcome::UidConflict {
                    name: other.clone(),
                });
            }
            if dst.objects.len() >= self.limits.max_objects {
                return Err(Error::QuotaExceeded);
            }
            let kind = src.meta.kind;
            let (removed, modseq) = (acc.bump(), acc.bump());
            let src = acc.collections.get_mut(&from).ok_or(Error::NotFound)?;
            let mut o = src.objects.remove(name).ok_or(Error::NotFound)?;
            src.meta.state = removed;
            acc.objects.push(Tomb {
                collection: from,
                kind,
                id: o.id,
                name: name.to_owned(),
                modseq: removed,
            });
            o.etag = modseq.to_string();
            o.placed = modseq;
            o.modseq = modseq;
            let moved = Moved {
                id: o.id,
                etag: o.etag.clone(),
                removed,
                modseq,
            };
            let dst = acc.collections.get_mut(&to).ok_or(Error::NotFound)?;
            dst.objects.insert(name.to_owned(), o);
            dst.meta.state = modseq;
            (kind, moved)
        };
        self.signal(scope, kind, from, moved.removed).await;
        self.signal(scope, kind, to, moved.modseq).await;
        Ok(MoveOutcome::Moved(moved))
    }

    async fn find_cards(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        needle: &str,
        limit: usize,
    ) -> Result<Vec<DavObject>> {
        let st = lock(&self.state);
        let (_, c) = st.find(scope, collection)?;
        Ok(c.objects
            .iter()
            .filter(|(_, o)| o.index.search.matches(needle))
            .take(limit)
            .map(|(n, o)| object(collection, n, o))
            .collect())
    }

    async fn changes(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<String>> {
        let st = lock(&self.state);
        let (acc, c) = st.find(scope, collection)?;
        acc.check_since(since)?;
        let mut all: Vec<(ModSeq, Change<String>)> = c
            .objects
            .iter()
            .filter(|(_, o)| o.modseq > since)
            .map(|(n, o)| (o.modseq, live(o.placed, since, n.clone())))
            .collect();
        if since != ModSeq::ZERO {
            // Последнее надгробие имени, которого сейчас нет.
            let mut gone: BTreeMap<&str, ModSeq> = BTreeMap::new();
            for t in acc.objects.iter().filter(|t| {
                t.collection == collection && t.modseq > since && !c.objects.contains_key(&t.name)
            }) {
                let m = gone.entry(&t.name).or_default();
                *m = (*m).max(t.modseq);
            }
            all.extend(
                gone.into_iter()
                    .map(|(n, m)| (m, Change::Destroyed(n.to_owned()))),
            );
        }
        Ok(page(all, since, limit, c.meta.state))
    }

    async fn collection_changes(
        &self,
        scope: AccountScope,
        kind: CollectionKind,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<CollectionId>> {
        let st = lock(&self.state);
        let Some(acc) = st.accounts.get(&scope) else {
            return untouched(since);
        };
        acc.check_since(since)?;
        let mut all: Vec<(ModSeq, Change<CollectionId>)> = acc
            .collections
            .iter()
            .filter(|(_, c)| c.meta.kind == kind && c.props > since)
            .map(|(id, c)| (c.props, live(c.created, since, *id)))
            .collect();
        if since != ModSeq::ZERO {
            all.extend(
                acc.deleted
                    .iter()
                    .filter(|d| d.1 == kind && d.2 > since)
                    .map(|d| (d.2, Change::Destroyed(d.0))),
            );
        }
        Ok(page(all, since, limit, acc.state(kind)))
    }

    async fn account_changes(
        &self,
        scope: AccountScope,
        kind: CollectionKind,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<DavObjectId>> {
        let st = lock(&self.state);
        let Some(acc) = st.accounts.get(&scope) else {
            return untouched(since);
        };
        acc.check_since(since)?;
        let mut all: Vec<(ModSeq, Change<DavObjectId>)> = acc
            .collections
            .values()
            .filter(|c| c.meta.kind == kind)
            .flat_map(|c| c.objects.values())
            .filter(|o| o.modseq > since)
            .map(|o| (o.modseq, live(o.created, since, o.id)))
            .collect();
        if since != ModSeq::ZERO {
            // Надгробие перенесённого объекта — не удаление: объект жив в другой коллекции.
            let alive: std::collections::HashSet<DavObjectId> = acc
                .collections
                .values()
                .flat_map(|c| c.objects.values().map(|o| o.id))
                .collect();
            all.extend(
                acc.objects
                    .iter()
                    .filter(|t| t.kind == kind && t.modseq > since && !alive.contains(&t.id))
                    .map(|t| (t.modseq, Change::Destroyed(t.id))),
            );
        }
        Ok(page(all, since, limit, acc.state(kind)))
    }

    async fn state(&self, scope: AccountScope, kind: CollectionKind) -> Result<ModSeq> {
        Ok(lock(&self.state)
            .accounts
            .get(&scope)
            .map_or(ModSeq::ZERO, |a| a.state(kind)))
    }

    async fn sync_gal(
        &self,
        tenant: TenantId,
        display_name: &str,
        cards: Vec<GalCard>,
    ) -> Result<ModSeq> {
        validate_gal(display_name, &cards, &self.limits)?;
        let (id, state, changed) = {
            let mut st = lock(&self.state);
            let gal = st.gal.entry(tenant).or_default();
            let before = gal.counter;
            // Книга вынимается на время записи: счётчик и надгробия — у `gal`.
            let existing = gal.collections.drain().next();
            let mut book = match existing {
                Some((_, book)) => book,
                None => {
                    let modseq = gal.bump();
                    let meta = Collection {
                        display_name: display_name.to_owned(),
                        state: modseq,
                        ..Collection::new(CollectionKind::AddressBook, GAL_COLLECTION_NAME)
                    };
                    Coll {
                        meta,
                        created: modseq,
                        props: modseq,
                        objects: BTreeMap::new(),
                        acl: BTreeMap::new(),
                    }
                }
            };
            let id = book.meta.id;
            if book.meta.display_name != display_name {
                let modseq = gal.bump();
                book.meta.display_name = display_name.to_owned();
                book.meta.state = modseq;
                book.props = modseq;
            }
            let cards: BTreeMap<String, GalCard> =
                cards.into_iter().map(|c| (c.name.clone(), c)).collect();
            let gone: Vec<String> = book
                .objects
                .keys()
                .filter(|n| !cards.contains_key(n.as_str()))
                .cloned()
                .collect();
            for name in gone {
                if let Some(o) = book.objects.remove(&name) {
                    let modseq = gal.bump();
                    book.meta.state = modseq;
                    gal.objects.push(Tomb {
                        collection: id,
                        kind: CollectionKind::AddressBook,
                        id: o.id,
                        name,
                        modseq,
                    });
                }
            }
            for (name, card) in cards {
                let prev = book.objects.get(&name);
                if prev.is_some_and(|o| o.body == card.body && o.index == card.index) {
                    continue;
                }
                let modseq = gal.bump();
                let (oid, created) =
                    prev.map_or((DavObjectId::new(), modseq), |o| (o.id, o.created));
                book.objects.insert(
                    name,
                    Obj {
                        id: oid,
                        etag: modseq.to_string(),
                        body: card.body,
                        index: card.index,
                        created,
                        placed: created,
                        modseq,
                    },
                );
                book.meta.state = modseq;
            }
            let state = book.meta.state;
            let changed = gal.counter != before;
            gal.collections.insert(id, book);
            (id, state, changed)
        };
        if changed && let Some(bus) = &self.bus {
            // Как у `signal`: ошибка шины не отменяет изменение.
            let _ = bus.publish(gal_changed(tenant, id, state)).await;
        }
        Ok(state)
    }

    async fn gal_changes(
        &self,
        tenant: TenantId,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<DavObjectId>> {
        let st = lock(&self.state);
        let (Some(gal), Some(book)) = (st.gal.get(&tenant), st.gal(tenant)) else {
            return untouched(since);
        };
        gal.check_since(since)?;
        let mut all: Vec<(ModSeq, Change<DavObjectId>)> = book
            .objects
            .values()
            .filter(|o| o.modseq > since)
            .map(|o| (o.modseq, live(o.created, since, o.id)))
            .collect();
        if since != ModSeq::ZERO {
            all.extend(
                gal.objects
                    .iter()
                    .filter(|t| t.modseq > since)
                    .map(|t| (t.modseq, Change::Destroyed(t.id))),
            );
        }
        Ok(page(all, since, limit, book.meta.state))
    }

    async fn inbox_put(
        &self,
        scope: AccountScope,
        name: &str,
        message: InboxMessage,
    ) -> Result<Put> {
        if !valid_name(name) {
            return Err(Error::invalid(format!(
                "недопустимое имя ресурса: {name:?}"
            )));
        }
        message.validate()?;
        if message.body.len() > self.limits.max_object_size {
            return Err(Error::QuotaExceeded);
        }
        let put = {
            let mut st = lock(&self.state);
            let acc = st.accounts.entry(scope).or_default();
            let prev = acc.inbox.get(name).map(|i| (i.id, i.received_at));
            if prev.is_none() && acc.inbox.len() >= self.limits.max_objects {
                return Err(Error::QuotaExceeded);
            }
            let modseq = acc.bump();
            let (id, received_at) =
                prev.unwrap_or_else(|| (DavObjectId::new(), OffsetDateTime::now_utc()));
            let etag = modseq.to_string();
            acc.inbox_created.entry(id).or_insert(modseq);
            acc.inbox.insert(
                name.to_owned(),
                InboxItem {
                    id,
                    name: name.to_owned(),
                    etag: etag.clone(),
                    modseq,
                    received_at,
                    message,
                },
            );
            acc.inbox_state = modseq;
            Put {
                id,
                etag,
                modseq,
                created: prev.is_none(),
            }
        };
        self.inbox_signal(scope, put.modseq).await;
        Ok(put)
    }

    async fn inbox_list(&self, scope: AccountScope) -> Result<Inbox> {
        let st = lock(&self.state);
        let Some(acc) = st.accounts.get(&scope) else {
            return Ok(Inbox::default());
        };
        let mut items: Vec<InboxItem> = acc.inbox.values().cloned().collect();
        items.sort_by_key(|i| i.modseq);
        Ok(Inbox {
            items,
            state: acc.inbox_state,
        })
    }

    async fn inbox_delete(&self, scope: AccountScope, name: &str) -> Result<ModSeq> {
        let modseq = {
            let mut st = lock(&self.state);
            let acc = st.accounts.get_mut(&scope).ok_or(Error::NotFound)?;
            let item = acc.inbox.remove(name).ok_or(Error::NotFound)?;
            let modseq = acc.bump();
            acc.inbox_created.remove(&item.id);
            acc.inbox_gone.push((item.id, modseq));
            acc.inbox_state = modseq;
            modseq
        };
        self.inbox_signal(scope, modseq).await;
        Ok(modseq)
    }

    async fn inbox_state(&self, scope: AccountScope) -> Result<ModSeq> {
        Ok(lock(&self.state)
            .accounts
            .get(&scope)
            .map_or(ModSeq::ZERO, |a| a.inbox_state))
    }

    async fn inbox_changes(
        &self,
        scope: AccountScope,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<DavObjectId>> {
        let st = lock(&self.state);
        let Some(acc) = st.accounts.get(&scope) else {
            return untouched(since);
        };
        acc.check_since(since)?;
        let mut all: Vec<(ModSeq, Change<DavObjectId>)> = acc
            .inbox
            .values()
            .filter(|i| i.modseq > since)
            .map(|i| {
                let created = acc.inbox_created.get(&i.id).copied().unwrap_or(i.modseq);
                (i.modseq, live(created, since, i.id))
            })
            .collect();
        if since != ModSeq::ZERO {
            all.extend(
                acc.inbox_gone
                    .iter()
                    .filter(|(_, m)| *m > since)
                    .map(|(id, m)| (*m, Change::Destroyed(*id))),
            );
        }
        Ok(page(all, since, limit, acc.inbox_state))
    }
}

/// Права на коллекции (CTR-48): записи — у своей коллекции учётной записи (книга GAL — `NotFound`), удаляются вместе
/// с ней, как внешним ключом в PostgreSQL.
#[async_trait]
impl CollectionAcl for MemDav {
    async fn collection_acl(
        &self,
        owner: AccountScope,
        collection: CollectionId,
    ) -> Result<Vec<CollectionAclEntry>> {
        let st = lock(&self.state);
        let c = st
            .accounts
            .get(&owner)
            .and_then(|a| a.collections.get(&collection))
            .ok_or(Error::NotFound)?;
        Ok(acl_entries(&c.acl))
    }

    async fn set_collection_acl(
        &self,
        owner: AccountScope,
        collection: CollectionId,
        identifier: &str,
        rights: CollectionRights,
    ) -> Result<()> {
        validate_identifier(identifier, false)?;
        let mut st = lock(&self.state);
        let c = st
            .accounts
            .get_mut(&owner)
            .and_then(|a| a.collections.get_mut(&collection))
            .ok_or(Error::NotFound)?;
        if rights.is_empty() {
            c.acl.remove(identifier);
        } else {
            c.acl.insert(identifier.to_owned(), rights);
        }
        Ok(())
    }

    async fn collection_rights(
        &self,
        owner: AccountScope,
        collection: CollectionId,
        identifiers: &[String],
    ) -> Result<CollectionRights> {
        for id in identifiers {
            validate_identifier(id, false)?;
        }
        let st = lock(&self.state);
        Ok(st
            .accounts
            .get(&owner)
            .and_then(|a| a.collections.get(&collection))
            .map_or(CollectionRights::NONE, |c| {
                collection_rights(acl_entries(&c.acl).iter(), identifiers)
            }))
    }

    async fn shared_with(
        &self,
        scope: AccountScope,
        identifiers: &[String],
    ) -> Result<Vec<SharedCollection>> {
        for id in identifiers {
            validate_identifier(id, false)?;
        }
        let st = lock(&self.state);
        let mut out: Vec<SharedCollection> = st
            .accounts
            .iter()
            .filter(|(owner, _)| owner.tenant == scope.tenant && owner.account != scope.account)
            .flat_map(|(owner, acc)| {
                acc.collections.iter().map(|(id, c)| SharedCollection {
                    owner: owner.account,
                    collection: *id,
                    rights: collection_rights(acl_entries(&c.acl).iter(), identifiers),
                })
            })
            .filter(|s| !s.rights.is_empty())
            .collect();
        out.sort_by_key(|s| (s.owner, s.collection));
        Ok(out)
    }
}

fn acl_entries(acl: &BTreeMap<String, CollectionRights>) -> Vec<CollectionAclEntry> {
    acl.iter()
        .map(|(identifier, rights)| CollectionAclEntry {
            identifier: identifier.clone(),
            rights: *rights,
        })
        .collect()
}

#[async_trait]
impl FreeBusy for MemDav {
    async fn query(
        &self,
        tenant: TenantId,
        accounts: &[AccountId],
        range: Period,
    ) -> Result<Vec<(AccountId, Vec<BusyPeriod>)>> {
        range.validate()?;
        let st = lock(&self.state);
        let mut out: Vec<(AccountId, Vec<BusyPeriod>)> = Vec::new();
        for &account in accounts {
            if out.iter().any(|(a, _)| *a == account) {
                continue;
            }
            let periods = st
                .accounts
                .get(&AccountScope { tenant, account })
                .map(|acc| {
                    acc.collections
                        .values()
                        .filter(|c| c.meta.kind == CollectionKind::Calendar && !c.meta.transparent)
                        .flat_map(|c| c.objects.values().map(move |o| (c, o)))
                        .flat_map(|(c, o)| {
                            if o.index.recurring {
                                // Повторы разворачиваются при запросе (META-54).
                                crate::busy::expand(
                                    &o.body,
                                    c.meta.timezone.as_deref(),
                                    &range,
                                    self.limits.max_busy_instances,
                                )
                                .unwrap_or_default()
                            } else {
                                o.index.busy.iter().filter_map(|p| p.clip(&range)).collect()
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            out.push((account, merge_busy(periods)));
        }
        Ok(out)
    }
}

/// Изменения учётной записи без изменений: счётчик нулевой, `since` больше нуля — `Conflict`.
fn untouched<T>(since: ModSeq) -> Result<Changes<T>> {
    if since != ModSeq::ZERO {
        return Err(Error::Conflict);
    }
    Ok(page(Vec::new(), since, usize::MAX, ModSeq::ZERO))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_contracts::dav::Component;
    use tp_contracts::{AccountId, TenantId};

    use crate::suite;

    fn scope() -> AccountScope {
        AccountScope {
            tenant: TenantId::new(),
            account: AccountId::new(),
        }
    }

    #[tokio::test]
    async fn suite_dav() {
        let store = MemDav::new().with_limits(suite::dav::LIMITS);
        suite::dav(&store, &suite::dav::DavFixture::new()).await;
    }

    #[tokio::test]
    async fn suite_collection_acl() {
        let store = MemDav::new();
        let tenant = TenantId::new();
        let fx = suite::sharing::SharingFixture {
            owner: AccountScope {
                tenant,
                account: AccountId::new(),
            },
            peer: AccountScope {
                tenant,
                account: AccountId::new(),
            },
            resource: AccountScope {
                tenant,
                account: AccountId::new(),
            },
            foreign: scope(),
        };
        suite::sharing::collection_acl(&store, &store, &fx).await;
    }

    #[tokio::test]
    async fn suite_dav_organize() {
        let store = MemDav::new().with_limits(suite::dav::LIMITS);
        suite::dav::organize(&store, scope()).await;
        suite::dav::organize(&MemDav::new(), scope()).await;
    }

    #[tokio::test]
    async fn suite_dav_signals() {
        let bus = crate::MemEventBus::default();
        let store = MemDav::new().with_bus(Arc::new(bus.clone()));
        suite::dav::signals(&store, &bus, scope()).await;
    }

    #[tokio::test]
    async fn suite_dav_gal() {
        let store = MemDav::new().with_limits(suite::dav::LIMITS);
        suite::dav::gal(&store, &suite::dav::DavFixture::new()).await;
    }

    #[tokio::test]
    async fn suite_dav_gal_signals() {
        let bus = crate::MemEventBus::default();
        let store = MemDav::new().with_bus(Arc::new(bus.clone()));
        suite::dav::gal_signals(&store, &bus, TenantId::new()).await;
    }

    #[tokio::test]
    async fn suite_itip() {
        let store = MemDav::new().with_limits(suite::dav::LIMITS);
        suite::itip(&store, &suite::dav::DavFixture::new()).await;
    }

    #[tokio::test]
    async fn suite_itip_signals() {
        let bus = crate::MemEventBus::default();
        let store = MemDav::new().with_bus(Arc::new(bus.clone()));
        suite::itip::signals(&store, &bus, scope()).await;
    }

    #[tokio::test]
    async fn suite_inbox_log() {
        let store = MemDav::new().with_limits(suite::dav::LIMITS);
        suite::itip::inbox_log(&store, scope()).await;
    }

    #[tokio::test]
    async fn suite_freebusy() {
        let store = MemDav::new().with_limits(suite::dav::LIMITS);
        suite::freebusy(&store, &store, &suite::dav::DavFixture::new()).await;
    }

    #[tokio::test]
    async fn suite_freebusy_recurring() {
        let store = MemDav::new().with_limits(suite::dav::LIMITS);
        suite::freebusy_recurring(&store, &store, &suite::dav::DavFixture::new()).await;
    }

    #[tokio::test]
    async fn stale_since_after_purge() {
        let store = MemDav::new();
        let s = scope();
        let cal = Collection::new(CollectionKind::Calendar, "work");
        let m1 = store
            .create_collection(s, cal.clone())
            .await
            .expect("create");
        let idx = ObjectIndex::new(Component::Event, "u1", None);
        store
            .put_object(s, cal.id, "a.ics", "x".into(), idx, Precondition::Any)
            .await
            .expect("put");
        store
            .delete_object(s, cal.id, "a.ics", Precondition::Any)
            .await
            .expect("delete");
        store.purge_tombstones(s);
        assert_eq!(store.changes(s, cal.id, m1, 10).await, Err(Error::Conflict));
        assert_eq!(
            store
                .account_changes(s, CollectionKind::Calendar, m1, 10)
                .await,
            Err(Error::Conflict)
        );
        // Полная синхронизация и токен после очистки работают.
        let full = store
            .changes(s, cal.id, ModSeq::ZERO, 10)
            .await
            .expect("full");
        assert!(full.created.is_empty() && full.destroyed.is_empty());
        let again = store
            .changes(s, cal.id, full.new_state, 10)
            .await
            .expect("again");
        assert!(again.updated.is_empty());
    }
}
