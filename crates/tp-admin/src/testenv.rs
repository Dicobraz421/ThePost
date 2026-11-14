//! Окружение тестов: Admin API поверх in-memory реализаций tp-testkit; запросы — `tower::ServiceExt::oneshot`.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode, header};
use bytes::Bytes;
use serde_json::Value;
use time::OffsetDateTime;
use tower::ServiceExt;
use tp_contracts::admin_audit::AdminAuditRecord;
use tp_contracts::auth::Principal;
use tp_contracts::ext::AdminRoutes;
use tp_contracts::ext::DecisionHooks;
use tp_contracts::mailstore::{Delivery, IngestRequest, MailStore};
use tp_contracts::meta::{
    Account, AccountKind, AccountScope, AccountState, AccountStore, AccountUpsert, AddressTarget,
    DisabledAccount, ExistingContent, Flags, QuotaUsage, SpecialUse,
};
use tp_contracts::{AccountId, DomainId, MessageId, Result, TenantId};
use tp_testkit::{
    Clock, FakeAuthenticator, MemAppPasswords, MemAuditLog, MemBlobIndex, MemBlobStore,
    MemDeliveryLog, MemEventBus, MemGroups, MemLocalAdminTokens, MemMetadata, MemPolicies,
    MemQuota, MemSieve, MemTransport, MemTrash, SystemClock,
};
use tp_testkit::{MemAddressBook, MemDav, MemMailboxAudit, MemSenderLists, MemSyncGate};

use crate::server::{Admin, AdminConfig, AdminRole, Deps, RoleBinding};
use crate::sharing::SharingStores;
use crate::sieve::ScriptError;

/// Издатель токенов OIDC тестов (`Deps::oidc_issuer`).
pub const ISSUER: &str = "https://idp.example.org/realms/mail";

/// Журнал аудита в памяти — [`MemAuditLog`] tp-testkit; [`MemAudit::records`] — записи для проверок.
#[derive(Debug, Clone, Default)]
pub struct MemAudit(pub MemAuditLog);

impl MemAudit {
    pub fn records(&self) -> Vec<AdminAuditRecord> {
        self.0.entries().into_iter().map(|e| e.record).collect()
    }
}

/// Решения модерации — [`MemModeration`] tp-testkit (CTR-31).
pub use tp_testkit::{MemModeration, ModerationDecision as Decision};

/// Вызовы `AccountStore` Admin API (имена методов по порядку) — проверка числа запросов к хранилищу.
#[derive(Debug, Clone, Default)]
pub struct AccountCalls(Arc<Mutex<Vec<&'static str>>>);

impl AccountCalls {
    /// Вызовы с прошлого `take` (и очищает список).
    pub fn take(&self) -> Vec<&'static str> {
        std::mem::take(&mut *self.0.lock().expect("calls"))
    }

    fn push(&self, method: &'static str) {
        self.0.lock().expect("calls").push(method);
    }
}

/// [`MemMetadata`] как `AccountStore` Admin API со счётом вызовов ([`AccountCalls`]).
struct CountedAccounts {
    inner: MemMetadata,
    calls: AccountCalls,
}

#[async_trait]
impl AccountStore for CountedAccounts {
    async fn get(&self, tenant: TenantId, id: AccountId) -> Result<Option<Account>> {
        self.calls.push("get");
        self.inner.get(tenant, id).await
    }

    async fn by_ids(&self, tenant: TenantId, ids: &[AccountId]) -> Result<Vec<Account>> {
        self.calls.push("by_ids");
        self.inner.by_ids(tenant, ids).await
    }

    async fn by_login(&self, tenant: TenantId, login: &str) -> Result<Option<Account>> {
        self.calls.push("by_login");
        self.inner.by_login(tenant, login).await
    }

    async fn by_address(&self, tenant: TenantId, address: &str) -> Result<Option<AddressTarget>> {
        self.calls.push("by_address");
        self.inner.by_address(tenant, address).await
    }

    async fn by_external_id(&self, tenant: TenantId, external_id: &str) -> Result<Option<Account>> {
        self.calls.push("by_external_id");
        self.inner.by_external_id(tenant, external_id).await
    }

    async fn list(
        &self,
        tenant: TenantId,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Account>> {
        self.calls.push("list");
        self.inner.list(tenant, prefix, after, limit).await
    }

    async fn upsert(&self, tenant: TenantId, data: AccountUpsert) -> Result<Account> {
        self.calls.push("upsert");
        self.inner.upsert(tenant, data).await
    }

    async fn quota(&self, tenant: TenantId, id: AccountId) -> Result<QuotaUsage> {
        self.calls.push("quota");
        self.inner.quota(tenant, id).await
    }

    async fn account_state(&self, tenant: TenantId, id: AccountId) -> Result<AccountState> {
        self.calls.push("account_state");
        self.inner.account_state(tenant, id).await
    }

    async fn disable_account(
        &self,
        tenant: TenantId,
        id: AccountId,
        until: OffsetDateTime,
    ) -> Result<()> {
        self.calls.push("disable_account");
        self.inner.disable_account(tenant, id, until).await
    }

    async fn restore_account(&self, tenant: TenantId, id: AccountId) -> Result<()> {
        self.calls.push("restore_account");
        self.inner.restore_account(tenant, id).await
    }

    async fn disabled_accounts(&self, tenant: TenantId) -> Result<Vec<DisabledAccount>> {
        self.calls.push("disabled_accounts");
        self.inner.disabled_accounts(tenant).await
    }

    async fn expired_accounts(
        &self,
        now: OffsetDateTime,
        limit: usize,
    ) -> Result<Vec<AccountScope>> {
        self.calls.push("expired_accounts");
        self.inner.expired_accounts(now, limit).await
    }

    async fn begin_removal(
        &self,
        tenant: TenantId,
        id: AccountId,
        now: OffsetDateTime,
    ) -> Result<()> {
        self.calls.push("begin_removal");
        self.inner.begin_removal(tenant, id, now).await
    }

    async fn attach_account(
        &self,
        tenant: TenantId,
        disabled: AccountId,
        to: AccountId,
        existing: Option<ExistingContent>,
    ) -> Result<Account> {
        self.calls.push("attach_account");
        self.inner
            .attach_account(tenant, disabled, to, existing)
            .await
    }

    async fn remove_disabled(&self, tenant: TenantId, id: AccountId) -> Result<()> {
        self.calls.push("remove_disabled");
        self.inner.remove_disabled(tenant, id).await
    }

    async fn grant_installation_admin(&self, tenant: TenantId, id: AccountId) -> Result<()> {
        self.calls.push("grant_installation_admin");
        self.inner.grant_installation_admin(tenant, id).await
    }

    async fn is_installation_admin(&self, tenant: TenantId, id: AccountId) -> Result<bool> {
        self.calls.push("is_installation_admin");
        self.inner.is_installation_admin(tenant, id).await
    }
}

/// Хранилища в памяти: метаданные, квоты, почтовое хранилище.
pub struct Stores {
    pub meta: MemMetadata,
    /// Вызовы `AccountStore` Admin API.
    pub account_calls: AccountCalls,
    pub quota: MemQuota,
    pub store: tp_mailstore::Store,
    pub passwords: MemAppPasswords,
    pub groups: MemGroups,
    pub delivery: MemDeliveryLog,
    pub moderation: MemModeration,
    pub policies: MemPolicies,
    pub trash: MemTrash,
    /// Адресная книга: контакты, оргструктура, динамические группы.
    pub addressbook: MemAddressBook,
    pub senders: MemSenderLists,
    pub mailbox_audit: MemMailboxAudit,
    pub sync_gate: MemSyncGate,
    /// Календари и адресные книги, права на коллекции.
    pub dav: MemDav,
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
            account_calls: AccountCalls::default(),
            quota,
            store,
            passwords,
            groups: MemGroups::new(),
            delivery: MemDeliveryLog::new(),
            moderation: MemModeration::default(),
            policies,
            trash,
            addressbook: MemAddressBook::new(),
            senders: MemSenderLists::new(),
            mailbox_audit: MemMailboxAudit::new(),
            sync_gate: MemSyncGate::new(),
            dav: MemDav::new(),
        }
    }

    /// Хранилища общего доступа, представителей и ресурсов (`Admin::with_sharing`).
    pub fn sharing(&self) -> SharingStores {
        SharingStores {
            acl: Arc::new(self.dav.clone()),
            delegates: Arc::new(self.meta.clone()),
            resources: Arc::new(self.meta.clone()),
            policy: Arc::new(self.meta.clone()),
            collections: Arc::new(self.dav.clone()),
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
            audit_log: Arc::new(MemAuditLog::new()),
            delivery: Arc::new(self.delivery.clone()),
            groups: Arc::new(self.groups.clone()),
            moderation: Arc::new(self.moderation.clone()),
            modules: Vec::new(),
            accounts: Arc::new(CountedAccounts {
                inner: self.meta.clone(),
                calls: self.account_calls.clone(),
            }),
            quota: Arc::new(self.quota.clone()),
            policies: Arc::new(self.policies.clone()),
            resolver: Arc::new(self.policies.clone()),
            trash: Arc::new(self.trash.clone()),
            store: Arc::new(self.store.clone()),
            app_passwords: self.app_passwords(),
            contacts: Arc::new(self.addressbook.clone()),
            org: Arc::new(self.addressbook.clone()),
            dynamic: Arc::new(self.addressbook.clone()),
            senders: Arc::new(self.senders.clone()),
            mailbox_audit: Arc::new(self.mailbox_audit.clone()),
            sync: Arc::new(self.sync_gate.clone()),
            oidc_issuer: Some(ISSUER.to_owned()),
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
    /// Вызовы `AccountStore` Admin API.
    pub account_calls: AccountCalls,
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
    /// Адресная книга: контакты, оргструктура, динамические группы.
    pub addressbook: MemAddressBook,
    pub mailbox_audit: MemMailboxAudit,
    pub sync_gate: MemSyncGate,
    /// Календари и адресные книги, права на коллекции (`Admin::with_sharing`).
    pub dav: MemDav,
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
        Self::build(config(), Vec::new(), false, true, true)
    }

    /// Без локального входа (`admin init`, вход по паролю и обмен — 503).
    pub fn without_local() -> Self {
        Self::build(config(), Vec::new(), true, false, true)
    }

    /// Без общего доступа, представителей и ресурсов (их маршруты — 503).
    pub fn without_sharing() -> Self {
        Self::build(config(), Vec::new(), true, true, false)
    }

    pub fn with(cfg: AdminConfig, modules: Vec<Arc<dyn AdminRoutes>>) -> Self {
        Self::build(cfg, modules, true, true, true)
    }

    fn build(
        cfg: AdminConfig,
        modules: Vec<Arc<dyn AdminRoutes>>,
        with_transport: bool,
        with_local: bool,
        with_sharing: bool,
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
                audit_log: Arc::new(audit.0.clone()),
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
        let admin = if with_sharing {
            admin.with_sharing(stores.sharing())
        } else {
            admin
        };
        let sieve = MemSieve::new();
        let held_blobs = MemBlobStore::new(Arc::new(SystemClock));
        let mut admin = admin
            .with_sieve(Arc::new(sieve.clone()), check_script)
            .with_held_blobs(Arc::new(held_blobs.clone()));
        // Вторая организация инсталляции — для проверок области ролей.
        admin.organizations.insert(other_tenant);
        let admin = Arc::new(admin);
        let app_passwords = stores.app_passwords();
        let Stores {
            meta,
            account_calls,
            quota,
            store,
            groups,
            delivery,
            moderation,
            policies,
            trash,
            addressbook,
            mailbox_audit,
            sync_gate,
            dav,
            ..
        } = stores;
        Self {
            auth,
            audit,
            groups,
            delivery,
            moderation,
            meta,
            account_calls,
            quota,
            store,
            policies,
            trash,
            app_passwords,
            transport,
            tokens,
            sieve,
            held_blobs,
            addressbook,
            mailbox_audit,
            sync_gate,
            dav,
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
                    aliases: Vec::new(),
                    kind: AccountKind::User,
                    hidden_from_gal: false,
                    resource_type: None,
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
