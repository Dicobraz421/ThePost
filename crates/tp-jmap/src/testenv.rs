//! Окружение тестов: сервер JMAP поверх in-memory реализаций tp-testkit и `tp_mailstore::Store`.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use std::ops::Range;

use async_trait::async_trait;
use axum::body::Bytes;
use futures::{StreamExt, stream};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tp_contracts::Result;
use tp_contracts::auth::{Authorizer, Principal, Resource, Rights, SendRight};
use tp_contracts::blob::{BlobIndex, BlobStore, ByteStream, Compression, ObjectStream};
use tp_contracts::dav::DavStore;
use tp_contracts::ext::{DecisionHooks, JmapCapability};
use tp_contracts::mailstore::{Delivery, IngestRequest, MailStore};
use tp_contracts::meta::{AccountScope, Flags, SpecialUse};
use tp_contracts::sieve::SieveStore;
use tp_contracts::{BlobHash, BlobKey};
use tp_contracts::{MailboxId, MessageId};
use tp_testkit::{
    Clock, FakeAuthenticator, FakeSearch, MemBlobIndex, MemBlobStore, MemDav, MemEventBus,
    MemMetadata, MemOutbound, MemPolicies, MemQuota, MemSieve, MemTrash, RecordingDecisionHook,
    SearchDoc, SystemClock,
};

use crate::{Deps, Jmap, JmapConfig};

/// Права отправки: свой адрес — Send As, остальные — из таблицы.
#[derive(Default)]
pub struct TestAuthz(pub Mutex<HashMap<String, SendRight>>);

#[async_trait]
impl Authorizer for TestAuthz {
    async fn rights(&self, _: &Principal, _: &Resource) -> Result<Rights> {
        Ok(Rights::ALL)
    }

    async fn send_right(&self, p: &Principal, from: &str) -> Result<Option<SendRight>> {
        if from.eq_ignore_ascii_case(&p.login) {
            return Ok(Some(SendRight::SendAs));
        }
        Ok(self
            .0
            .lock()
            .expect("lock")
            .get(&from.to_lowercase())
            .copied())
    }
}

/// Двойник `tp_blob::Decompressing` (tp-blob — не зависимость полосы): объект с признаком [`Compression::Zstd`]
/// читается как исходное содержимое, если оно отмечено [`Unpacking::known`]; иначе — как есть (сжатые байты,
/// как хранилище без распаковки). Остальное — как у `inner`.
#[derive(Debug, Clone)]
pub struct Unpacking {
    pub inner: MemBlobStore,
    pub index: MemBlobIndex,
    originals: Arc<Mutex<HashMap<BlobHash, Bytes>>>,
}

impl Unpacking {
    /// Отмечает исходное содержимое сжатых объектов с хэшем `data`.
    pub fn known(&self, data: &[u8]) {
        let hash = BlobHash(Sha256::digest(data).into());
        self.originals
            .lock()
            .expect("lock")
            .insert(hash, Bytes::copy_from_slice(data));
    }
}

#[async_trait]
impl BlobStore for Unpacking {
    async fn put(&self, key: &BlobKey, data: Bytes) -> Result<()> {
        self.inner.put(key, data).await
    }

    async fn put_stream(&self, key: &BlobKey, data: ByteStream, hint: Option<u64>) -> Result<()> {
        self.inner.put_stream(key, data, hint).await
    }

    async fn get(&self, key: &BlobKey, range: Option<Range<u64>>) -> Result<ByteStream> {
        if self.index.compression(key).await? == Some(Compression::Zstd) {
            let original = self.originals.lock().expect("lock").get(&key.hash).cloned();
            if let Some(data) = original {
                assert!(range.is_none(), "двойник: диапазон сжатого объекта");
                return Ok(stream::once(async move { Ok(data) }).boxed());
            }
        }
        self.inner.get(key, range).await
    }

    async fn head(&self, key: &BlobKey) -> Result<Option<u64>> {
        self.inner.head(key).await
    }

    async fn delete(&self, key: &BlobKey) -> Result<()> {
        self.inner.delete(key).await
    }

    fn list(&self, prefix: &str, older_than: OffsetDateTime) -> ObjectStream {
        self.inner.list(prefix, older_than)
    }

    async fn put_raw(&self, path: &str, data: Bytes) -> Result<()> {
        self.inner.put_raw(path, data).await
    }

    async fn get_raw(&self, path: &str, range: Option<Range<u64>>) -> Result<ByteStream> {
        self.inner.get_raw(path, range).await
    }

    async fn delete_raw(&self, path: &str) -> Result<()> {
        self.inner.delete_raw(path).await
    }
}

pub const USING: &[&str] = &["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"];

pub struct Env {
    pub meta: MemMetadata,
    pub search: FakeSearch,
    pub auth: FakeAuthenticator,
    pub store: tp_mailstore::Store,
    pub scope: AccountScope,
    pub principal: Principal,
    pub authz: Arc<TestAuthz>,
    pub outbound: Arc<MemOutbound>,
    pub bus: MemEventBus,
    /// Корзина (`purge`); настройка `trash.purge` — из `policies`.
    pub trash: MemTrash,
    pub policies: MemPolicies,
    /// Точка принятия решений хранилища и сервера (Legal Hold, доставка).
    pub hook: RecordingDecisionHook,
    /// Объекты BLOB сервера JMAP (`Deps::blobs`) и их учёт.
    pub blobs: Unpacking,
    /// Скрипты Sieve и автоответ (`Jmap::with_sieve`), сигналы — в `bus`.
    pub sieve: MemSieve,
    /// Календари и контакты (`Jmap::with_dav`), сигналы — в `bus`.
    pub dav: MemDav,
    pub jmap: Arc<Jmap>,
}

impl Env {
    pub async fn new() -> Self {
        Self::with(JmapConfig::default(), Vec::new()).await
    }

    pub async fn with(cfg: JmapConfig, caps: Vec<Arc<dyn JmapCapability>>) -> Self {
        Self::with_signer(cfg, caps, None).await
    }

    /// Окружение без хранилищ Sieve и DAV (capability `vacationresponse`, `sieve`, `contacts` не объявлены).
    pub async fn bare() -> Self {
        Self::build(JmapConfig::default(), Vec::new(), None, false).await
    }

    /// Окружение с подписью профиля `.mobileconfig`.
    pub async fn with_signer(
        cfg: JmapConfig,
        caps: Vec<Arc<dyn JmapCapability>>,
        signer: Option<Arc<dyn crate::ProfileSigner>>,
    ) -> Self {
        Self::build(cfg, caps, signer, true).await
    }

    async fn build(
        cfg: JmapConfig,
        caps: Vec<Arc<dyn JmapCapability>>,
        signer: Option<Arc<dyn crate::ProfileSigner>>,
        stores: bool,
    ) -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let blobs = MemBlobStore::new(clock.clone());
        let index = MemBlobIndex::new(clock.clone());
        let meta = MemMetadata::new(clock).with_blob_index(index.clone());
        let bus = MemEventBus::default();
        let policies = MemPolicies::new(&meta);
        let trash = MemTrash::new(&meta).with_policies(policies.clone());
        let hook = RecordingDecisionHook::new("test");
        let hooks = DecisionHooks::new(vec![Arc::new(hook.clone())]);
        let store = tp_mailstore::Store::new(tp_mailstore::Deps {
            blobs: Arc::new(blobs.clone()),
            index: Arc::new(index.clone()),
            messages: Arc::new(meta.clone()),
            changes: Arc::new(meta.clone()),
            trash: Arc::new(trash.clone()),
            quota: Arc::new(MemQuota::new(&meta)),
            bus: Arc::new(bus.clone()),
            hooks: hooks.clone(),
        });
        let scope = meta.account("user@example.org").await.expect("account");
        let principal = Principal {
            tenant: scope.tenant,
            account: scope.account,
            login: "user@example.org".into(),
            groups: BTreeSet::new(),
            scopes: None,
        };
        let auth = FakeAuthenticator::default();
        auth.add_password("user@example.org", "app-secret", principal.clone());
        auth.add_token("token-1", principal.clone());
        let search = FakeSearch::default();
        let authz = Arc::new(TestAuthz::default());
        let outbound = Arc::new(MemOutbound::default());
        let unpacking = Unpacking {
            inner: blobs,
            index: index.clone(),
            originals: Arc::default(),
        };
        let sieve = MemSieve::new().with_bus(Arc::new(bus.clone()));
        let dav = MemDav::new().with_bus(Arc::new(bus.clone()));
        let jmap = Jmap::new(
            Deps {
                auth: Arc::new(auth.clone()),
                mailboxes: Arc::new(meta.clone()),
                messages: Arc::new(meta.clone()),
                changes: Arc::new(meta.clone()),
                store: Arc::new(store.clone()),
                trash: Arc::new(trash.clone()),
                policies: Arc::new(policies.clone()),
                search: Arc::new(search.clone()),
                blobs: Arc::new(unpacking.clone()),
                blob_index: Arc::new(index),
                bus: Arc::new(bus.clone()),
                accounts: Arc::new(meta.clone()),
                authz: authz.clone(),
                outbound: Some(outbound.clone()),
                hooks,
                capabilities: caps,
            },
            cfg,
        )
        .expect("jmap")
        .with_profile_signer(signer)
        .with_sieve(stores.then(|| Arc::new(sieve.clone()) as Arc<dyn SieveStore>))
        .with_dav(stores.then(|| Arc::new(dav.clone()) as Arc<dyn DavStore>))
        .with_freebusy(
            stores.then(|| Arc::new(dav.clone()) as Arc<dyn tp_contracts::schedule::FreeBusy>),
        );
        Self {
            meta,
            search,
            auth,
            store,
            scope,
            principal,
            authz,
            outbound,
            bus,
            trash,
            policies,
            hook,
            blobs: unpacking,
            sieve,
            dav,
            jmap: Arc::new(jmap),
        }
    }

    pub fn account(&self) -> String {
        self.scope.account.to_string()
    }

    pub fn mailbox(&self, role: SpecialUse) -> MailboxId {
        self.meta.mailbox_by_role(self.scope, role).expect("role")
    }

    /// Ответ API на запрос JSON.
    pub async fn request(&self, req: Value) -> Value {
        match self
            .jmap
            .api(&self.principal, req.to_string().as_bytes())
            .await
        {
            Ok(r) => serde_json::to_value(r).expect("serialize"),
            Err(p) => p.to_json(),
        }
    }

    /// Вызовы методов с `USING`: ответы методов.
    pub async fn calls(&self, calls: Value) -> Vec<Value> {
        let resp = self
            .request(json!({ "using": USING, "methodCalls": calls }))
            .await;
        resp["methodResponses"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| panic!("no methodResponses: {resp}"))
    }

    /// Один вызов: аргументы ответа (имя ответа должно совпасть с `expect`).
    pub async fn call(&self, method: &str, args: Value, expect: &str) -> Value {
        let r = self.calls(json!([[method, args, "c0"]])).await;
        assert_eq!(r[0][0], expect, "{method}: {}", r[0]);
        r[0][1].clone()
    }

    /// Доставляет письмо в папку и индексирует его в поиске.
    pub async fn deliver(&self, mailbox: MailboxId, raw: &[u8], flags: Flags) -> MessageId {
        self.deliver_at(mailbox, raw, flags, OffsetDateTime::now_utc())
            .await
    }

    pub async fn deliver_at(
        &self,
        mailbox: MailboxId,
        raw: &[u8],
        flags: Flags,
        at: OffsetDateTime,
    ) -> MessageId {
        let chunks: Vec<Result<Bytes>> = vec![Ok(Bytes::copy_from_slice(raw))];
        let got = self
            .store
            .ingest(IngestRequest {
                tenant: self.scope.tenant,
                mail_from: "sender@example.com".into(),
                raw: Box::pin(futures::stream::iter(chunks)),
                deliveries: vec![Delivery {
                    account: self.scope.account,
                    mailbox,
                    extra_headers: vec![],
                    flags,
                }],
                received_at: at,
            })
            .await
            .expect("ingest");
        let id = got.delivered[0].message;
        if let Ok(doc) = SearchDoc::parse(id, mailbox, at, raw) {
            self.search.index(self.scope, doc);
        }
        id
    }
}

/// Простое письмо.
pub fn mail(subject: &str, from: &str, extra: &str, body: &str) -> Vec<u8> {
    format!(
        "From: {from}\r\nTo: user@example.org\r\nSubject: {subject}\r\n\
         Date: Fri, 30 Oct 2026 10:00:00 +0300\r\n{extra}\r\n{body}\r\n"
    )
    .into_bytes()
}

/// Запускает HTTP-сервер JMAP на случайном порту.
pub async fn serve(jmap: Arc<Jmap>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let router = jmap.router();
    tokio::spawn(async move { axum::serve(listener, router).await });
    addr
}

/// Запрос HTTP/1.0 (ответ без chunked, до закрытия соединения): код, заголовки, тело.
pub async fn http10(addr: std::net::SocketAddr, req: &[u8]) -> (u16, String, Vec<u8>) {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
    s.write_all(req).await.expect("write");
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.expect("read");
    let split = out
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map_or(out.len(), |p| p + 4);
    let head = String::from_utf8_lossy(&out[..split]).into_owned();
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, head, out[split..].to_vec())
}
