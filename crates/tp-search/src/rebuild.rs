//! Перестройка индекса ящика (SRS 4.6, CONTRACTS.md#Search): индекс — производные данные.
//!
//! [`Rebuilder::rebuild`] строит новый индекс рядом с действующим ([`IndexManager::shadow_path`]) полным
//! обходом писем учётной записи ([`DocumentSource::messages`]), пока запросы обслуживает прежний.
//! Изменения индексатора на время перестройки применяются к обоим индексам. Затем под
//! [`IndexManager::lock`] новый индекс выгружается новым поколением и манифест атомарно переключается
//! (CAS, запрос перестройки снимается), после чего каталог перестройки становится каталогом индекса.
//! Запрос перестройки, пришедший во время неё, — `Error::Conflict`: перестройка отменяется и
//! запускается заново. Ящики к перестройке — [`IndexManifestStore::pending_rebuilds`] (роль indexer,
//! для ящиков, которые она индексирует).
//!
//! [`IndexManifestStore::pending_rebuilds`]: tp_contracts::search::IndexManifestStore::pending_rebuilds

use std::sync::Arc;

use schemars::JsonSchema;
use serde::Deserialize;
use tp_contracts::meta::AccountScope;
use tp_contracts::search::IndexManifest;
use tp_contracts::{Error, Result};

use crate::index::{IndexManager, IndexOp, MailIndex, blocking};
use crate::indexer::DocumentSource;
use crate::persist::IndexStore;

/// Перестройка индексов (секция `search.rebuild`; поля необязательны, неизвестное — ошибка).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RebuildConfig {
    /// Писем в одном коммите перестраиваемого индекса (не меньше одного).
    pub batch_docs: usize,
}

impl Default for RebuildConfig {
    fn default() -> Self {
        Self { batch_docs: 500 }
    }
}

/// Перестройка индексов учётных записей на этом узле.
pub struct Rebuilder {
    indexes: Arc<IndexManager>,
    source: Arc<dyn DocumentSource>,
    store: Option<Arc<IndexStore>>,
    cfg: RebuildConfig,
}

impl std::fmt::Debug for Rebuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rebuilder")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

impl Rebuilder {
    /// Перестройка индексов `indexes` по содержимому писем из `source`.
    pub fn new(
        indexes: Arc<IndexManager>,
        source: Arc<dyn DocumentSource>,
        cfg: RebuildConfig,
    ) -> Self {
        Self {
            indexes,
            source,
            store: None,
            cfg,
        }
    }

    /// Перестроенный индекс сохраняется в BlobStore и переключается в манифесте (SRCH-05).
    pub fn with_store(mut self, store: Arc<IndexStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Перестраивает индекс учётной записи и переключает на него запросы; возвращает число писем.
    pub async fn rebuild(&self, scope: AccountScope) -> Result<u64> {
        let start = match &self.store {
            Some(store) => store.manifest(scope).await?,
            None => IndexManifest::default(),
        };
        let m = self.indexes.clone();
        let shadow = blocking(move || m.begin_shadow(scope)).await?;
        let res = self.build_and_switch(scope, &shadow, &start).await;
        drop(shadow);
        if res.is_err() {
            let m = self.indexes.clone();
            if let Err(e) = blocking(move || m.drop_shadow(scope)).await {
                tracing::warn!(tenant = %scope.tenant, account = %scope.account, error = %e,
                    "search index rebuild cleanup failed");
            }
        }
        res
    }

    async fn build_and_switch(
        &self,
        scope: AccountScope,
        shadow: &Arc<MailIndex>,
        start: &IndexManifest,
    ) -> Result<u64> {
        let messages = self.source.messages(scope).await?;
        for chunk in messages.chunks(self.cfg.batch_docs.max(1)) {
            let mut ops = Vec::with_capacity(chunk.len());
            for message in chunk {
                if let Some(doc) = self.source.document(scope, *message).await? {
                    ops.push(IndexOp::Upsert(Box::new(doc)));
                }
            }
            let s = shadow.clone();
            blocking(move || s.apply(&ops)).await?;
        }

        let _gate = self.indexes.lock(scope).await;
        if let Some(store) = &self.store {
            let current = store.manifest(scope).await?;
            if current.rebuild != start.rebuild {
                return Err(Error::Conflict);
            }
            store
                .publish_rebuilt(scope, shadow, &current, start.rebuild)
                .await?;
        }
        let docs = shadow.docs();
        let m = self.indexes.clone();
        blocking(move || m.promote_shadow(scope)).await?;
        tracing::info!(tenant = %scope.tenant, account = %scope.account, docs,
            "search index rebuilt");
        Ok(docs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexDoc;
    use crate::index::tests::scope;
    use crate::indexer::{Indexer, IndexerConfig};
    use crate::persist::tests::{Node, apply, count, mail, node, store, upserts};
    use async_trait::async_trait;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use time::OffsetDateTime;
    use tokio::sync::{Notify, RwLock};
    use tp_contracts::MessageId;
    use tp_contracts::events::{CoreEvent, CoreEventKind, EventLog, EventSink, Partition};
    use tp_contracts::search::{IndexManifestStore, RebuildReason, RebuildRequest};
    use tp_testkit::{MemBlobStore, MemEventLog, MemIndexManifests};

    /// Письма в памяти.
    #[derive(Default)]
    struct Docs(Mutex<BTreeMap<MessageId, IndexDoc>>);

    impl Docs {
        fn put(&self, doc: &IndexDoc) {
            self.0
                .lock()
                .expect("lock")
                .insert(doc.message, doc.clone());
        }
    }

    #[async_trait]
    impl DocumentSource for Docs {
        async fn document(&self, _: AccountScope, message: MessageId) -> Result<Option<IndexDoc>> {
            Ok(self.0.lock().expect("lock").get(&message).cloned())
        }

        async fn messages(&self, _: AccountScope) -> Result<Vec<MessageId>> {
            Ok(self.0.lock().expect("lock").keys().copied().collect())
        }
    }

    /// Те же письма, но обход перестройки стоит, пока тест держит `pause`.
    struct Paused {
        docs: Arc<Docs>,
        pause: RwLock<()>,
        reached: Notify,
    }

    #[async_trait]
    impl DocumentSource for Paused {
        async fn document(
            &self,
            scope: AccountScope,
            message: MessageId,
        ) -> Result<Option<IndexDoc>> {
            self.reached.notify_one();
            let _wait = self.pause.read().await;
            self.docs.document(scope, message).await
        }

        async fn messages(&self, scope: AccountScope) -> Result<Vec<MessageId>> {
            self.docs.messages(scope).await
        }
    }

    async fn deliver(indexer: &Indexer, scope: AccountScope, doc: &IndexDoc) {
        let log = MemEventLog::new(1);
        log.append(CoreEvent {
            tenant: scope.tenant,
            account: scope.account,
            mailbox: Some(doc.mailbox),
            kind: CoreEventKind::Delivered {
                message: doc.message,
            },
            at: OffsetDateTime::now_utc(),
        });
        let after = log.position("t", Partition(0)).await.expect("position");
        let batch = log.read(Partition(0), after, 10).await.expect("read");
        indexer.handle(&batch).await.expect("handle");
    }

    fn request(at: i64) -> RebuildRequest {
        RebuildRequest {
            requested_at: OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(at),
            reason: RebuildReason::Admin,
        }
    }

    struct Setup {
        node: Node,
        blob: Arc<MemBlobStore>,
        manifests: Arc<MemIndexManifests>,
        store: Arc<IndexStore>,
        docs: Arc<Docs>,
        paused: Arc<Paused>,
        indexer: Indexer,
        rebuilder: Arc<Rebuilder>,
        scope: AccountScope,
    }

    /// Индекс из 30 писем «Отчёт» в BlobStore и запрос перестройки.
    async fn setup() -> Setup {
        let (blob, manifests, store) = store();
        let node = node();
        let scope = scope();
        let docs = Arc::new(Docs::default());
        let reports: Vec<_> = (0..30).map(|i| mail(&format!("Отчёт {i}"))).collect();
        for d in &reports {
            docs.put(d);
        }
        let index = node.indexes.open_or_create(scope).expect("create");
        apply(&index, upserts(&reports)).await;
        store.publish(scope, &index).await.expect("publish");
        manifests
            .request_rebuild(scope, request(1))
            .await
            .expect("request");
        let paused = Arc::new(Paused {
            docs: docs.clone(),
            pause: RwLock::new(()),
            reached: Notify::new(),
        });
        let indexer = Indexer::new(node.indexes.clone(), docs.clone(), IndexerConfig::default())
            .with_store(store.clone());
        let rebuilder = Arc::new(
            Rebuilder::new(
                node.indexes.clone(),
                paused.clone(),
                RebuildConfig { batch_docs: 10 },
            )
            .with_store(store.clone()),
        );
        Setup {
            node,
            blob,
            manifests,
            store,
            docs,
            paused,
            indexer,
            rebuilder,
            scope,
        }
    }

    fn generation(m: &IndexManifest) -> Option<String> {
        m.segments
            .first()
            .and_then(|s| s.path.rsplit_once('/'))
            .map(|(g, _)| g.to_owned())
    }

    #[tokio::test]
    async fn search_works_during_rebuild() {
        let s = setup().await;
        let scope = s.scope;
        let before = s.store.manifest(scope).await.expect("manifest");
        let pause = s.paused.pause.write().await;
        let task = {
            let r = s.rebuilder.clone();
            tokio::spawn(async move { r.rebuild(scope).await })
        };
        s.paused.reached.notified().await;
        assert!(s.node.indexes.shadow_path(scope).exists());

        // Перестройка идёт: запросы — по прежнему индексу, новые письма видны сразу.
        assert_eq!(count(&s.node.search, scope, "отчет").await, 30);
        let invoice = mail("Счёт на оплату");
        s.docs.put(&invoice);
        deliver(&s.indexer, scope, &invoice).await;
        assert_eq!(count(&s.node.search, scope, "счет").await, 1);
        assert_eq!(count(&s.node.search, scope, "отчет").await, 30);

        drop(pause);
        assert_eq!(task.await.expect("join").expect("rebuild"), 31);

        // Переключено: новое поколение без запроса; письмо, пришедшее во время перестройки, на месте.
        let after = s.store.manifest(scope).await.expect("manifest");
        assert_eq!((after.rebuild, after.docs()), (None, 31));
        assert!(after.version > before.version);
        assert_ne!(generation(&after), generation(&before));
        assert_eq!(count(&s.node.search, scope, "отчет").await, 30);
        assert_eq!(count(&s.node.search, scope, "счет").await, 1);
        assert!(!s.node.indexes.shadow_path(scope).exists());
        assert_eq!(s.node.indexes.cached(scope).expect("open").synced(), after);
        // Прежнее поколение удалено из BlobStore.
        let mut stored = s.blob.paths();
        stored.retain(|p| p.starts_with(&crate::persist::prefix(scope)));
        assert_eq!(stored.len(), after.segments.len());

        // Индекс обновляется и после переключения.
        let late = mail("Счёт повторный");
        s.docs.put(&late);
        deliver(&s.indexer, scope, &late).await;
        assert_eq!(count(&s.node.search, scope, "счет").await, 2);
        assert_eq!(s.store.manifest(scope).await.expect("m").docs(), 32);
    }

    #[tokio::test]
    async fn new_request_during_rebuild_restarts_it() {
        let s = setup().await;
        let scope = s.scope;
        let pause = s.paused.pause.write().await;
        let task = {
            let r = s.rebuilder.clone();
            tokio::spawn(async move { r.rebuild(scope).await })
        };
        s.paused.reached.notified().await;
        s.manifests
            .request_rebuild(scope, request(2))
            .await
            .expect("request again");
        drop(pause);
        assert!(matches!(task.await.expect("join"), Err(Error::Conflict)));
        assert!(!s.node.indexes.shadow_path(scope).exists());
        assert_eq!(count(&s.node.search, scope, "отчет").await, 30);
        assert_eq!(
            s.store.manifest(scope).await.expect("m").rebuild,
            Some(request(2))
        );
        // Повтор выполняет перестройку по новому запросу.
        assert_eq!(s.rebuilder.rebuild(scope).await.expect("again"), 30);
        assert_eq!(s.store.manifest(scope).await.expect("m").rebuild, None);
    }

    #[tokio::test]
    async fn local_rebuild_drops_stale_documents() {
        let n = node();
        let scope = scope();
        let docs = Arc::new(Docs::default());
        let kept = mail("Отчёт живой");
        docs.put(&kept);
        let index = n.indexes.open_or_create(scope).expect("create");
        apply(&index, upserts(&[kept, mail("Отчёт удалённый")])).await;
        assert_eq!(count(&n.search, scope, "отчет").await, 2);
        let r = Rebuilder::new(n.indexes.clone(), docs, RebuildConfig::default());
        assert_eq!(r.rebuild(scope).await.expect("rebuild"), 1);
        assert_eq!(count(&n.search, scope, "отчет").await, 1);
    }

    #[test]
    fn config_defaults_and_unknown() {
        assert_eq!(
            crate::tests::from_pairs::<RebuildConfig>(&[]).expect("defaults"),
            RebuildConfig::default()
        );
        assert!(crate::tests::from_pairs::<RebuildConfig>(&[("batch", "1")]).is_err());
    }
}
