//! Окружение тестов: сервер JMAP поверх in-memory реализаций tp-testkit и `tp_mailstore::Store`.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Bytes;
use serde_json::{Value, json};
use time::OffsetDateTime;
use tp_contracts::auth::{Authorizer, Principal, Resource, Rights, SendRight};
use tp_contracts::ext::{DecisionHooks, JmapCapability};
use tp_contracts::mailstore::{Delivery, IngestRequest, MailStore};
use tp_contracts::meta::{AccountScope, Flags, SpecialUse};
use tp_contracts::{MailboxId, MessageId};
use tp_contracts::{Result, TenantId};
use tp_testkit::{
    Clock, FakeAuthenticator, FakeSearch, MemBlobIndex, MemBlobStore, MemEventBus, MemMetadata,
    MemPolicies, MemQuota, MemTrash, RecordingDecisionHook, SearchDoc, SystemClock,
};

use crate::{Deps, Jmap, JmapConfig, Outbound};

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

/// Поставленное в очередь письмо.
#[derive(Debug, Clone)]
pub struct Queued {
    pub tenant: TenantId,
    pub mail_from: String,
    pub rcpts: Vec<String>,
    pub raw: Bytes,
}

/// Очередь исходящей почты в памяти.
#[derive(Default)]
pub struct TestOutbound(pub Mutex<Vec<Queued>>);

impl TestOutbound {
    pub fn queued(&self) -> Vec<Queued> {
        self.0.lock().expect("lock").clone()
    }
}

#[async_trait]
impl Outbound for TestOutbound {
    async fn enqueue(
        &self,
        tenant: TenantId,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<()> {
        self.0.lock().expect("lock").push(Queued {
            tenant,
            mail_from: mail_from.to_owned(),
            rcpts: rcpts.to_vec(),
            raw,
        });
        Ok(())
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
    pub outbound: Arc<TestOutbound>,
    pub bus: MemEventBus,
    /// Корзина (`purge`); настройка `trash.purge` — из `policies`.
    pub trash: MemTrash,
    pub policies: MemPolicies,
    /// Точка принятия решений хранилища и сервера (Legal Hold, доставка).
    pub hook: RecordingDecisionHook,
    pub jmap: Arc<Jmap>,
}

impl Env {
    pub async fn new() -> Self {
        Self::with(JmapConfig::default(), Vec::new()).await
    }

    pub async fn with(cfg: JmapConfig, caps: Vec<Arc<dyn JmapCapability>>) -> Self {
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
        let outbound = Arc::new(TestOutbound::default());
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
                blobs: Arc::new(blobs),
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
        .expect("jmap");
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
