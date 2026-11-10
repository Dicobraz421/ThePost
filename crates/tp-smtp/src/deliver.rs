//! Локальная доставка (SRS 4.1, CONTRACTS.md#MailStore): разрешение получателей (учётные записи, псевдонимы и
//! группы каталога), заголовки `Return-Path` и `Received`, [`MailStore::ingest`] одним вызовом на письмо.
//!
//! [`Delivery`] — обработчик сессий ([`Handler`]) обеих ролей:
//! - получатель в локальном домене — учётные записи, на которые он разрешается; неизвестный — `550 5.1.1`;
//!   переполненный ящик — `452 4.2.2` или `552 5.2.2` (`delivery.quota_reject`) до `DATA`;
//! - группа рассылки ([`Delivery::with_groups`], [`crate::groups`]): отправитель вне круга — `550 5.7.1` на `RCPT`,
//!   письмо модерируемой группы — в очередь модерации, иначе группа раскрывается;
//! - внешний получатель: smtp-in — `550 5.7.1` (не открытый релей), submission — в очередь исходящей почты
//!   ([`Outbound`], [`crate::queue::Queue`]); узел без очереди (`Deps::outbound` = `None`) — `451 4.3.2`;
//! - submission: до доставки и очереди — `DecisionHooks::on_submission` (`Deny` — `550 5.7.1`; дополнения:
//!   заголовки в начало письма, копии в папки и внешним адресам);
//! - журнал доставки ([`Delivery::with_tracking`], [`crate::tracking`]): приём, отказы, доставка, модерация,
//!   уведомления;
//! - адреса пересылки модулей (`Ingested::forward`, решение 2026-10-31-ingest-envelope; `CopyTo::Address`):
//!   локальные — сразу в ящики тем же `ingest` (SMTP-28), внешние — в очередь [`Outbound`].
//!
//! Отправка вне сессии SMTP (JMAP `EmailSubmission/set`) — [`Delivery::submit`] и `impl Outbound for Delivery`
//! (CMP-04, контракт `tp_contracts::queue::Outbound`, CTR-28): локальные получатели доставляются сразу тем же
//! `ingest`, внешние — в очередь (без очереди — `SubmitStatus::Failed` с `451 4.3.0`, SMTP-28); статус по получателю.
//!
//! Адресация ([`crate::address`]): получатели переписываются до разрешения, отправитель — после проверок;
//! подадрес `user+tag` доставляется в ящик `user` с `Delivered-To: user+tag@…`, неизвестный адрес домена — на
//! адрес для всей почты домена (псевдоним каталога `@domain`), если он задан.
//!
//! Каталог (псевдонимы, адреса учётных записей, группы) читается полным снимком и кэшируется на
//! `delivery.directory_refresh`; учётная запись — [`AccountStore::by_login`]. Проверки SPF, DKIM, DMARC и ARC
//! входящей почты — [`crate::authcheck`]: их заголовки добавляются к письму, письмо с проваленным DMARC
//! (`quarantine`, `reject`) доставляется в «Спам». Затем письмо проверяют фильтры Milter ([`crate::milter`]):
//! отказ фильтра — ответ на `DATA`, карантин — «Спам» (smtp-in) или отказ (submission).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use time::OffsetDateTime;
use tp_contracts::auth::{DirChange, DirCursor, Directory};
use tp_contracts::ext::{CopyTo, Decision, DecisionHooks, SubmissionInfo};
use tp_contracts::groups::DistributionGroup;
use tp_contracts::mailstore::{Delivery as MailDelivery, IngestRequest, Ingested, MailStore};
use tp_contracts::meta::{Account, AccountScope, AccountStore, MailboxStore, SpecialUse};
use tp_contracts::queue::{DsnRet, Outbound, SubmitStatus, Submitted};
use tp_contracts::retention::Quota;
use tp_contracts::{AccountId, Error, MailboxId, Result, TenantId};

use crate::address::{Addressing, Side};
use crate::authcheck::AuthCheck;
use crate::config::{DeliveryConfig, QuotaReject, SmtpConfig};
use crate::dsn::{self, DsnSink};
use crate::groups::{Groups, Hold};
use crate::milter::{Milters, Verdict};
use crate::server::{Envelope, Handler, Mode, Reply, is_positive, reply};
use crate::tracking::{self, Mail, Step, Tracking, Trail};
use tp_contracts::delivery::{DeliveryDecision as Dd, DeliveryEventKind as Ek};

/// Зависимости доставки, выбранные приложением.
#[derive(Clone)]
pub struct Deps {
    /// Организация, которой принадлежат локальные домены узла (модуль «Мультитенантность» — позже).
    pub tenant: TenantId,
    /// Почтовое хранилище.
    pub store: Arc<dyn MailStore>,
    /// Учётные записи.
    pub accounts: Arc<dyn AccountStore>,
    /// Папки: «Входящие» и «Спам» получателя.
    pub mailboxes: Arc<dyn MailboxStore>,
    /// Каталог: домены, псевдонимы, адреса учётных записей, группы. `None` — только `delivery.local_domains`
    /// и имена входа.
    pub directory: Option<Arc<dyn Directory>>,
    /// Квоты: переполненный ящик отклоняется на `RCPT`.
    pub quota: Option<Arc<dyn Quota>>,
    /// Очередь исходящей почты; `None` — узел не отправляет наружу (submission: `451` внешним получателям).
    pub outbound: Option<Arc<dyn Outbound>>,
    /// Точки принятия решений модулей (`on_submission`; `on_delivery` вызывает хранилище).
    pub hooks: DecisionHooks,
    /// Проверки SPF, DKIM, DMARC, ARC входящей почты (smtp-in); `None` — без проверок.
    pub authcheck: Option<Arc<AuthCheck>>,
    /// Фильтры Milter; `None` — без фильтров.
    pub milter: Option<Arc<Milters>>,
}

impl fmt::Debug for Deps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deps")
            .field("tenant", &self.tenant)
            .field("directory", &self.directory.is_some())
            .field("outbound", &self.outbound.is_some())
            .field("authcheck", &self.authcheck.is_some())
            .field("milter", &self.milter.is_some())
            .finish_non_exhaustive()
    }
}

/// Куда разрешился адрес получателя.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// Локальные учётные записи (адрес, псевдоним или группа).
    Local(Vec<Account>),
    /// Домен не локальный.
    External,
    /// Локальный домен, но адреса нет (или все учётные записи отключены).
    Unknown,
}

/// Снимок каталога: локальные домены и адрес → имена входа.
#[derive(Debug, Default)]
struct Snapshot {
    domains: HashSet<String>,
    addresses: HashMap<String, BTreeSet<String>>,
}

/// Наибольшая глубина вложенности групп каталога.
const MAX_GROUP_DEPTH: usize = 8;

/// Получатель после переписывания и разрешения.
struct Target {
    /// Адрес после переписывания ([`Addressing::rewrite`]).
    address: String,
    resolved: Resolved,
    /// Подадрес (`user+tag`): полный адрес для заголовка `Delivered-To` копии.
    delivered_to: Option<String>,
    /// Адрес — группа рассылки [`Groups`] (ограничения отправителей, модерация).
    group: Option<DistributionGroup>,
}

/// Чьё письмо разрешается: отправителя (ограничения и модерация групп) или модуля (копии и пересылки точек
/// принятия решений — группы только раскрываются).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Origin {
    /// Отправитель: учётная запись организации или `None` — внешний.
    Sender(Option<AccountId>),
    /// Копия или пересылка модуля.
    Module,
}

/// Локальная доставка и обработчик транзакций SMTP.
pub struct Delivery {
    deps: Deps,
    hostname: String,
    cfg: DeliveryConfig,
    addressing: Addressing,
    snapshot: Mutex<Option<(Instant, Arc<Snapshot>)>>,
    groups: Option<Groups>,
    tracking: Option<Tracking>,
}

impl fmt::Debug for Delivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Delivery")
            .field("deps", &self.deps)
            .field("hostname", &self.hostname)
            .finish_non_exhaustive()
    }
}

/// Письмо, подготовленное к доставке.
struct Prepared {
    deliveries: Vec<(String, MailDelivery)>,
    /// Внешние адреса (после переписывания).
    external: Vec<String>,
    /// Внешние получатели в исходной записи (статусы отправки).
    queued: Vec<String>,
    /// Модерируемые группы: исходный адрес получателя и группа (по одной записи на группу).
    held: Vec<(String, DistributionGroup)>,
    /// Получатели, которым письмо не доставлено после приёма (уведомление отправителю).
    failed: Vec<dsn::Recipient>,
}

impl Delivery {
    /// Доставка с зависимостями `deps` и параметрами секции `smtp`.
    pub fn new(deps: Deps, cfg: &SmtpConfig) -> Self {
        Self {
            deps,
            hostname: cfg.hostname.clone(),
            cfg: cfg.delivery.clone(),
            addressing: Addressing::new(&cfg.addressing),
            snapshot: Mutex::new(None),
            groups: None,
            tracking: None,
        }
    }

    /// Журнал доставки (SMTP-17, [`crate::tracking`]): приём, отказы, доставка, модерация, уведомления.
    pub fn with_tracking(mut self, tracking: Tracking) -> Self {
        self.tracking = Some(tracking);
        self
    }

    /// Записывает события письма в журнал доставки (если подключён).
    async fn track(&self, sender: &str, message_id: Option<&str>, steps: Vec<Step>) {
        if let Some(t) = &self.tracking {
            let mail = Mail {
                tenant: self.deps.tenant,
                sender,
                message_id,
                queue_id: None,
                at: OffsetDateTime::now_utc(),
            };
            t.write(&mail, steps).await;
        }
    }

    /// Группы рассылки с ограничениями отправителей и модерацией (SMTP-16, [`crate::groups`]).
    pub fn with_groups(mut self, groups: Groups) -> Self {
        self.groups = Some(groups);
        self
    }

    /// Разрешает адрес получателя: переписывание ([`crate::address`]), затем адрес целиком, адрес без тега
    /// подадреса, адрес для всей почты домена (псевдоним каталога `@domain`).
    pub async fn resolve(&self, address: &str) -> Result<Resolved> {
        Ok(self.target(address).await?.resolved)
    }

    async fn target(&self, address: &str) -> Result<Target> {
        let address = self
            .addressing
            .rewrite(address, Side::Recipient)
            .unwrap_or_else(|| address.to_owned());
        let lower = address.to_lowercase();
        let target = |resolved, delivered_to| Target {
            address: address.clone(),
            resolved,
            delivered_to,
            group: None,
        };
        let Some((_, domain)) = lower.rsplit_once('@') else {
            return Ok(target(Resolved::Unknown, None));
        };
        let snap = self.snapshot().await?;
        if !snap.domains.contains(domain) {
            return Ok(target(Resolved::External, None));
        }
        if let Some(groups) = &self.groups
            && let Some(group) = groups.by_address(self.deps.tenant, &lower).await?
        {
            let mut accounts = Vec::new();
            for id in groups.members(self.deps.tenant, &group).await? {
                if let Some(acc) = self.deps.accounts.get(self.deps.tenant, id).await?
                    && acc.enabled
                {
                    accounts.push(acc);
                }
            }
            return Ok(Target {
                group: Some(group),
                ..target(Resolved::Local(accounts), None)
            });
        }
        if let Some(accounts) = self.lookup(&snap, &lower).await? {
            return Ok(target(Resolved::Local(accounts), None));
        }
        if let Some((base, _tag)) = self.addressing.split(&lower)
            && let Some(accounts) = self.lookup(&snap, &base).await?
        {
            return Ok(target(Resolved::Local(accounts), Some(address.clone())));
        }
        let catch_all = format!("@{domain}");
        if snap.addresses.contains_key(&catch_all)
            && let Some(accounts) = self.lookup(&snap, &catch_all).await?
        {
            return Ok(target(Resolved::Local(accounts), None));
        }
        Ok(target(Resolved::Unknown, None))
    }

    /// Включённые учётные записи адреса `address` (нижний регистр): по снимку каталога или как имя входа.
    async fn lookup(&self, snap: &Snapshot, address: &str) -> Result<Option<Vec<Account>>> {
        let logins = match snap.addresses.get(address) {
            Some(logins) => logins.iter().cloned().collect(),
            None => vec![address.to_owned()],
        };
        let mut accounts = Vec::new();
        for login in logins {
            if let Some(acc) = self
                .deps
                .accounts
                .by_login(self.deps.tenant, &login)
                .await?
                && acc.enabled
                && !accounts.iter().any(|a: &Account| a.id == acc.id)
            {
                accounts.push(acc);
            }
        }
        Ok((!accounts.is_empty()).then_some(accounts))
    }

    async fn snapshot(&self) -> Result<Arc<Snapshot>> {
        if let Some((at, snap)) = &*lock(&self.snapshot)
            && at.elapsed() < self.cfg.directory_refresh
        {
            return Ok(snap.clone());
        }
        let snap = Arc::new(self.load_snapshot().await?);
        *lock(&self.snapshot) = Some((Instant::now(), snap.clone()));
        Ok(snap)
    }

    async fn load_snapshot(&self) -> Result<Snapshot> {
        let mut snap = Snapshot {
            domains: self
                .cfg
                .local_domains
                .iter()
                .map(|d| d.to_lowercase())
                .collect(),
            addresses: HashMap::new(),
        };
        let Some(dir) = &self.deps.directory else {
            return Ok(snap);
        };
        let tenant = self.deps.tenant;
        snap.domains.extend(
            dir.domains(tenant)
                .await?
                .into_iter()
                .map(|d| d.name.to_lowercase()),
        );
        // external_id → (имя входа, адреса); группы: external_id → (адрес, члены).
        let mut accounts = HashMap::new();
        let mut cursor: Option<DirCursor> = None;
        loop {
            let page = dir.accounts_since(tenant, cursor.as_ref(), 1000).await?;
            for change in page.changes {
                match change {
                    DirChange::Upsert(a) if a.enabled => {
                        accounts.insert(a.external_id.clone(), (a.login, a.emails));
                    }
                    DirChange::Upsert(a) => {
                        accounts.remove(&a.external_id);
                    }
                    DirChange::Removed { external_id } => {
                        accounts.remove(&external_id);
                    }
                }
            }
            cursor = Some(page.next);
            if !page.has_more {
                break;
            }
        }
        let mut groups = HashMap::new();
        let mut cursor: Option<DirCursor> = None;
        loop {
            let page = dir.groups_since(tenant, cursor.as_ref(), 1000).await?;
            for change in page.changes {
                match change {
                    DirChange::Upsert(g) => {
                        groups.insert(g.external_id.clone(), (g.email, g.members));
                    }
                    DirChange::Removed { external_id } => {
                        groups.remove(&external_id);
                    }
                }
            }
            cursor = Some(page.next);
            if !page.has_more {
                break;
            }
        }
        // Имена входа по external_id учётной записи или группы (с раскрытием вложенных групп).
        let expand = |root: &str| {
            let mut out = BTreeSet::new();
            let mut seen = HashSet::new();
            let mut stack = vec![(root.to_owned(), 0usize)];
            while let Some((id, depth)) = stack.pop() {
                if !seen.insert(id.clone()) {
                    continue;
                }
                if let Some((login, _)) = accounts.get(&id) {
                    out.insert(login.to_lowercase());
                } else if let Some((_, members)) = groups.get(&id)
                    && depth < MAX_GROUP_DEPTH
                {
                    stack.extend(members.iter().map(|m| (m.clone(), depth + 1)));
                }
            }
            out
        };
        let mut addresses: HashMap<String, BTreeSet<String>> = HashMap::new();
        for (login, emails) in accounts.values() {
            for email in emails {
                addresses
                    .entry(email.to_lowercase())
                    .or_default()
                    .insert(login.to_lowercase());
            }
        }
        for (id, (email, _)) in &groups {
            if let Some(email) = email {
                addresses
                    .entry(email.to_lowercase())
                    .or_default()
                    .extend(expand(id));
            }
        }
        for alias in dir.aliases(tenant).await? {
            addresses
                .entry(alias.address.to_lowercase())
                .or_default()
                .extend(expand(&alias.target));
        }
        snap.addresses = addresses;
        Ok(snap)
    }

    /// Папка доставки: «Спам» (`junk`) или «Входящие».
    async fn mailbox(&self, scope: AccountScope, junk: bool) -> Result<MailboxId> {
        let list = self.deps.mailboxes.list(scope).await?;
        let find = |role| list.iter().find(|m| m.role == Some(role)).map(|m| m.id);
        let found = if junk {
            find(SpecialUse::Junk).or_else(|| find(SpecialUse::Inbox))
        } else {
            find(SpecialUse::Inbox)
        };
        found.ok_or_else(|| Error::Internal(format!("account {} has no inbox", scope.account)))
    }

    /// Разрешает всех получателей конверта: доставки (по одной на учётную запись), внешние адреса,
    /// модерируемые группы. Отправитель вне круга группы (принят на `RCPT` до изменения группы, отправка
    /// из JMAP) — в `failed`.
    async fn prepare(
        &self,
        id: &str,
        rcpts: &[String],
        junk: bool,
        origin: Origin,
    ) -> Result<Prepared> {
        let mut out = Prepared {
            deliveries: Vec::new(),
            external: Vec::new(),
            queued: Vec::new(),
            held: Vec::new(),
            failed: Vec::new(),
        };
        for rcpt in rcpts {
            let target = self.target(rcpt).await?;
            if let (Some(group), Origin::Sender(sender)) = (&target.group, origin) {
                if !self.group_allows(group, sender).await? {
                    tracing::info!(%id, group = %group.id, "smtp: отправитель вне круга группы рассылки");
                    out.failed.push(dsn::Recipient::new(
                        rcpt.clone(),
                        dsn::Action::Failed,
                        GROUP_DENIED,
                    ));
                    continue;
                }
                if group.requires_moderation(sender) {
                    if !out.held.iter().any(|(_, g)| g.id == group.id) {
                        out.held.push((rcpt.clone(), group.clone()));
                    }
                    continue;
                }
            }
            match target.resolved {
                Resolved::Local(accounts) => {
                    for acc in accounts {
                        if out.deliveries.iter().any(|(_, d)| d.account == acc.id) {
                            continue;
                        }
                        let scope = AccountScope {
                            tenant: acc.tenant,
                            account: acc.id,
                        };
                        let mailbox = self.mailbox(scope, junk).await?;
                        out.deliveries.push((
                            rcpt.clone(),
                            MailDelivery {
                                account: acc.id,
                                mailbox,
                                extra_headers: target
                                    .delivered_to
                                    .iter()
                                    .map(|a| ("Delivered-To".to_owned(), clean(a)))
                                    .collect(),
                                flags: Default::default(),
                            },
                        ));
                    }
                }
                Resolved::External => {
                    if !out
                        .external
                        .iter()
                        .any(|x| x.eq_ignore_ascii_case(&target.address))
                    {
                        out.external.push(target.address);
                    }
                    out.queued.push(rcpt.clone());
                }
                // Получатель принят на RCPT, но исчез из каталога (или неизвестен при отправке из JMAP):
                // письмо ему не доставляется, отправителю — уведомление.
                Resolved::Unknown => {
                    tracing::warn!(%id, "smtp: локальный получатель не найден");
                    out.failed.push(dsn::Recipient::new(
                        rcpt.clone(),
                        dsn::Action::Failed,
                        "550 5.1.1 mailbox unavailable",
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Может ли `sender` писать в группу (без [`Groups`] групп-адресатов нет — да).
    async fn group_allows(
        &self,
        group: &DistributionGroup,
        sender: Option<AccountId>,
    ) -> Result<bool> {
        match &self.groups {
            Some(g) => g.allowed(self.deps.tenant, group, sender).await,
            None => Ok(true),
        }
    }

    /// Учётная запись отправителя по адресу (отправка вне сессии SMTP: адрес проверил вызывающий):
    /// ровно одна учётная запись организации, иначе — внешний.
    async fn sender_by_address(&self, mail_from: &str) -> Result<Option<AccountId>> {
        if mail_from.is_empty() {
            return Ok(None);
        }
        Ok(match self.target(mail_from).await? {
            Target {
                resolved: Resolved::Local(accounts),
                group: None,
                ..
            } if accounts.len() == 1 => Some(accounts[0].id),
            _ => None,
        })
    }

    /// Ставит письмо `raw` в очереди модерации групп `held`.
    async fn hold_all(
        &self,
        held: &[(String, DistributionGroup)],
        mail_from: &str,
        sender: Option<AccountId>,
        raw: &Bytes,
        at: OffsetDateTime,
    ) -> Result<()> {
        let Some(groups) = &self.groups else {
            return Ok(());
        };
        for (_, group) in held {
            let id = groups
                .hold(Hold {
                    tenant: self.deps.tenant,
                    group,
                    sender: mail_from,
                    sender_account: sender,
                    message_id: crate::tracking::message_id(raw),
                    raw: raw.clone(),
                    at,
                })
                .await?;
            tracing::info!(group = %group.id, held = %id, "smtp: письмо ждёт решения модератора группы");
        }
        Ok(())
    }

    /// Заголовки трассировки (RFC 5321, 4.4): `Return-Path` и `Received`.
    fn trace_headers(&self, env: &Envelope, received_at: OffsetDateTime) -> String {
        let proto = match (
            env.smtputf8,
            env.esmtp,
            env.conn.tls,
            env.principal.is_some(),
        ) {
            (true, _, true, true) => "UTF8SMTPSA",
            (true, _, true, false) => "UTF8SMTPS",
            (true, _, false, true) => "UTF8SMTPA",
            (true, _, false, false) => "UTF8SMTP",
            (false, true, true, true) => "ESMTPSA",
            (false, true, true, false) => "ESMTPS",
            (false, true, false, true) => "ESMTPA",
            (false, true, false, false) => "ESMTP",
            (false, false, _, _) => "SMTP",
        };
        let ip = match env.conn.client.ip().to_canonical() {
            std::net::IpAddr::V4(ip) => ip.to_string(),
            std::net::IpAddr::V6(ip) => format!("IPv6:{ip}"),
        };
        // Дата RFC 5322 — mail-builder (UTC).
        let date = mail_builder::headers::date::Date::new(received_at.unix_timestamp()).to_rfc822();
        let mut h = format!(
            "Return-Path: <{}>\r\nReceived: from {} ([{ip}])\r\n\tby {} (The Post) with {proto} id {}",
            clean(&env.mail_from),
            clean(&env.helo),
            self.hostname,
            env.id,
        );
        // Адрес получателя — только при одном получателе: остальные не должны видеть друг друга.
        if let [rcpt] = env.rcpts.as_slice() {
            h.push_str(&format!("\r\n\tfor <{}>", clean(rcpt)));
        }
        h.push_str(&format!(";\r\n\t{date}\r\n"));
        h
    }

    async fn ingest(
        &self,
        mail_from: &str,
        raw: Bytes,
        deliveries: Vec<MailDelivery>,
        received_at: OffsetDateTime,
    ) -> Result<Ingested> {
        self.deps
            .store
            .ingest(IngestRequest {
                tenant: self.deps.tenant,
                mail_from: mail_from.to_owned(),
                raw: Box::pin(futures::stream::once(async move { Ok(raw) })),
                deliveries,
                received_at,
            })
            .await
    }

    /// `ingest`; превышение квоты — повтор без переполненных ящиков (их адреса — в `failed`, отправителю
    /// уведомление).
    async fn ingest_all(
        &self,
        id: &str,
        mail_from: &str,
        raw: Bytes,
        mut deliveries: Vec<(String, MailDelivery)>,
        received_at: OffsetDateTime,
        failed: &mut Vec<dsn::Recipient>,
    ) -> Result<Ingested> {
        let all = deliveries.iter().map(|(_, d)| d.clone()).collect();
        match self.ingest(mail_from, raw.clone(), all, received_at).await {
            Err(Error::QuotaExceeded) => {}
            other => return other,
        }
        let Some(quota) = &self.deps.quota else {
            return Err(Error::QuotaExceeded);
        };
        let mut fits = Vec::new();
        let mut full = Vec::new();
        for (rcpt, d) in deliveries.drain(..) {
            let scope = AccountScope {
                tenant: self.deps.tenant,
                account: d.account,
            };
            match quota.check(scope, raw.len() as u64).await {
                Ok(()) => fits.push(d),
                Err(Error::QuotaExceeded) => {
                    tracing::warn!(%id, account = %d.account, "smtp: ящик переполнен, письмо не доставлено");
                    // Копия точки принятия решений (без адреса) уведомления не даёт.
                    if !rcpt.is_empty() && !full.contains(&rcpt) {
                        full.push(rcpt);
                    }
                }
                Err(e) => return Err(e),
            }
        }
        if fits.is_empty() {
            return Err(Error::QuotaExceeded);
        }
        let out = self.ingest(mail_from, raw, fits, received_at).await?;
        failed.extend(
            full.into_iter()
                .map(|r| dsn::Recipient::new(r, dsn::Action::Failed, "552 5.2.2 mailbox full")),
        );
        Ok(out)
    }

    /// Ставит в очередь исходящей почты внешних получателей и пересылки.
    async fn queue_out(
        &self,
        id: &str,
        mail_from: &str,
        rcpts: Vec<String>,
        raw: Bytes,
    ) -> Result<()> {
        if rcpts.is_empty() {
            return Ok(());
        }
        match &self.deps.outbound {
            Some(q) => q.enqueue(self.deps.tenant, mail_from, &rcpts, raw).await,
            None => {
                // Узел без очереди: внешних получателей submission не принимает (RCPT — 451, `submit` —
                // `SubmitStatus::Failed`), здесь остаются только внешние пересылки и копии модулей.
                tracing::warn!(
                    %id,
                    count = rcpts.len(),
                    "smtp: очередь исходящей почты не подключена, пересылка не выполнена"
                );
                Ok(())
            }
        }
    }

    /// Копии точки принятия решений на адреса (`CopyTo::Address`): локальные — доставки в ящики (без адреса
    /// получателя: копия уведомления отправителю не даёт и адрес ему не раскрывает), внешние — в очередь.
    async fn add_copies(
        &self,
        id: &str,
        prepared: &mut Prepared,
        addresses: &[String],
        junk: bool,
    ) -> Result<()> {
        if addresses.is_empty() {
            return Ok(());
        }
        let copies = self.prepare(id, addresses, junk, Origin::Module).await?;
        for (_, d) in copies.deliveries {
            if !prepared
                .deliveries
                .iter()
                .any(|(_, x)| x.account == d.account)
            {
                prepared.deliveries.push((String::new(), d));
            }
        }
        push_unique(&mut prepared.external, copies.external);
        Ok(())
    }

    /// Пересылки модулей (`Ingested::forward`): локальные адреса — сразу в ящики (CMP-04, SMTP-28), их
    /// пересылки — так же, пока есть учётные записи, ещё не получившие письмо (`delivered`; защита от
    /// петель). Неизвестный адрес и переполненный ящик — только в журнал: адрес пересылки отправителю не
    /// раскрывается. Возвращает внешние адреса — для очереди.
    #[allow(clippy::too_many_arguments)]
    async fn forward(
        &self,
        id: &str,
        mail_from: &str,
        raw: &Bytes,
        mut forward: Vec<String>,
        junk: bool,
        received_at: OffsetDateTime,
        mut delivered: HashSet<AccountId>,
    ) -> Result<Vec<String>> {
        let mut external = Vec::new();
        while !forward.is_empty() {
            let p = self.prepare(id, &forward, junk, Origin::Module).await?;
            push_unique(&mut external, p.external);
            let deliveries: Vec<_> = p
                .deliveries
                .into_iter()
                .filter(|(_, d)| delivered.insert(d.account))
                .map(|(_, d)| (String::new(), d))
                .collect();
            if deliveries.is_empty() {
                break;
            }
            let mut ignored = Vec::new();
            forward = match self
                .ingest_all(
                    id,
                    mail_from,
                    raw.clone(),
                    deliveries,
                    received_at,
                    &mut ignored,
                )
                .await
            {
                Ok(ingested) => ingested.forward,
                Err(Error::QuotaExceeded) => {
                    tracing::warn!(%id, "smtp: пересылка не доставлена, ящики переполнены");
                    Vec::new()
                }
                Err(e) => return Err(e),
            };
        }
        Ok(external)
    }

    /// Принимает письмо: проверки, фильтры, доставка и очередь. Ответ фильтра (отказ, отброс) — `Ok(Some)`.
    async fn accept(&self, env: &Envelope, raw: Bytes, trail: &mut Trail) -> Result<Option<Reply>> {
        let received_at = OffsetDateTime::now_utc();
        trail.message_id = tracking::message_id(&raw);
        let check = match (&self.deps.authcheck, env.mode) {
            (Some(ac), Mode::Inbound) => Some(ac.check(env, &raw).await),
            _ => None,
        };
        // Отправитель конверта переписывается после проверок (SPF — по исходному).
        let rewritten = self
            .addressing
            .rewrite(&env.mail_from, Side::Sender)
            .map(|mail_from| Envelope {
                mail_from,
                ..env.clone()
            });
        let env = rewritten.as_ref().unwrap_or(env);
        // DMARC не пройден при политике quarantine/reject — в «Спам».
        let mut junk = check.as_ref().is_some_and(|c| c.junk);
        let mut headers = self.trace_headers(env, received_at);
        if let Some(c) = &check {
            // Authentication-Results и Received-SPF — выше Received этого узла (RFC 8601, 7208).
            headers.insert_str(0, &c.headers);
        }
        let mut full = Vec::with_capacity(headers.len() + raw.len());
        full.extend_from_slice(headers.as_bytes());
        full.extend_from_slice(&raw);
        let mut full = Bytes::from(full);
        if let Some(m) = self.deps.milter.as_ref().filter(|m| m.applies(env.mode)) {
            let out = m.check(env, full).await;
            match out.verdict {
                Verdict::Accept => {}
                Verdict::Refuse(r) => return Ok(Some(r)),
                Verdict::Discard => {
                    trail.decision = Some(Dd::Discard);
                    return Ok(Some(reply(250, [2, 0, 0], format!("OK id={}", env.id))));
                }
            }
            if let Some(reason) = &out.quarantine {
                tracing::info!(id = %env.id, %reason, "smtp: письмо в карантине фильтра");
                match env.mode {
                    Mode::Inbound => junk = true,
                    Mode::Submission => {
                        return Ok(Some(reply(
                            550,
                            [5, 7, 1],
                            "Message held by content filter",
                        )));
                    }
                }
            }
            full = out.message;
        }
        let mut copies = Vec::new();
        if env.mode == Mode::Submission
            && let Some(principal) = &env.principal
            && !self.deps.hooks.is_empty()
        {
            let info = SubmissionInfo {
                principal: principal.clone(),
                mail_from: env.mail_from.clone(),
                rcpt_to: env.rcpts.clone(),
                size: full.len() as u64,
            };
            match self.deps.hooks.on_submission(&info).await? {
                Decision::Allow => {}
                Decision::Deny(reason) => {
                    tracing::info!(id = %env.id, %reason, "smtp: отправка запрещена точкой принятия решений");
                    return Ok(Some(reply(550, [5, 7, 1], "Message rejected by policy")));
                }
                Decision::Augment(a) => {
                    let mut h = String::new();
                    for (name, value) in &a.headers {
                        h.push_str(&format!("{}: {}\r\n", clean(name), clean(value)));
                    }
                    let mut m = Vec::with_capacity(h.len() + full.len());
                    m.extend_from_slice(h.as_bytes());
                    m.extend_from_slice(&full);
                    full = Bytes::from(m);
                    copies = a.copies;
                }
            }
        }
        if junk {
            trail.decision = Some(Dd::Quarantine);
        }
        let sender = sender_account(env, self.deps.tenant);
        let mut prepared = self
            .prepare(&env.id, &env.rcpts, junk, Origin::Sender(sender))
            .await?;
        // Очередь модерации — до доставки: её сбой — временный отказ без частичной доставки.
        self.hold_all(&prepared.held, &env.mail_from, sender, &full, received_at)
            .await?;
        let mut copy_to = Vec::new();
        for copy in copies {
            match copy {
                CopyTo::Mailbox(d) => {
                    if !prepared
                        .deliveries
                        .iter()
                        .any(|(_, x)| x.account == d.account && x.mailbox == d.mailbox)
                    {
                        prepared.deliveries.push((String::new(), d));
                    }
                }
                CopyTo::Address(a) => copy_to.push(a),
            }
        }
        self.add_copies(&env.id, &mut prepared, &copy_to, junk)
            .await?;
        let mut rcpts = prepared.external;
        let mut failed = prepared.failed;
        let local = local_rcpts(&prepared.deliveries);
        if !prepared.deliveries.is_empty() {
            let delivered = accounts_of(&prepared.deliveries);
            let forward = self
                .ingest_all(
                    &env.id,
                    &env.mail_from,
                    full.clone(),
                    prepared.deliveries,
                    received_at,
                    &mut failed,
                )
                .await?
                .forward;
            let external = self
                .forward(
                    &env.id,
                    &env.mail_from,
                    &full,
                    forward,
                    junk,
                    received_at,
                    delivered,
                )
                .await?;
            push_unique(&mut rcpts, external);
        }
        self.queue_out(&env.id, &env.mail_from, rcpts, full.clone())
            .await?;
        outcome_steps(&mut trail.steps, &prepared.held, &local, &failed, junk);
        if !failed.is_empty() && !env.mail_from.is_empty() {
            let sent = self
                .bounce(&env.id, &env.mail_from, &failed, &full, received_at)
                .await;
            trail.steps.push(dsn_step(&env.mail_from, sent));
        }
        Ok(None)
    }

    /// Уведомление о недоставке после приёма (`250` уже не отменить); сбой — только в журнал.
    async fn bounce(
        &self,
        id: &str,
        mail_from: &str,
        rcpts: &[dsn::Recipient],
        original: &[u8],
        received_at: OffsetDateTime,
    ) -> bool {
        let raw = dsn::build(&dsn::Report {
            hostname: &self.hostname,
            to: mail_from,
            envid: None,
            arrival: received_at,
            now: OffsetDateTime::now_utc(),
            retry_until: None,
            rcpts,
            original,
            ret: DsnRet::Headers,
            id,
        });
        if let Err(e) = self
            .send_dsn(self.deps.tenant, mail_from, Bytes::from(raw))
            .await
        {
            tracing::warn!(%id, error = %e, "smtp: уведомление о недоставке не отправлено");
            return false;
        }
        true
    }

    /// Отправка от имени пользователя вне сессии SMTP (JMAP `EmailSubmission/set`, CMP-04): получатели в
    /// локальных доменах (`delivery.local_domains` и каталог: домены, адреса, псевдонимы, группы)
    /// доставляются сразу через [`MailStore::ingest`] — как входящая почта, без очереди, MX, MTA-STS и DANE;
    /// внешние — в очередь исходящей почты ([`Deps::outbound`]) письмом `raw` без изменений. Пересылки модулей
    /// на локальные адреса — сразу в ящики, на внешние — в очередь.
    ///
    /// Неизвестный локальный адрес и переполненный ящик — статус [`SubmitStatus::Failed`] и уведомление о
    /// недоставке отправителю (как для входящей почты). Узел без очереди: внешние получатели —
    /// `SubmitStatus::Failed` с временной ошибкой [`NO_OUTBOUND`] без уведомления (статус получает отправитель),
    /// локальные доставляются. Ошибка хранилища, каталога или очереди — `Err`.
    /// Другая организация (`tenant` ≠ `Deps::tenant`) — все получатели в очередь, как до CMP-04 (без очереди —
    /// все `Failed`).
    pub async fn submit(
        &self,
        tenant: TenantId,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<Submitted> {
        let id = crate::server::next_id();
        let rewritten = self.addressing.rewrite(mail_from, Side::Sender);
        let mail_from = rewritten.as_deref().unwrap_or(mail_from);
        if tenant != self.deps.tenant {
            let status = match &self.deps.outbound {
                Some(q) => {
                    q.enqueue(tenant, mail_from, rcpts, raw).await?;
                    SubmitStatus::Queued
                }
                None => {
                    no_outbound(&id, rcpts.len());
                    SubmitStatus::Failed(NO_OUTBOUND.into())
                }
            };
            return Ok(Submitted::all(rcpts, status));
        }
        let sender = self.sender_by_address(mail_from).await?;
        let prepared = self
            .prepare(&id, rcpts, false, Origin::Sender(sender))
            .await?;
        let received_at = OffsetDateTime::now_utc();
        self.hold_all(&prepared.held, mail_from, sender, &raw, received_at)
            .await?;
        let queue = self.deps.outbound.is_some();
        if !queue && !prepared.external.is_empty() {
            no_outbound(&id, prepared.external.len());
        }
        let mut failed = prepared.failed;
        let mut forward = Vec::new();
        let local_list = local_rcpts(&prepared.deliveries);
        if !prepared.deliveries.is_empty() {
            let mut full = self
                .submit_headers(&id, mail_from, rcpts, received_at)
                .into_bytes();
            full.extend_from_slice(&raw);
            let full = Bytes::from(full);
            let local: Vec<String> = prepared.deliveries.iter().map(|(r, _)| r.clone()).collect();
            let delivered = accounts_of(&prepared.deliveries);
            match self
                .ingest_all(
                    &id,
                    mail_from,
                    full.clone(),
                    prepared.deliveries,
                    received_at,
                    &mut failed,
                )
                .await
            {
                Ok(ingested) => {
                    forward = self
                        .forward(
                            &id,
                            mail_from,
                            &full,
                            ingested.forward,
                            false,
                            received_at,
                            delivered,
                        )
                        .await?;
                }
                // Все ящики переполнены: письмо принято отправкой, отправителю — уведомление.
                Err(Error::QuotaExceeded) => {
                    for r in local {
                        if !failed.iter().any(|f| f.address == r) {
                            failed.push(dsn::Recipient::new(
                                r,
                                dsn::Action::Failed,
                                "552 5.2.2 mailbox full",
                            ));
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        let statuses = rcpts
            .iter()
            .map(|r| {
                let status =
                    if let Some(f) = failed.iter().find(|f| f.address.eq_ignore_ascii_case(r)) {
                        SubmitStatus::Failed(f.reply.clone())
                    } else if prepared.held.iter().any(|(x, _)| x.eq_ignore_ascii_case(r)) {
                        // Ждёт решения модератора группы.
                        SubmitStatus::Queued
                    } else if prepared.queued.iter().any(|x| x.eq_ignore_ascii_case(r)) {
                        if queue {
                            SubmitStatus::Queued
                        } else {
                            SubmitStatus::Failed(NO_OUTBOUND.into())
                        }
                    } else {
                        SubmitStatus::Delivered
                    };
                (r.clone(), status)
            })
            .collect();
        let mut out = if queue { prepared.external } else { Vec::new() };
        push_unique(&mut out, forward);
        self.queue_out(&id, mail_from, out, raw.clone()).await?;
        let mut steps = vec![Step::new(Ek::Receive, Dd::Accept, rcpts.to_vec())];
        outcome_steps(&mut steps, &prepared.held, &local_list, &failed, false);
        if !failed.is_empty() && !mail_from.is_empty() {
            let sent = self
                .bounce(&id, mail_from, &failed, &raw, received_at)
                .await;
            steps.push(dsn_step(mail_from, sent));
        }
        self.track(mail_from, tracking::message_id(&raw).as_deref(), steps)
            .await;
        tracing::info!(%id, count = rcpts.len(), "smtp: отправка вне сессии обработана");
        Ok(Submitted { rcpts: statuses })
    }

    /// Заголовки трассировки письма, отправленного вне сессии SMTP ([`Delivery::submit`]).
    fn submit_headers(
        &self,
        id: &str,
        mail_from: &str,
        rcpts: &[String],
        received_at: OffsetDateTime,
    ) -> String {
        let date = mail_builder::headers::date::Date::new(received_at.unix_timestamp()).to_rfc822();
        let mut h = format!(
            "Return-Path: <{}>\r\nReceived: by {} (The Post) id {id}",
            clean(mail_from),
            self.hostname,
        );
        if let [rcpt] = rcpts {
            h.push_str(&format!("\r\n\tfor <{}>", clean(rcpt)));
        }
        h.push_str(&format!(";\r\n\t{date}\r\n"));
        h
    }
}

/// Отправка через [`Delivery`]: локальные получатели — сразу в ящики, внешние — в очередь (CMP-04).
/// Приложение подключает её вместо [`crate::queue::Queue`] для JMAP `EmailSubmission/set`.
#[async_trait]
impl Outbound for Delivery {
    async fn enqueue(
        &self,
        tenant: TenantId,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<()> {
        let sent = Delivery::submit(self, tenant, mail_from, rcpts, raw).await?;
        // Узел без очереди и ни одной доставки: письмо никуда не попало — ошибка, повтор безопасен.
        // Иначе внешним получателям — только `Failed` в журнале (повтор задвоил бы локальную доставку).
        let lost = |s: &SubmitStatus| matches!(s, SubmitStatus::Failed(r) if r == NO_OUTBOUND);
        if !sent.rcpts.is_empty() && sent.rcpts.iter().all(|(_, s)| lost(s)) {
            return Err(Error::UNAVAILABLE);
        }
        Ok(())
    }

    async fn submit(
        &self,
        tenant: TenantId,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<Submitted> {
        Delivery::submit(self, tenant, mail_from, rcpts, raw).await
    }
}

/// Статус внешнего получателя [`Delivery::submit`] на узле без очереди исходящей почты (временная ошибка).
pub const NO_OUTBOUND: &str = "451 4.3.0 outbound queue unavailable";

/// Внешние получатели на узле без очереди исходящей почты — в журнал.
fn no_outbound(id: &str, count: usize) {
    tracing::warn!(
        %id,
        count,
        "smtp: очередь исходящей почты не подключена, внешним адресам отправка недоступна"
    );
}

/// Учётные записи доставок.
fn accounts_of(deliveries: &[(String, MailDelivery)]) -> HashSet<AccountId> {
    deliveries.iter().map(|(_, d)| d.account).collect()
}

/// Дописывает адреса `more` в `list` без повторов (регистр не учитывается).
fn push_unique(list: &mut Vec<String>, more: Vec<String>) {
    for a in more {
        if !list.iter().any(|x| x.eq_ignore_ascii_case(&a)) {
            list.push(a);
        }
    }
}

/// Уведомление о доставке: локальному адресату — во «Входящие», внешнему — в очередь исходящей почты.
#[async_trait]
impl DsnSink for Delivery {
    async fn send_dsn(&self, tenant: TenantId, to: &str, raw: Bytes) -> Result<()> {
        match self.resolve(to).await? {
            Resolved::Local(accounts) => {
                let mut deliveries = Vec::new();
                for acc in accounts {
                    let scope = AccountScope {
                        tenant: acc.tenant,
                        account: acc.id,
                    };
                    deliveries.push(MailDelivery {
                        account: acc.id,
                        mailbox: self.mailbox(scope, false).await?,
                        extra_headers: Vec::new(),
                        flags: Default::default(),
                    });
                }
                self.deps
                    .store
                    .ingest(IngestRequest {
                        tenant,
                        mail_from: String::new(),
                        raw: Box::pin(futures::stream::once(async move { Ok(raw) })),
                        deliveries,
                        received_at: OffsetDateTime::now_utc(),
                    })
                    .await
                    .map(|_| ())
            }
            Resolved::External => match &self.deps.outbound {
                Some(q) => q.enqueue(tenant, "", &[to.to_owned()], raw).await,
                None => {
                    tracing::warn!(
                        "smtp: уведомление внешнему адресу без очереди исходящей почты не отправлено"
                    );
                    Ok(())
                }
            },
            Resolved::Unknown => {
                tracing::warn!("smtp: адресат уведомления о доставке не найден");
                Ok(())
            }
        }
    }
}

#[async_trait]
impl Handler for Delivery {
    async fn rcpt(&self, env: &Envelope, rcpt: &str) -> Reply {
        let r = self.check_rcpt(env, rcpt).await;
        if !is_positive(&r) {
            let step = Step::new(Ek::Reject, tracking::decision(&r), vec![rcpt.to_owned()])
                .reason(tracking::reason(&r));
            self.track(&env.mail_from, None, vec![step]).await;
        }
        r
    }

    async fn data(&self, env: &Envelope, raw: Bytes) -> Reply {
        let mut trail = Trail::default();
        let r = match self.accept(env, raw, &mut trail).await {
            Ok(Some(r)) => r,
            Ok(None) => reply(250, [2, 0, 0], format!("OK id={}", env.id)),
            Err(Error::QuotaExceeded) => reply(552, [5, 2, 2], "Mailbox full"),
            Err(Error::Invalid(m)) => {
                tracing::info!(id = %env.id, error = %m, "smtp: письмо отклонено");
                reply(554, [5, 6, 0], "Message rejected")
            }
            Err(Error::Forbidden) => reply(554, [5, 7, 1], "Message rejected"),
            Err(e) => temporary(&e),
        };
        if self.tracking.is_some() {
            let head = if is_positive(&r) {
                Step::new(
                    Ek::Receive,
                    trail.decision.unwrap_or(Dd::Accept),
                    env.rcpts.clone(),
                )
            } else {
                Step::new(Ek::Reject, tracking::decision(&r), env.rcpts.clone())
                    .reason(tracking::reason(&r))
            };
            let mut steps = vec![head];
            steps.append(&mut trail.steps);
            self.track(&env.mail_from, trail.message_id.as_deref(), steps)
                .await;
        }
        r
    }
}

impl Delivery {
    /// Проверка получателя до ответа `250` (SRS 4.1): существование, группа рассылки, квота.
    async fn check_rcpt(&self, env: &Envelope, rcpt: &str) -> Reply {
        let target = match self.target(rcpt).await {
            Ok(t) => t,
            Err(e) => return temporary(&e),
        };
        if let Some(group) = &target.group {
            let sender = sender_account(env, self.deps.tenant);
            match self.group_allows(group, sender).await {
                Ok(true) => {}
                Ok(false) => {
                    tracing::info!(id = %env.id, group = %group.id, "smtp: отправитель вне круга группы рассылки");
                    return reply(550, [5, 7, 1], "Sender not allowed to send to this group");
                }
                Err(e) => return temporary(&e),
            }
            // Письмо модерируемой группы ждёт решения модератора: квоты членов не проверяются.
            if group.requires_moderation(sender) {
                return reply(250, [2, 1, 5], "OK");
            }
        }
        match target.resolved {
            Resolved::Local(accounts) => {
                if let Some(quota) = &self.deps.quota {
                    for acc in &accounts {
                        let scope = AccountScope {
                            tenant: acc.tenant,
                            account: acc.id,
                        };
                        match quota.check(scope, 0).await {
                            Ok(()) => {}
                            Err(Error::QuotaExceeded)
                                if accounts.len() == 1 && target.group.is_none() =>
                            {
                                return match self.cfg.quota_reject {
                                    QuotaReject::Temporary => reply(452, [4, 2, 2], "Mailbox full"),
                                    QuotaReject::Permanent => reply(552, [5, 2, 2], "Mailbox full"),
                                };
                            }
                            // Группа: переполненный ящик одного члена не отклоняет адрес.
                            Err(Error::QuotaExceeded) => {}
                            Err(e) => return temporary(&e),
                        }
                    }
                }
                reply(250, [2, 1, 5], "OK")
            }
            Resolved::External => match env.mode {
                Mode::Inbound => reply(550, [5, 7, 1], "Relaying denied"),
                Mode::Submission if self.deps.outbound.is_some() => reply(250, [2, 1, 5], "OK"),
                // Узел без очереди исходящей почты внешних получателей не принимает.
                Mode::Submission => reply(451, [4, 3, 2], "Outbound delivery not available"),
            },
            Resolved::Unknown => reply(550, [5, 1, 1], "Mailbox unavailable"),
        }
    }
}

/// Исходные адреса локальных получателей доставок без повторов (копии модулей — без адреса — не входят).
fn local_rcpts(deliveries: &[(String, MailDelivery)]) -> Vec<String> {
    let mut out = Vec::new();
    push_unique(
        &mut out,
        deliveries
            .iter()
            .filter(|(r, _)| !r.is_empty())
            .map(|(r, _)| r.clone())
            .collect(),
    );
    out
}

/// События журнала доставки после приёма: модерация, доставка в ящики (`local` без `failed`), отказы
/// получателям.
fn outcome_steps(
    steps: &mut Vec<Step>,
    held: &[(String, DistributionGroup)],
    local: &[String],
    failed: &[dsn::Recipient],
    junk: bool,
) {
    for (rcpt, _) in held {
        steps.push(
            Step::new(Ek::Deliver, Dd::Hold, vec![rcpt.clone()]).reason("held for moderation"),
        );
    }
    let delivered: Vec<String> = local
        .iter()
        .filter(|r| !failed.iter().any(|f| f.address.eq_ignore_ascii_case(r)))
        .cloned()
        .collect();
    if !delivered.is_empty() {
        let step = Step::new(Ek::Deliver, Dd::Accept, delivered);
        steps.push(if junk {
            step.reason("junk folder")
        } else {
            step
        });
    }
    for f in failed {
        steps.push(
            Step::new(
                Ek::Reject,
                tracking::decision_of_line(&f.reply),
                vec![f.address.clone()],
            )
            .reason(f.reply.clone()),
        );
    }
}

/// Событие уведомления о недоставке отправителю `to`: отправлено (`Accept`) или нет (`Defer`).
fn dsn_step(to: &str, sent: bool) -> Step {
    let (decision, reason) = if sent {
        (Dd::Accept, "failure notification")
    } else {
        (Dd::Defer, "failure notification not sent")
    };
    Step::new(Ek::Dsn, decision, vec![to.to_owned()])
        .sender("")
        .reason(reason)
}

/// Отказ отправителю вне круга группы рассылки после приёма (уведомление о недоставке).
const GROUP_DENIED: &str = "550 5.7.1 sender not allowed to send to this group";

/// Учётная запись отправителя транзакции: аутентифицированный пользователь организации; smtp-in — внешний.
fn sender_account(env: &Envelope, tenant: TenantId) -> Option<AccountId> {
    env.principal
        .as_ref()
        .filter(|p| env.mode == Mode::Submission && p.tenant == tenant)
        .map(|p| p.account)
}

/// Временная ошибка (`451 4.3.0`): хранилище или каталог недоступны.
fn temporary(e: &Error) -> Reply {
    tracing::warn!(error = %e, "smtp: временная ошибка");
    reply(451, [4, 3, 0], "Temporary failure, try again later")
}

/// Значение для заголовка: без управляющих символов (CR, LF) — защита от внедрения заголовков.
fn clean(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests;
