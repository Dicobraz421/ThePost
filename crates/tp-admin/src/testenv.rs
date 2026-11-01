//! Окружение тестов: Admin API поверх in-memory реализаций tp-testkit; запросы — `tower::ServiceExt::oneshot`.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use serde_json::Value;
use tower::ServiceExt;
use tp_contracts::auth::Principal;
use tp_contracts::ext::AdminRoutes;
use tp_contracts::meta::{AccountScope, Policy, PolicyResolver};
use tp_contracts::{AccountId, DomainId, Error, MailboxId, Result, TenantId};
use tp_testkit::{FakeAuthenticator, MemMetadata, MemQuota, SystemClock};

use crate::accounts::{Domain, DomainStore, PolicyStore, PolicyTarget};
use crate::audit::{AuditRecord, AuditSink};
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

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
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

/// Зависимости поверх in-memory реализаций (без субъектов и модулей).
pub fn deps() -> Deps {
    let meta = MemMetadata::new(Arc::new(SystemClock));
    let policies = MemPolicies::default();
    Deps {
        auth: Arc::new(FakeAuthenticator::default()),
        audit: Arc::new(MemAudit::default()),
        modules: Vec::new(),
        accounts: Arc::new(meta.clone()),
        quota: Arc::new(MemQuota::new(&meta)),
        policies: Arc::new(policies.clone()),
        resolver: Arc::new(policies),
        domains: Arc::new(MemDomains::default()),
    }
}

/// Токены: `root` — администратор инсталляции, `org` — администратор организации по умолчанию,
/// `user` — без роли (организация по умолчанию), `other` — администратор организации по имени входа в другой организации.
pub struct Env {
    pub auth: FakeAuthenticator,
    pub audit: MemAudit,
    pub meta: MemMetadata,
    pub quota: MemQuota,
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
        let audit = MemAudit::default();
        let meta = MemMetadata::new(Arc::new(SystemClock));
        let quota = MemQuota::new(&meta);
        let policies = MemPolicies::default();
        let admin = Admin::new(
            Deps {
                auth: Arc::new(auth.clone()),
                audit: Arc::new(audit.clone()),
                modules,
                accounts: Arc::new(meta.clone()),
                quota: Arc::new(quota.clone()),
                policies: Arc::new(policies.clone()),
                resolver: Arc::new(policies),
                domains: Arc::new(MemDomains::default()),
            },
            cfg,
        )
        .expect("admin");
        Self {
            auth,
            audit,
            meta,
            quota,
            router: Arc::new(admin).router(),
            other_tenant,
        }
    }

    /// Учётная запись организации по умолчанию.
    pub async fn account(&self, login: &str) -> AccountScope {
        self.meta.account(login).await.expect("account")
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
