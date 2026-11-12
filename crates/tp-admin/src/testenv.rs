//! Окружение тестов: Admin API поверх in-memory реализаций tp-testkit; запросы — `tower::ServiceExt::oneshot`.

use std::collections::BTreeSet;
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
use tp_contracts::mailstore::{Delivery, IngestRequest, MailStore};
use tp_contracts::meta::{AccountScope, AccountStore, AccountUpsert, Flags, Policy, SpecialUse};
use tp_contracts::{AccountId, DomainId, Error, MessageId, Result, TenantId};
use tp_testkit::{
    Clock, FakeAuthenticator, MemAppPasswords, MemBlobIndex, MemBlobStore, MemDeliveryLog,
    MemEventBus, MemGroups, MemLocalAdminTokens, MemMetadata, MemPolicies, MemQuota, MemSieve,
    MemTransport, MemTrash, PolicyLevel, SystemClock,
};

use crate::accounts::{Domain, DomainStore, PolicyStore, PolicyTarget};
use crate::audit::{AuditEntry, AuditLog, AuditPage, AuditQuery, AuditRecord, AuditSink};
use crate::server::{Admin, AdminConfig, AdminRole, Deps, RoleBinding};
use crate::sieve::ScriptError;

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

/// Решения модерации — [`MemModeration`] tp-testkit (CTR-31).
pub use tp_testkit::{MemModeration, ModerationDecision as Decision};

/// Политики уровней — [`MemPolicies`] tp-testkit (разрешение цепочки и выбор пользователя `trash.purge`).
#[async_trait]
impl PolicyStore for MemPolicies {
    async fn policy(&self, tenant: TenantId, target: PolicyTarget) -> Result<Policy> {
        Ok(MemPolicies::policy(self, tenant, level(target)))
    }

    async fn set_policy(
        &self,
        tenant: TenantId,
        target: PolicyTarget,
        policy: &Policy,
    ) -> Result<()> {
        MemPolicies::set_policy(self, tenant, level(target), policy.clone());
        Ok(())
    }
}

fn level(target: PolicyTarget) -> PolicyLevel {
    match target {
        PolicyTarget::Tenant => PolicyLevel::Tenant,
        PolicyTarget::Domain(d) => PolicyLevel::Domain(d),
        PolicyTarget::Account(a) => PolicyLevel::Account(a),
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
    pub policies: MemPolicies,
    pub trash: MemTrash,
}

impl Stores {
    pub fn new() -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let index = MemBlobIndex::new(clock.clone());
        let meta = MemMetadata::new(clock.clone()).with_blob_index(index.clone());
        let quota = MemQuota::new(&meta);
        let policies = MemPolicies::new(&meta);
        let trash = MemTrash::new(&meta).with_policies(policies.clone());
        let store = tp_mailstore::Store::new(tp_mailstore::Deps {
            blobs: Arc::new(MemBlobStore::new(clock)),
            index: Arc::new(index),
            messages: Arc::new(meta.clone()),
            changes: Arc::new(meta.clone()),
            trash: Arc::new(trash.clone()),
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
            policies,
            trash,
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
            policies: Arc::new(self.policies.clone()),
            resolver: Arc::new(self.policies.clone()),
            domains: Arc::new(MemDomains::default()),
            trash: Arc::new(self.trash.clone()),
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
    pub policies: MemPolicies,
    pub trash: MemTrash,
    pub app_passwords: tp_auth::AppPasswords,
    pub transport: MemTransport,
    pub tokens: MemLocalAdminTokens,
    pub sieve: MemSieve,
    /// Объекты писем очереди модерации (`Admin::with_held_blobs`).
    pub held_blobs: MemBlobStore,
    pub admin: Arc<Admin>,
    pub router: Router,
    pub other_tenant: TenantId,
}

/// Проверка скрипта тестов (вместо `tp_sieve::check`): каждое `oops` — ошибка с позицией.
pub fn check_script(text: &str) -> Vec<ScriptError> {
    let mut errors = Vec::new();
    for (i, line) in text.lines().enumerate() {
        for (at, _) in line.match_indices("oops") {
            errors.push(ScriptError {
                line: i as u32 + 1,
                column: line[..at].chars().count() as u32 + 1,
                message: "неизвестная команда".into(),
            });
        }
    }
    errors
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

    /// Без настроек транспорта (маршруты транспорта — 503).
    pub fn without_transport() -> Self {
        Self::build(config(), Vec::new(), false, true)
    }

    /// Без локального входа (`admin init` и обмен — 503).
    pub fn without_local() -> Self {
        Self::build(config(), Vec::new(), true, false)
    }

    pub fn with(cfg: AdminConfig, modules: Vec<Arc<dyn AdminRoutes>>) -> Self {
        Self::build(cfg, modules, true, true)
    }

    fn build(
        cfg: AdminConfig,
        modules: Vec<Arc<dyn AdminRoutes>>,
        with_transport: bool,
        with_local: bool,
    ) -> Self {
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
        let transport = MemTransport::new();
        let admin = if with_transport {
            admin.with_transport(Arc::new(transport.clone()))
        } else {
            admin
        };
        let tokens = MemLocalAdminTokens::new();
        let admin = if with_local {
            admin.with_local_tokens(Arc::new(tokens.clone()))
        } else {
            admin
        };
        let sieve = MemSieve::new();
        let held_blobs = MemBlobStore::new(Arc::new(SystemClock));
        let admin = Arc::new(
            admin
                .with_sieve(Arc::new(sieve.clone()), check_script)
                .with_held_blobs(Arc::new(held_blobs.clone())),
        );
        let app_passwords = stores.app_passwords();
        let Stores {
            meta,
            quota,
            store,
            groups,
            delivery,
            moderation,
            policies,
            trash,
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
            policies,
            trash,
            app_passwords,
            transport,
            tokens,
            sieve,
            held_blobs,
            router: admin.clone().router(),
            admin,
            other_tenant,
        }
    }

    /// Учётная запись организации по умолчанию.
    pub async fn account(&self, login: &str) -> AccountScope {
        self.account_in(TenantId::DEFAULT, login).await
    }

    /// Учётная запись организации `tenant` (новый домен, без каталога).
    pub async fn account_in(&self, tenant: TenantId, login: &str) -> AccountScope {
        let acc = self
            .meta
            .upsert(
                tenant,
                AccountUpsert {
                    domain: DomainId::new(),
                    login: login.to_owned(),
                    display_name: login.to_owned(),
                    external_id: None,
                    enabled: true,
                },
            )
            .await
            .expect("account");
        AccountScope {
            tenant: acc.tenant,
            account: acc.id,
        }
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
