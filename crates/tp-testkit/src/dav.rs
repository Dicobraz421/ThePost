//! [`DavStore`] и [`FreeBusy`] в памяти (CONTRACTS.md#Dav, CONTRACTS.md#Schedule; CTR-45, CTR-47).
//!
//! `ModSeq` — свой счётчик учётной записи (у `PgMeta` — общий с письмами), ETag — `ModSeq` записи. С шиной
//! ([`MemDav::with_bus`]) после каждого изменения публикуется `DavChanged`, после изменения входящих планирования —
//! `ScheduleInboxChanged`. [`MemDav::purge_tombstones`] имитирует очистку надгробий: прежние `since` после неё —
//! `Conflict`.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tp_contracts::bus::EventBus;
use tp_contracts::dav::{
    Collection, CollectionId, CollectionKind, DavLimits, DavObject, DavObjectId, DavStore,
    DeleteOutcome, ObjectIndex, ObjectRef, Precondition, Put, PutOutcome, TimeRange, dav_changed,
    valid_name,
};
use tp_contracts::meta::{AccountScope, Changes};
use tp_contracts::schedule::{
    BusyPeriod, FreeBusy, Inbox, InboxItem, InboxMessage, Period, merge_busy,
    schedule_inbox_changed,
};
use tp_contracts::{AccountId, Error, ModSeq, Result, TenantId};

use crate::infra::lock;

#[derive(Debug, Clone)]
struct Obj {
    id: DavObjectId,
    etag: String,
    body: String,
    index: ObjectIndex,
    created: ModSeq,
    modseq: ModSeq,
}

#[derive(Debug)]
struct Coll {
    meta: Collection,
    created: ModSeq,
    props: ModSeq,
    objects: BTreeMap<String, Obj>,
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
}

impl Account {
    fn bump(&mut self) -> ModSeq {
        self.counter = self.counter.next();
        self.counter
    }

    fn coll(&self, id: CollectionId) -> Result<&Coll> {
        self.collections.get(&id).ok_or(Error::NotFound)
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
    state: Arc<Mutex<HashMap<AccountScope, Account>>>,
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
        let acc = st.entry(scope).or_default();
        acc.horizon = acc.counter;
        acc.objects.clear();
        acc.deleted.clear();
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
            .get(&scope)
            .map(|a| {
                a.collections
                    .values()
                    .filter(|c| c.meta.kind == kind)
                    .map(|c| c.meta.clone())
                    .collect()
            })
            .unwrap_or_default();
        out.sort_by(|a, b| a.order.cmp(&b.order).then_with(|| a.name.cmp(&b.name)));
        Ok(out)
    }

    async fn get_collection(
        &self,
        scope: AccountScope,
        id: CollectionId,
    ) -> Result<Option<Collection>> {
        Ok(lock(&self.state)
            .get(&scope)
            .and_then(|a| a.collections.get(&id))
            .map(|c| c.meta.clone()))
    }

    async fn collection_by_name(
        &self,
        scope: AccountScope,
        kind: CollectionKind,
        name: &str,
    ) -> Result<Option<Collection>> {
        Ok(lock(&self.state).get(&scope).and_then(|a| {
            a.collections
                .values()
                .find(|c| c.meta.kind == kind && c.meta.name == name)
                .map(|c| c.meta.clone())
        }))
    }

    async fn create_collection(
        &self,
        scope: AccountScope,
        mut collection: Collection,
    ) -> Result<ModSeq> {
        collection.validate()?;
        let (kind, id) = (collection.kind, collection.id);
        let modseq = {
            let mut st = lock(&self.state);
            let acc = st.entry(scope).or_default();
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
            let modseq = acc.bump();
            collection.state = modseq;
            acc.collections.insert(
                id,
                Coll {
                    meta: collection,
                    created: modseq,
                    props: modseq,
                    objects: BTreeMap::new(),
                },
            );
            modseq
        };
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
        let modseq = {
            let mut st = lock(&self.state);
            let acc = st.get_mut(&scope).ok_or(Error::NotFound)?;
            let cur = acc.coll(id)?;
            if cur.meta.kind != kind || cur.meta.name != collection.name {
                return Err(Error::invalid("вид и имя коллекции не меняются"));
            }
            let modseq = acc.bump();
            let c = acc.collections.get_mut(&id).ok_or(Error::NotFound)?;
            c.meta = Collection {
                state: modseq,
                ..collection
            };
            c.props = modseq;
            modseq
        };
        self.signal(scope, kind, id, modseq).await;
        Ok(modseq)
    }

    async fn delete_collection(&self, scope: AccountScope, id: CollectionId) -> Result<ModSeq> {
        let (kind, modseq) = {
            let mut st = lock(&self.state);
            let acc = st.get_mut(&scope).ok_or(Error::NotFound)?;
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
        let c = st.get(&scope).ok_or(Error::NotFound)?.coll(collection)?;
        Ok(c.objects.get(name).map(|o| object(collection, name, o)))
    }

    async fn object_by_uid(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        uid: &str,
    ) -> Result<Option<DavObject>> {
        let st = lock(&self.state);
        let c = st.get(&scope).ok_or(Error::NotFound)?.coll(collection)?;
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
        let c = st.get(&scope).ok_or(Error::NotFound)?.coll(collection)?;
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
        let c = st.get(&scope).ok_or(Error::NotFound)?.coll(collection)?;
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
        let Some(acc) = st.get(&scope) else {
            return Ok(Vec::new());
        };
        Ok(ids
            .iter()
            .filter_map(|id| {
                acc.collections.iter().find_map(|(cid, c)| {
                    c.objects
                        .iter()
                        .find(|(_, o)| o.id == *id)
                        .map(|(n, o)| object(*cid, n, o))
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
        let c = st.get(&scope).ok_or(Error::NotFound)?.coll(collection)?;
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
            let acc = st.get_mut(&scope).ok_or(Error::NotFound)?;
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
            let prev = cur.map(|o| (o.id, o.created));
            let modseq = acc.bump();
            let (id, created) = prev.unwrap_or((DavObjectId::new(), modseq));
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
            let acc = st.get_mut(&scope).ok_or(Error::NotFound)?;
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

    async fn changes(
        &self,
        scope: AccountScope,
        collection: CollectionId,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<String>> {
        let st = lock(&self.state);
        let acc = st.get(&scope).ok_or(Error::NotFound)?;
        let c = acc.coll(collection)?;
        acc.check_since(since)?;
        let mut all: Vec<(ModSeq, Change<String>)> = c
            .objects
            .iter()
            .filter(|(_, o)| o.modseq > since)
            .map(|(n, o)| (o.modseq, live(o.created, since, n.clone())))
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
        let Some(acc) = st.get(&scope) else {
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
        let Some(acc) = st.get(&scope) else {
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
            all.extend(
                acc.objects
                    .iter()
                    .filter(|t| t.kind == kind && t.modseq > since)
                    .map(|t| (t.modseq, Change::Destroyed(t.id))),
            );
        }
        Ok(page(all, since, limit, acc.state(kind)))
    }

    async fn state(&self, scope: AccountScope, kind: CollectionKind) -> Result<ModSeq> {
        Ok(lock(&self.state)
            .get(&scope)
            .map_or(ModSeq::ZERO, |a| a.state(kind)))
    }

    async fn inbox_put(&self, scope: AccountScope, name: &str, message: InboxMessage) -> Result<Put> {
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
            let acc = st.entry(scope).or_default();
            let prev = acc.inbox.get(name).map(|i| i.id);
            if prev.is_none() && acc.inbox.len() >= self.limits.max_objects {
                return Err(Error::QuotaExceeded);
            }
            let modseq = acc.bump();
            let id = prev.unwrap_or_else(DavObjectId::new);
            let etag = modseq.to_string();
            acc.inbox.insert(
                name.to_owned(),
                InboxItem {
                    id,
                    name: name.to_owned(),
                    etag: etag.clone(),
                    modseq,
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
        let Some(acc) = st.get(&scope) else {
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
            let acc = st.get_mut(&scope).ok_or(Error::NotFound)?;
            if acc.inbox.remove(name).is_none() {
                return Err(Error::NotFound);
            }
            let modseq = acc.bump();
            acc.inbox_state = modseq;
            modseq
        };
        self.inbox_signal(scope, modseq).await;
        Ok(modseq)
    }
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
                .get(&AccountScope { tenant, account })
                .map(|acc| {
                    acc.collections
                        .values()
                        .filter(|c| c.meta.kind == CollectionKind::Calendar && !c.meta.transparent)
                        .flat_map(|c| c.objects.values())
                        .flat_map(|o| o.index.busy.iter())
                        .filter_map(|p| p.clip(&range))
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
    async fn suite_dav_signals() {
        let bus = crate::MemEventBus::default();
        let store = MemDav::new().with_bus(Arc::new(bus.clone()));
        suite::dav::signals(&store, &bus, scope()).await;
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
    async fn suite_freebusy() {
        let store = MemDav::new().with_limits(suite::dav::LIMITS);
        suite::freebusy(&store, &store, &suite::dav::DavFixture::new()).await;
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
