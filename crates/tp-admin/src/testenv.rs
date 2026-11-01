//! Окружение тестов: Admin API поверх in-memory реализаций tp-testkit; запросы — `tower::ServiceExt::oneshot`.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use bytes::Bytes;
use serde_json::Value;
use time::OffsetDateTime;
use tower::ServiceExt;
use tp_contracts::auth::Principal;
use tp_contracts::ext::AdminRoutes;
use tp_contracts::ext::DecisionHooks;
use tp_contracts::groups::{DistributionGroup, HeldMessage};
use tp_contracts::mailstore::{Delivery, IngestRequest, MailStore};
use tp_contracts::meta::{AccountScope, Flags, Policy, PolicyResolver, SpecialUse};
use tp_contracts::{
    AccountId, DomainId, Error, GroupId, HeldId, MailboxId, MessageId, Result, TenantId,
};
use tp_testkit::{
    Clock, FakeAuthenticator, MemAppPasswords, MemBlobIndex, MemBlobStore, MemDeliveryLog,
    MemEventBus, MemGroups, MemMetadata, MemQuota, MemTrash, SystemClock,
};

use crate::accounts::{Domain, DomainStore, PolicyStore, PolicyTarget};
use crate::audit::{AuditEntry, AuditLog, AuditPage, AuditQuery, AuditRecord, AuditSink};
use crate::groups::Moderation;
use crate::server::{Admin, AdminConfig, AdminRole, Deps, RoleBinding};

/// Журнал аудита в памяти.
#[derive(Debug, Clone, Default)]
pub struct MemAudit(Arc<Mutex<Vec<AuditRecord>>>);

impl MemAudit {
    pub fn records(&self) -> Vec<AuditRecord> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl AuditSink for MemAudit {
    async fn record(&self, record: &AuditRecord) -> Result<()> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(record.clone());
        Ok(())
    }
}

#[async_trait]
impl AuditLog for MemAudit {
    async fn search(&self, tenant: TenantId, q: &AuditQuery) -> Result<AuditPage> {
        let entries: Vec<AuditEntry> = lock(&self.0)
            .iter()
            .enumerate()
            .map(|(i, r)| AuditEntry {
                seq: i as u64 + 1,
                record: r.clone(),
            })
            .filter(|e| {
                e.record.tenant == Some(tenant)
                    && q.after.is_none_or(|a| e.seq > a)
                    && q.since.is_none_or(|t| e.record.at >= t)
                    && q.until.is_none_or(|t| e.record.at < t)
            })
            .take(q.limit)
            .collect();
        let next = (entries.len() == q.limit)
            .then(|| entries.last().map(|e| e.seq))
            .flatten();
        Ok(AuditPage { entries, next })
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Решение модерации, переданное [`MemModeration`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Deliver(HeldId, GroupId),
    Reject(HeldId, String),
}

/// Решения модерации в памяти; `fail_next` — ошибка следующего решения.
#[derive(Debug, Clone, Default)]
pub struct MemModeration(Arc<Mutex<(Vec<Decision>, Option<Error>)>>);

impl MemModeration {
    pub fn decisions(&self) -> Vec<Decision> {
        lock(&self.0).0.clone()
    }

    pub fn fail_next(&self, e: Error) {
        lock(&self.0).1 = Some(e);
    }

    fn push(&self, d: Decision) -> Result<()> {
        let mut st = lock(&self.0);
        if let Some(e) = st.1.take() {
            return Err(e);
        }
        st.0.push(d);
        Ok(())
    }
}

#[async_trait]
impl Moderation for MemModeration {
    async fn deliver(&self, _: TenantId, group: &DistributionGroup, m: &HeldMessage) -> Result<()> {
        self.push(Decision::Deliver(m.id, group.id))
    }

    async fn reject(&self, _: TenantId, m: &HeldMessage, reason: &str) -> Result<()> {
        self.push(Decision::Reject(m.id, reason.to_owned()))
    }
}

/// Политики уровней в памяти; разрешение — наложение организация → домен → учётная запись.
#[derive(Debug, Clone, Default)]
pub struct MemPolicies(Arc<Mutex<HashMap<(TenantId, PolicyTarget), Policy>>>);

#[async_trait]
impl PolicyStore for MemPolicies {
    async fn policy(&self, tenant: TenantId, target: PolicyTarget) -> Result<Policy> {
        Ok(lock(&self.0)
            .get(&(tenant, target))
            .cloned()
            .unwrap_or_default())
    }

    async fn set_policy(
        &self,
        tenant: TenantId,
        target: PolicyTarget,
        policy: &Policy,
    ) -> Result<()> {
        let mut m = lock(&self.0);
        if *policy == Policy::default() {
            m.remove(&(tenant, target));
        } else {
            m.insert((tenant, target), policy.clone());
        }
        Ok(())
    }
}

#[async_trait]
impl PolicyResolver for MemPolicies {
    async fn resolve(
        &self,
        tenant: TenantId,
        domain: Option<DomainId>,
        account: Option<AccountId>,
        _mailbox: Option<MailboxId>,
    ) -> Result<Policy> {
        let levels = [
            Some(PolicyTarget::Tenant),
            domain.map(PolicyTarget::Domain),
            account.map(PolicyTarget::Account),
        ];
        let mut p = Policy::default();
        for t in levels.into_iter().flatten() {
            p = p.overlay(self.policy(tenant, t).await?);
        }
        Ok(p)
    }
}

/// Домены в памяти (без проверки учётных записей домена при удалении).
#[derive(Debug, Clone, Default)]
pub struct MemDomains(Arc<Mutex<Vec<(TenantId, Domain)>>>);

#[async_trait]
impl DomainStore for MemDomains {
    async fn list(&self, tenant: TenantId) -> Result<Vec<Domain>> {
        let mut v: Vec<Domain> = lock(&self.0)
            .iter()
            .filter(|(t, _)| *t == tenant)
            .map(|(_, d)| d.clone())
            .collect();
        v.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(v)
    }

    async fn create(&self, tenant: TenantId, name: &str) -> Result<Domain> {
        let mut m = lock(&self.0);
        if m.iter().any(|(_, d)| d.name == name) {
            return Err(Error::Conflict);
        }
        let d = Domain {
            id: DomainId::new(),
            name: name.to_owned(),
        };
        m.push((tenant, d.clone()));
        Ok(d)
    }

    async fn delete(&self, tenant: TenantId, id: DomainId) -> Result<()> {
        let mut m = lock(&self.0);
        let before = m.len();
        m.retain(|(t, d)| !(*t == tenant && d.id == id));
        if m.len() == before {
            return Err(Error::NotFound);
        }
        Ok(())
    }
}

/// Хранилища в памяти: метаданные, квоты, почтовое хранилище.
pub struct Stores {
    pub meta: MemMetadata,
    pub quota: MemQuota,
    pub store: tp_mailstore::Store,
    pub passwords: MemAppPasswords,
    pub groups: MemGroups,
    pub delivery: MemDeliveryLog,
    pub moderation: MemModeration,
}

impl Stores {
    pub fn new() -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let index = MemBlobIndex::new(clock.clone());
        let meta = MemMetadata::new(clock.clone()).with_blob_index(index.clone());
        let quota = MemQuota::new(&meta);
        let store = tp_mailstore::Store::new(tp_mailstore::Deps {
            blobs: Arc::new(MemBlobStore::new(clock)),
            index: Arc::new(index),
            messages: Arc::new(meta.clone()),
            changes: Arc::new(meta.clone()),
            trash: Arc::new(MemTrash::new(&meta)),
            quota: Arc::new(quota.clone()),
            bus: Arc::new(MemEventBus::default()),
            hooks: DecisionHooks::default(),
        });
        let passwords = MemAppPasswords::new(&meta);
        Self {
            meta,
            quota,
            store,
            passwords,
            groups: MemGroups::new(),
            delivery: MemDeliveryLog::new(),
            moderation: MemModeration::default(),
        }
    }

    /// Пароли приложений поверх хранилища паролей.
    pub fn app_passwords(&self) -> tp_auth::AppPasswords {
        tp_auth::AppPasswords::new(
            Arc::new(self.passwords.clone()),
            tp_auth::PasswordConfig::default(),
        )
        .expect("app passwords")
    }

    /// Зависимости Admin API поверх хранилищ (без субъектов и модулей).
    pub fn deps(&self) -> Deps {
        let policies = MemPolicies::default();
        Deps {
            auth: Arc::new(FakeAuthenticator::default()),
            audit: Arc::new(MemAudit::default()),
            audit_log: None,
            delivery: Arc::new(self.delivery.clone()),
            groups: Arc::new(self.groups.clone()),
            moderation: Arc::new(self.moderation.clone()),
            modules: Vec::new(),
            accounts: Arc::new(self.meta.clone()),
            quota: Arc::new(self.quota.clone()),
            policies: Arc::new(policies.clone()),
            resolver: Arc::new(policies),
            domains: Arc::new(MemDomains::default()),
            trash: Arc::new(MemTrash::new(&self.meta)),
            store: Arc::new(self.store.clone()),
            app_passwords: self.app_passwords(),
        }
    }
}

/// Зависимости поверх новых хранилищ в памяти.
pub fn deps() -> Deps {
    Stores::new().deps()
}

/// Токены: `root` — администратор инсталляции, `org` — администратор организации по умолчанию,
/// `user` — без роли (организация по умолчанию), `other` — администратор организации по имени входа в другой организации,
/// `operator` и `auditor` — оператор и аудитор организации по умолчанию.
pub struct Env {
    pub auth: FakeAuthenticator,
    pub audit: MemAudit,
    pub groups: MemGroups,
    pub delivery: MemDeliveryLog,
    pub moderation: MemModeration,
    pub meta: MemMetadata,
    pub quota: MemQuota,
    pub store: tp_mailstore::Store,
    pub app_passwords: tp_auth::AppPasswords,
    pub router: Router,
    pub other_tenant: TenantId,
}

/// Субъект с группами.
pub fn subject(tenant: TenantId, login: &str, groups: &[&str]) -> Principal {
    Principal {
        tenant,
        account: AccountId::new(),
        login: login.into(),
        groups: groups
            .iter()
            .map(|g| (*g).to_owned())
            .collect::<BTreeSet<_>>(),
        scopes: None,
    }
}

/// Назначения ролей тестов.
pub fn config() -> AdminConfig {
    AdminConfig {
        roles: vec![
            RoleBinding {
                role: AdminRole::InstallationAdmin,
                logins: vec![],
                groups: vec!["mail-root".into()],
            },
            RoleBinding {
                role: AdminRole::OrganizationAdmin,
                logins: vec!["Admin@Example.org".into()],
                groups: vec![],
            },
            RoleBinding {
                role: AdminRole::Operator,
                logins: vec![],
                groups: vec!["mail-ops".into()],
            },
            RoleBinding {
                role: AdminRole::Auditor,
                logins: vec![],
                groups: vec!["security".into()],
            },
        ],
        ..AdminConfig::default()
    }
}

impl Env {
    pub fn new() -> Self {
        Self::with(config(), Vec::new())
    }

    pub fn with(cfg: AdminConfig, modules: Vec<Arc<dyn AdminRoutes>>) -> Self {
        let auth = FakeAuthenticator::default();
        let other_tenant = TenantId::new();
        let d = TenantId::DEFAULT;
        auth.add_token("root", subject(d, "root@example.org", &["mail-root"]));
        auth.add_token("org", subject(d, "admin@example.org", &["staff"]));
        auth.add_token("user", subject(d, "user@example.org", &["staff"]));
        auth.add_token("other", subject(other_tenant, "admin@example.org", &[]));
        auth.add_token("operator", subject(d, "ops@example.org", &["mail-ops"]));
        auth.add_token("auditor", subject(d, "sec@example.org", &["security"]));
        let audit = MemAudit::default();
        let stores = Stores::new();
        let admin = Admin::new(
            Deps {
                auth: Arc::new(auth.clone()),
                audit: Arc::new(audit.clone()),
                audit_log: Some(Arc::new(audit.clone())),
                modules,
                ..stores.deps()
            },
            cfg,
        )
        .expect("admin");
        let app_passwords = stores.app_passwords();
        let Stores {
            meta,
            quota,
            store,
            groups,
            delivery,
            moderation,
            ..
        } = stores;
        Self {
            auth,
            audit,
            groups,
            delivery,
            moderation,
            meta,
            quota,
            store,
            app_passwords,
            router: Arc::new(admin).router(),
            other_tenant,
        }
    }

    /// Учётная запись организации по умолчанию.
    pub async fn account(&self, login: &str) -> AccountScope {
        self.meta.account(login).await.expect("account")
    }

    /// Доставляет простое письмо во «Входящие».
    pub async fn deliver(&self, scope: AccountScope, subject: &str) -> MessageId {
        let inbox = self
            .meta
            .mailbox_by_role(scope, SpecialUse::Inbox)
            .expect("inbox");
        let raw = format!(
            "From: a@example.com
To: user@example.org
Subject: {subject}

body
"
        );
        let chunks: Vec<Result<Bytes>> = vec![Ok(Bytes::from(raw))];
        let got = self
            .store
            .ingest(IngestRequest {
                tenant: scope.tenant,
                mail_from: "a@example.com".into(),
                raw: Box::pin(futures::stream::iter(chunks)),
                deliveries: vec![Delivery {
                    account: scope.account,
                    mailbox: inbox,
                    extra_headers: vec![],
                    flags: Flags::default(),
                }],
                received_at: OffsetDateTime::now_utc(),
            })
            .await
            .expect("ingest");
        got.delivered[0].message
    }

    /// Запрос с Bearer-токеном `token` и телом JSON: код, заголовки, тело (JSON; не JSON — строка; пусто — `Null`).
    pub async fn call(
        &self,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value) {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            req = req.header(header::AUTHORIZATION, format!("Bearer {t}"));
        }
        let req = match body {
            Some(b) => req
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(b.to_string())),
            None => req.body(Body::empty()),
        }
        .expect("request");
        let resp = self.router.clone().oneshot(req).await.expect("response");
        let (parts, body) = resp.into_parts();
        let bytes = to_bytes(body, usize::MAX).await.expect("body");
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (parts.status, parts.headers, value)
    }
}
