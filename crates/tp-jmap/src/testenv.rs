//! Окружение тестов: сервер JMAP поверх in-memory реализаций tp-testkit и `tp_mailstore::Store`.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::body::Bytes;
use serde_json::{Value, json};
use time::OffsetDateTime;
use tp_contracts::Result;
use tp_contracts::auth::Principal;
use tp_contracts::ext::{DecisionHooks, JmapCapability};
use tp_contracts::mailstore::{Delivery, IngestRequest, MailStore};
use tp_contracts::meta::{AccountScope, Flags, SpecialUse};
use tp_contracts::{MailboxId, MessageId};
use tp_testkit::{
    Clock, FakeAuthenticator, FakeSearch, MemBlobIndex, MemBlobStore, MemEventBus, MemMetadata,
    MemQuota, MemTrash, SearchDoc, SystemClock,
};

use crate::{Deps, Jmap, JmapConfig};

pub const USING: &[&str] = &["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"];

pub struct Env {
    pub meta: MemMetadata,
    pub search: FakeSearch,
    pub auth: FakeAuthenticator,
    pub store: tp_mailstore::Store,
    pub scope: AccountScope,
    pub principal: Principal,
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
        let store = tp_mailstore::Store::new(tp_mailstore::Deps {
            blobs: Arc::new(blobs),
            index: Arc::new(index),
            messages: Arc::new(meta.clone()),
            changes: Arc::new(meta.clone()),
            trash: Arc::new(MemTrash::new(&meta)),
            quota: Arc::new(MemQuota::new(&meta)),
            bus: Arc::new(MemEventBus::default()),
            hooks: DecisionHooks::default(),
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
        let jmap = Jmap::new(
            Deps {
                auth: Arc::new(auth.clone()),
                mailboxes: Arc::new(meta.clone()),
                messages: Arc::new(meta.clone()),
                changes: Arc::new(meta.clone()),
                store: Arc::new(store.clone()),
                search: Arc::new(search.clone()),
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
