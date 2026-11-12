//! Локальная доставка (SRS 4.1, CONTRACTS.md#MailStore): разрешение получателей (учётные записи, псевдонимы и
//! группы каталога), заголовки `Return-Path` и `Received`, [`MailStore::ingest`] одним вызовом на письмо.
//!
//! [`Delivery`] — обработчик сессий ([`Handler`]) обеих ролей:
//! - локальный домен — только принятый домен организации узла ([`Delivery::with_transport`], `ClusterTransport`,
//!   решение 96); получатель в нём — учётные записи, на которые он разрешается; неизвестный — `550 5.1.1`;
//!   переполненный ящик — `452 4.2.2` или `552 5.2.2` (`delivery.quota_reject`) до `DATA`;
//! - группа рассылки ([`Delivery::with_groups`], [`crate::groups`]): отправитель вне круга — `550 5.7.1` на `RCPT`,
//!   письмо модерируемой группы — в очередь модерации, иначе группа раскрывается: учётные записи — в ящики,
//!   почтовые контакты — внешним адресатам; динамическая группа — по фильтру на момент доставки (SMTP-26);
//! - внешний получатель: smtp-in — `550 5.7.1` (не открытый релей), submission — в очередь исходящей почты
//!   ([`Outbound`], [`crate::queue::Queue`]); узел без очереди (`Deps::outbound` = `None`) — `451 4.3.2`;
//! - submission: до доставки и очереди — `DecisionHooks::on_submission` (`Deny` — `550 5.7.1`; дополнения:
//!   заголовки в начало письма, копии в папки и внешним адресам);
//! - журнал доставки ([`Delivery::with_tracking`], [`crate::tracking`]): приём, отказы, доставка, модерация,
//!   уведомления;
//! - адреса пересылки модулей (`Ingested::forward`, решение 2026-10-31-ingest-envelope; `CopyTo::Address`):
//!   локальные — сразу в ящики тем же `ingest` (SMTP-28), внешние — в очередь [`Outbound`] с отправителем SRS и
//!   ARC-подписью ([`Delivery::with_forwarding`], [`crate::forward`]); уведомление на адрес SRS — обратно
//!   исходному отправителю (подделанный или просроченный адрес — `550 5.1.1`);
//! - виды принятых доменов (SMTP-21): полномочный — неизвестный адрес `550 5.1.1`; внутренняя ретрансляция — неизвестный адрес принимается и ставится в очередь
//!   (smtp-out доставляет его на `next_hop` домена); домен другой организации — `451 4.3.0` (узел обслуживает
//!   одну организацию);
//! - доверенные источники ([`crate::sources`], SMTP-20): приложение — внешние получатели только при `external`,
//!   без проверок SPF/DKIM/DMARC; шлюз без данных об исходном клиенте — результаты проверок шлюза;
//! - фильтры Milter ([`crate::milter`]) двух направлений: приём — smtp-in; исходящая почта — submission и
//!   [`Delivery::submit`] (JMAP EmailSubmission, SMTP-27), до правил потока: отказ фильтра — отказ отправки,
//!   изменённые заголовки — дальше по ним;
//! - правила потока почты ([`Delivery::with_mail_flow`], `MailFlow`, SMTP-22) — один раз на письмо: smtp-in после
//!   Milter, submission и [`Delivery::submit`] — до точек принятия решений и очереди; `Reject` — `550 5.7.1`,
//!   `Drop` — `250` без доставки, изменённые конверт и тело — дальше по ним, `matched` — в журнал доставки;
//! - правила Sieve получателей ([`Delivery::with_rules`], [`crate::sieve`], SMTP-31, SMTP-32) — для каждого
//!   локального получателя (smtp-in, submission, [`Delivery::submit`]) после правил потока и до `ingest`: папки и
//!   флаги, отброс, перенаправление, автоответ; перенаправленное письмо и внутренний автоответ — через правила
//!   получателя с проверкой петли ([`Delivery::relay`], SMTP-34);
//! - приглашения iTIP в письмах ([`Delivery::with_itip`], [`crate::itip`]): письмо с частью `text/calendar;
//!   method=…` после доставки в ящик передаётся `ItipDelivery` получателя — smtp-in как iMIP (SMTP-33),
//!   submission и [`Delivery::submit`] как внутреннее планирование (решение 78, SMTP-36); письмо остаётся в ящике,
//!   отказ и сбой iTIP — только запись в журнал.
//! - личные списки отправителей ящика ([`Delivery::with_sender_lists`], `SenderLists`, SMTP-25) — при каждой
//!   доставке в ящик (smtp-in, submission, [`Delivery::submit`], пересылки и перенаправления): письмо
//!   заблокированного отправителя — в «Спам», надёжного — во «Входящие» поверх карантина фильтра Milter; сверяется
//!   `From` письма, затем отправитель конверта. Письмо с проваленным DMARC остаётся в «Спаме» и от «надёжного»:
//!   его адрес мог быть подделан;
//! - решения модератора (`tp_contracts::groups::Moderation`, CTR-31, CTR-43; FIX-01, `moderation`): одобренное письмо
//!   очереди модерации — участникам группы или исходным получателям правила потока без повторной модерации, отказ —
//!   уведомление отправителю (RFC 3464) с причиной модератора.
//!
//! Отправка вне сессии SMTP (JMAP `EmailSubmission/set`) — [`Delivery::submit`] и `impl Outbound for Delivery`
//! (CMP-04, контракт `tp_contracts::queue::Outbound`, CTR-28): локальные получатели доставляются сразу тем же
//! `ingest`, внешние — в очередь (без очереди — `SubmitStatus::Failed` с `451 4.3.0`, SMTP-28); статус по получателю.
//! Отложенная отправка и её отмена (`Outbound::schedule`, `cancel_scheduled`; SMTP-24) — `scheduled`.
//! Лимиты писем и адресатов в час учётной записи отправителя — общие с submission ([`Delivery::with_send_limits`],
//! [`crate::limits`], SMTP-43).
//!
//! Адресация ([`crate::address`]): получатели переписываются до разрешения, отправитель — после проверок, по
//! таблице организации из снимка настроек транспорта ([`Delivery::with_transport`], `TenantTransport::rewrites`);
//! подадрес `user+tag` доставляется в ящик `user` с `Delivered-To: user+tag@…`, неизвестный адрес домена — на
//! адрес для всей почты домена (псевдоним каталога `@domain`), если он задан.
//!
//! Каталог (псевдонимы, адреса учётных записей, группы) читается полным снимком и кэшируется на
//! `delivery.directory_refresh`; учётная запись — [`AccountStore::by_login`]. Проверки SPF, DKIM, DMARC и ARC
//! входящей почты — [`crate::authcheck`]: их заголовки добавляются к письму, письмо с проваленным DMARC
//! (`quarantine`, `reject`) доставляется в «Спам». Затем письмо проверяют фильтры Milter ([`crate::milter`]):
//! отказ фильтра — ответ на `DATA`, карантин — «Спам» (smtp-in) или отказ (submission).

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_trait::async_trait;
use bytes::Bytes;
use time::OffsetDateTime;
use tp_contracts::auth::{Authorizer, DirChange, DirCursor, Directory, Principal};
use tp_contracts::blob::{BlobIndex, BlobStore};
use tp_contracts::ext::{CopyTo, Decision, DecisionHooks, SubmissionInfo};
use tp_contracts::groups::DistributionGroup;
use tp_contracts::mailstore::{Delivery as MailDelivery, IngestRequest, Ingested, MailStore};
use tp_contracts::meta::{
    Account, AccountScope, AccountStore, Mailbox, MailboxStore, Policy, PolicyResolver, SpecialUse,
};
use tp_contracts::queue::{DsnRet, Outbound, SubmitStatus, Submitted};
use tp_contracts::retention::Quota;
use tp_contracts::schedule::{ItipDelivery, ItipMessage, ItipOutcome, ItipSource};
use tp_contracts::senders::{SenderLists, SenderVerdict};
use tp_contracts::sieve::{AutoReply, DeliveryRules, MailboxRef, RulesInput, RulesOutcome};
use tp_contracts::transport::{
    AcceptedDomain, BlobRef, DomainKind, EnvelopeSide, FlowDirection, FlowEnvelope, FlowInput,
    FlowOutcome, FlowVerdict, MailFlow,
};
use tp_contracts::{AccountId, Error, MailboxId, QueueId, Result, TenantId};

use crate::address::Addressing;
use crate::authcheck::AuthCheck;
use crate::config::MilterDirection;
use crate::config::{DeliveryConfig, QuotaReject, SmtpConfig};
use crate::dsn::{self, DsnSink};
use crate::forward::Forwarding;
use crate::groups::{Groups, Hold};
use crate::itip;
use crate::limits::{self, SendLimits};
use crate::milter::{self, Milters, Verdict};
use crate::routes::Routes;
use crate::server::{Envelope, Handler, Mode, Reply, is_positive, reply};
use crate::sieve::{self, Hop, Sieved, VacationRate};
use crate::sources;
use crate::srs::SrsError;
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
    /// Каталог: псевдонимы, адреса учётных записей, группы. `None` — только имена входа.
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

/// Снимок каталога: адрес → имена входа.
#[derive(Debug, Default)]
struct Snapshot {
    addresses: HashMap<String, BTreeSet<String>>,
}

/// Наибольшая глубина вложенности групп каталога.
const MAX_GROUP_DEPTH: usize = 8;

/// Получатель после переписывания и разрешения.
struct Target {
    /// Адрес после переписывания (`TenantTransport::rewrite`).
    address: String,
    resolved: Resolved,
    /// Подадрес (`user+tag`): полный адрес для заголовка `Delivered-To` копии.
    delivered_to: Option<String>,
    /// Адрес — группа рассылки [`Groups`] (ограничения отправителей, модерация).
    group: Option<DistributionGroup>,
    /// Адреса почтовых контактов в составе группы (SMTP-26): внешняя доставка.
    contacts: Vec<String>,
    /// Неизвестный адрес домена внутренней ретрансляции: в очередь (на `next_hop` домена).
    relay: bool,
    /// Принятый домен другой организации.
    foreign: bool,
}

/// Правила потока почты и хранилище объектов писем для них.
#[derive(Clone)]
struct FlowDeps {
    flow: Arc<dyn MailFlow>,
    blobs: Arc<dyn BlobStore>,
    index: Arc<dyn BlobIndex>,
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
    /// Снимки настроек транспорта: таблица переписывания адресов организации.
    transport: Option<Arc<Routes>>,
    snapshot: Mutex<Option<(Instant, Arc<Snapshot>)>>,
    groups: Option<Groups>,
    tracking: Option<Tracking>,
    forwarding: Option<Arc<Forwarding>>,
    flow: Option<FlowDeps>,
    rules: Option<Arc<dyn DeliveryRules>>,
    itip: Option<Arc<dyn ItipDelivery>>,
    /// Права отправки от чужого адреса (Send As, Send on Behalf) для приглашений из почтового клиента (SMTP-36).
    authz: Option<Arc<dyn Authorizer>>,
    senders: Option<Arc<dyn SenderLists>>,
    /// Политики организации (SMTP-40): профиль Milter, лимиты сеанса submission ([`Handler::policy`]).
    policies: Option<Arc<dyn PolicyResolver>>,
    vacation_rate: VacationRate,
    /// `delivery.redirect_hops`.
    redirect_hops: u32,
    /// `delivery.redirect_total`.
    redirect_total: u32,
    /// Лимиты отправки учётной записи в час для [`Delivery::submit`] (SMTP-43), общие с сеансами submission.
    send_limits: Option<Arc<SendLimits>>,
    /// `smtp.limits.max_tracked_accounts`.
    max_tracked: usize,
}

impl fmt::Debug for Delivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Delivery")
            .field("deps", &self.deps)
            .field("hostname", &self.hostname)
            .finish_non_exhaustive()
    }
}

/// Письмо отправки вне сессии SMTP после правил потока.
struct Flowed {
    mail_from: String,
    rcpts: Vec<String>,
    raw: Bytes,
}

/// Итог [`Delivery::pre_submit`].
enum Pre {
    /// Судьба письма решена: статус всем получателям.
    Done(SubmitStatus),
    /// Дальше — доставка и очередь.
    Go(Flowed),
}

/// Что известно о письме для выбора папки получателя: вердикт проверок и фильтров и отправитель — для личных
/// списков отправителей ящика (SMTP-25).
struct Filing {
    /// Письмо — спам: DMARC не пройден (`quarantine`, `reject`) или карантин фильтра Milter.
    junk: bool,
    /// Подлинность отправителя не подтверждена (DMARC не пройден): «надёжный» отправитель спам не отменяет —
    /// адрес мог быть подделан.
    forged: bool,
    /// Адреса отправителя для списков: `From` письма, затем отправитель конверта.
    senders: Vec<String>,
}

impl Filing {
    fn new(junk: bool, forged: bool, mail_from: &str, raw: &[u8]) -> Self {
        let mut senders = Vec::new();
        if let Some(from) = mail_parser::MessageParser::default()
            .parse_headers(raw)
            .as_ref()
            .and_then(|m| m.from()?.first()?.address())
        {
            senders.push(from.to_owned());
        }
        if !mail_from.is_empty() && !senders.iter().any(|s| s.eq_ignore_ascii_case(mail_from)) {
            senders.push(mail_from.to_owned());
        }
        Self {
            junk,
            forged,
            senders,
        }
    }
}

/// Письмо, подготовленное к доставке.
struct Prepared {
    deliveries: Vec<(String, MailDelivery)>,
    /// Учётные записи, которым письмо доставляется как спам (папка «Спам»).
    junk: HashSet<AccountId>,
    /// Внешние адреса (после переписывания).
    external: Vec<String>,
    /// Адреса для очереди без пересылки: неизвестные адреса доменов внутренней ретрансляции, исходные
    /// отправители уведомлений на адреса SRS.
    relayed: Vec<String>,
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
            transport: None,
            snapshot: Mutex::new(None),
            groups: None,
            tracking: None,
            forwarding: None,
            flow: None,
            rules: None,
            itip: None,
            authz: None,
            senders: None,
            policies: None,
            vacation_rate: VacationRate::new(cfg.delivery.vacation_rate),
            redirect_hops: cfg.delivery.redirect_hops,
            redirect_total: cfg.delivery.redirect_total,
            send_limits: None,
            max_tracked: cfg.limits.max_tracked_accounts,
        }
    }

    /// Лимиты отправки в час (SMTP-43) для [`Delivery::submit`] и отложенной отправки (JMAP): писем —
    /// `Policy::send_per_hour`, адресатов — `Policy::rcpts_per_hour` учётной записи отправителя; те же счётчики, что
    /// у сеансов submission (`SessionCtx::send_limits`). Без них отправка вне сессии не ограничена.
    pub fn with_send_limits(mut self, limits: Arc<SendLimits>) -> Self {
        self.send_limits = Some(limits);
        self
    }

    /// Личные списки надёжных и заблокированных отправителей ящиков (SMTP-25): при доставке в ящик письмо
    /// заблокированного отправителя — в «Спам», надёжного — во «Входящие» поверх вердикта фильтров.
    pub fn with_sender_lists(mut self, senders: Arc<dyn SenderLists>) -> Self {
        self.senders = Some(senders);
        self
    }

    /// Письмо — спам для ящика `scope`: вердикт письма с поправкой на списки отправителей ящика. Сверяется `From`
    /// письма, затем отправитель конверта — решает первое совпадение. «Надёжный» не отменяет спам письма с
    /// неподтверждённым отправителем ([`Filing::forged`]).
    async fn junk_for(&self, scope: AccountScope, filing: &Filing) -> Result<bool> {
        let Some(lists) = self.senders.as_ref().filter(|_| !filing.senders.is_empty()) else {
            return Ok(filing.junk);
        };
        let list = lists.sender_list(scope).await?;
        Ok(match filing.senders.iter().find_map(|a| list.verdict(a)) {
            Some(SenderVerdict::Blocked) => true,
            Some(SenderVerdict::Safe) if !filing.forged => false,
            Some(SenderVerdict::Safe) | None => filing.junk,
        })
    }

    /// Правила Sieve получателей ([`crate::sieve`], SMTP-31): для каждого локального получателя после правил потока
    /// и до `ingest` — папки и флаги доставки, отброс, перенаправление и автоответ (SMTP-32, не чаще
    /// `delivery.vacation_rate` в час на учётную запись).
    pub fn with_rules(mut self, rules: Arc<dyn DeliveryRules>) -> Self {
        self.rules = Some(rules);
        self
    }

    /// Приглашения iTIP в письмах: письмо с частью `text/calendar; method=…` после доставки в ящик передаётся `itip`
    /// для каждой учётной записи получателя, если `From` письма совпадает с `ORGANIZER` (сообщения организатора) или
    /// `ATTENDEE` (ответы участника) и ему можно доверять:
    /// - smtp-in (iMIP, RFC 6047, SMTP-33) — письмо прошло DMARC, источник `ItipSource::Imip`;
    /// - submission и [`Delivery::submit`] (решение 78, SMTP-36) — `From` принадлежит учётной записи отправителя
    ///   (вошедшей в сеанс submission; у [`Delivery::submit`] — по отправителю конверта): её адрес, псевдоним
    ///   (`Account::addresses`) или право Send As / Send on Behalf ([`Delivery::with_authz`]); источник
    ///   `ItipSource::Internal`. Иначе — только письмо.
    ///
    /// Письмо в любом случае остаётся в ящике; отказ и сбой iTIP — запись в журнал.
    pub fn with_itip(mut self, itip: Arc<dyn ItipDelivery>) -> Self {
        self.itip = Some(itip);
        self
    }

    /// Права отправки от чужого адреса (`Authorizer::send_right`) для приглашений из почтового клиента
    /// ([`Delivery::with_itip`], SMTP-36). Без них внутренним приглашением считается только письмо со своим адресом
    /// или псевдонимом в `From`.
    pub fn with_authz(mut self, authz: Arc<dyn Authorizer>) -> Self {
        self.authz = Some(authz);
        self
    }

    /// Передаёт сообщение iTIP письма `raw` учётным записям `targets` (получатель конверта и учётная запись), кроме
    /// получателей из `failed`; доверие к `From` — по `origin`.
    async fn apply_itip(
        &self,
        id: &str,
        raw: &[u8],
        origin: ItipOrigin<'_>,
        targets: &[(String, AccountId)],
        failed: &[dsn::Recipient],
    ) {
        let Some(delivery) = &self.itip else {
            return;
        };
        let msg = match itip::extract(raw) {
            None => return,
            Some(Err(reason)) => {
                tracing::info!(%id, reason, "smtp: сообщение iTIP не применено");
                return;
            }
            Some(Ok(m)) => m,
        };
        let source = match origin {
            ItipOrigin::Imip { dmarc_pass: true } => ItipSource::Imip,
            ItipOrigin::Imip { dmarc_pass: false } => {
                tracing::info!(%id, reason = "отправитель не прошёл DMARC", "smtp: сообщение iTIP не применено");
                return;
            }
            ItipOrigin::Internal { account, principal } => {
                match self.owns_from(account, principal, &msg.from).await {
                    Ok(true) => ItipSource::Internal,
                    Ok(false) => {
                        tracing::info!(%id, %account, "smtp: From приглашения не принадлежит отправителю, осталось письмом");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(%id, %account, error = %e, "smtp: сбой проверки отправителя приглашения");
                        return;
                    }
                }
            }
        };
        let mut seen = HashSet::new();
        for (rcpt, account) in targets {
            if failed.iter().any(|f| f.address.eq_ignore_ascii_case(rcpt)) || !seen.insert(*account)
            {
                continue;
            }
            let to = self
                .addressing
                .split(rcpt)
                .map_or_else(|| rcpt.clone(), |(base, _)| base)
                .to_lowercase();
            let message = ItipMessage {
                recipient: AccountScope {
                    tenant: self.deps.tenant,
                    account: *account,
                },
                to,
                from: msg.from.clone(),
                method: msg.method,
                ics: msg.ics.clone(),
                source,
            };
            match delivery.deliver(&message).await {
                Ok(ItipOutcome::Applied) => {
                    tracing::debug!(%id, %account, method = %msg.method, "smtp: сообщение iTIP применено");
                }
                Ok(ItipOutcome::Ignored(reason)) => {
                    tracing::info!(%id, %account, %reason, "smtp: сообщение iTIP не применено");
                }
                Err(e) => {
                    tracing::warn!(%id, %account, error = %e, "smtp: сбой приёма iTIP");
                }
            }
        }
    }

    /// Адрес `from` принадлежит учётной записи `account` организации узла (решение 78): её адрес или псевдоним
    /// (`Account::addresses`), иначе право отправки от него ([`Delivery::with_authz`]). Субъект проверки прав —
    /// `principal` сеанса submission; без него (отправка вне сессии SMTP) — учётная запись без групп.
    async fn owns_from(
        &self,
        account: AccountId,
        principal: Option<&Principal>,
        from: &str,
    ) -> Result<bool> {
        let tenant = self.deps.tenant;
        let Some(acc) = self.deps.accounts.get(tenant, account).await? else {
            return Ok(false);
        };
        if acc.addresses().iter().any(|a| a.eq_ignore_ascii_case(from)) {
            return Ok(true);
        }
        let Some(authz) = &self.authz else {
            return Ok(false);
        };
        let own;
        let principal = match principal {
            Some(p) => p,
            None => {
                own = Principal {
                    tenant,
                    account,
                    login: acc.login,
                    groups: BTreeSet::new(),
                    scopes: None,
                };
                &own
            }
        };
        Ok(authz.send_right(principal, from).await?.is_some())
    }

    /// Пересылка наружу: SRS-переписывание отправителя и ARC-подпись ([`crate::forward`], SMTP-23); приём
    /// уведомлений на адреса SRS.
    pub fn with_forwarding(mut self, forwarding: Arc<Forwarding>) -> Self {
        self.forwarding = Some(forwarding);
        self
    }

    /// Правила потока почты (`Registry::mail_flow`, SMTP-22); тело письма передаётся модулю объектом `blobs`
    /// (одна ссылка `index` на время вызова).
    pub fn with_mail_flow(
        mut self,
        flow: Arc<dyn MailFlow>,
        blobs: Arc<dyn BlobStore>,
        index: Arc<dyn BlobIndex>,
    ) -> Self {
        self.flow = Some(FlowDeps { flow, blobs, index });
        self
    }

    /// Принятый домен кластера по имени (снимок настроек транспорта); без снимков — `None`.
    async fn accepted(&self, domain: &str) -> Result<Option<AcceptedDomain>> {
        match &self.transport {
            None => Ok(None),
            Some(routes) => Ok(routes.cluster().await?.domain(domain).cloned()),
        }
    }

    /// Домен — локальный: принятый домен организации узла.
    async fn local_domain(&self, domain: &str) -> Result<bool> {
        let domain = domain.to_lowercase();
        Ok(self
            .accepted(&domain)
            .await?
            .is_some_and(|d| d.tenant == self.deps.tenant))
    }

    /// Адрес SRS нашего домена: `Some(Ok)` — исходный адрес, `Some(Err)` — подделан или просрочен; `None` — не SRS.
    async fn srs_reverse(
        &self,
        address: &str,
    ) -> Result<Option<std::result::Result<String, SrsError>>> {
        let Some(fwd) = &self.forwarding else {
            return Ok(None);
        };
        let Some(domain) = fwd.srs_domain() else {
            return Ok(None);
        };
        let looks = address.rsplit_once('@').is_some_and(|(l, d)| {
            d.eq_ignore_ascii_case(domain)
                && l.len() > 5
                && l.get(..4).is_some_and(|t| {
                    t.eq_ignore_ascii_case("SRS0") || t.eq_ignore_ascii_case("SRS1")
                })
        });
        if !looks {
            return Ok(None);
        }
        let Some(srs) = fwd.srs().await? else {
            return Ok(None);
        };
        Ok(Some(srs.reverse(address, OffsetDateTime::now_utc())))
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

    /// Снимки настроек транспорта `routes`: принятые домены (`ClusterTransport`) — единственный источник
    /// локальных доменов, таблица переписывания адресов конверта организации (SMTP-29,
    /// `TenantTransport::rewrites`). Снимок перечитывается после сигнала `TransportChanged` — [`Routes::watch`]
    /// запускает владелец снимков (один на узел, общий со smtp-out). Без снимков локальных доменов нет и адреса
    /// не переписываются; снимок недоступен — временная ошибка.
    pub fn with_transport(mut self, routes: Arc<Routes>) -> Self {
        self.transport = Some(routes);
        self
    }

    /// Политики организации (SMTP-40, решение 97): профиль Milter письма (`Policy::milter_profile`; submission —
    /// цепочка учётной записи отправителя, smtp-in — уровень организации) и лимиты сеанса submission
    /// ([`Handler::policy`]: `max_recipients`, `send_per_hour`, `rcpts_per_hour`). Без политик — умолчания
    /// полей `Policy`.
    pub fn with_policies(mut self, policies: Arc<dyn PolicyResolver>) -> Self {
        self.policies = Some(policies);
        self
    }

    /// Политика учётной записи `account`: цепочка «организация → основной домен → учётная запись»; учётной
    /// записи нет — уровень организации.
    async fn account_policy(&self, tenant: TenantId, account: AccountId) -> Result<Policy> {
        let Some(policies) = &self.policies else {
            return Ok(Policy::default());
        };
        match self.deps.accounts.get(tenant, account).await? {
            Some(a) => {
                policies
                    .resolve(tenant, Some(a.domain), Some(a.id), None)
                    .await
            }
            None => policies.resolve(tenant, None, None, None).await,
        }
    }

    /// Политика письма транзакции `env`: субъект AUTH — его учётная запись, иначе — организация узла.
    async fn message_policy(&self, env: &Envelope) -> Result<Policy> {
        match (&env.principal, &self.policies) {
            (_, None) => Ok(Policy::default()),
            (Some(p), Some(_)) => self.account_policy(p.tenant, p.account).await,
            (None, Some(policies)) => policies.resolve(self.deps.tenant, None, None, None).await,
        }
    }

    /// Адрес конверта `address` организации `tenant` после переписывания для стороны `side`; `None` — правила нет.
    async fn rewrite(
        &self,
        tenant: TenantId,
        address: &str,
        side: EnvelopeSide,
    ) -> Result<Option<String>> {
        match &self.transport {
            None => Ok(None),
            Some(routes) => Ok(routes.tenant(tenant).await?.rewrite(address, side)),
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
            .rewrite(self.deps.tenant, address, EnvelopeSide::Recipient)
            .await?
            .unwrap_or_else(|| address.to_owned());
        let lower = address.to_lowercase();
        let target = |resolved, delivered_to| Target {
            address: address.clone(),
            resolved,
            delivered_to,
            group: None,
            contacts: Vec::new(),
            relay: false,
            foreign: false,
        };
        let Some((_, domain)) = lower.rsplit_once('@') else {
            return Ok(target(Resolved::Unknown, None));
        };
        let Some(accepted) = self.accepted(domain).await? else {
            return Ok(target(Resolved::External, None));
        };
        if accepted.tenant != self.deps.tenant {
            return Ok(Target {
                foreign: true,
                ..target(Resolved::External, None)
            });
        }
        let relay = matches!(accepted.kind, DomainKind::InternalRelay { .. });
        let snap = self.snapshot().await?;
        if let Some(groups) = &self.groups
            && let Some(group) = groups.by_address(self.deps.tenant, &lower).await?
        {
            let members = groups.members(self.deps.tenant, &group).await?;
            let mut accounts = Vec::new();
            for id in members.accounts {
                if let Some(acc) = self.deps.accounts.get(self.deps.tenant, id).await?
                    && acc.enabled
                {
                    accounts.push(acc);
                }
            }
            return Ok(Target {
                group: Some(group),
                contacts: members.contacts,
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
        if relay {
            // Адрес вне каталога в домене внутренней ретрансляции — на старый сервер (`next_hop`).
            return Ok(Target {
                relay: true,
                ..target(Resolved::External, None)
            });
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
        let Some(dir) = &self.deps.directory else {
            return Ok(Snapshot::default());
        };
        let tenant = self.deps.tenant;
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
        Ok(Snapshot { addresses })
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
        filing: &Filing,
        origin: Origin,
    ) -> Result<Prepared> {
        let mut out = Prepared {
            deliveries: Vec::new(),
            junk: HashSet::new(),
            external: Vec::new(),
            relayed: Vec::new(),
            queued: Vec::new(),
            held: Vec::new(),
            failed: Vec::new(),
        };
        for rcpt in rcpts {
            match self.srs_reverse(rcpt).await? {
                Some(Ok(original)) => {
                    // Уведомление на адрес SRS — исходному отправителю.
                    push_unique(&mut out.relayed, vec![original]);
                    out.queued.push(rcpt.clone());
                    continue;
                }
                Some(Err(e)) => {
                    tracing::info!(%id, error = %e, "smtp: адрес SRS отклонён");
                    out.failed.push(dsn::Recipient::new(
                        rcpt.clone(),
                        dsn::Action::Failed,
                        BAD_SRS,
                    ));
                    continue;
                }
                None => {}
            }
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
            // Контакты в составе группы — внешним адресатам (очередь; письмо извне — пересылкой).
            push_unique(&mut out.external, target.contacts);
            match target.resolved {
                Resolved::Local(accounts) => {
                    for acc in accounts {
                        self.push_delivery(
                            &mut out,
                            rcpt,
                            &acc,
                            target.delivered_to.as_deref(),
                            filing,
                        )
                        .await?;
                    }
                }
                Resolved::External => {
                    let list = if target.relay {
                        &mut out.relayed
                    } else {
                        &mut out.external
                    };
                    push_unique(list, vec![target.address]);
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

    /// Добавляет доставку письма получателя `rcpt` в ящик учётной записи `acc` (одна на учётную запись): папка —
    /// «Входящие» или «Спам» по [`Delivery::junk_for`]; `delivered_to` — полный адрес с подадресом.
    async fn push_delivery(
        &self,
        out: &mut Prepared,
        rcpt: &str,
        acc: &Account,
        delivered_to: Option<&str>,
        filing: &Filing,
    ) -> Result<()> {
        if out.deliveries.iter().any(|(_, d)| d.account == acc.id) {
            return Ok(());
        }
        let scope = AccountScope {
            tenant: acc.tenant,
            account: acc.id,
        };
        let junk = self.junk_for(scope, filing).await?;
        if junk {
            out.junk.insert(acc.id);
        }
        let mailbox = self.mailbox(scope, junk).await?;
        out.deliveries.push((
            rcpt.to_owned(),
            MailDelivery {
                account: acc.id,
                mailbox,
                extra_headers: delivered_to
                    .iter()
                    .map(|a| ("Delivered-To".to_owned(), clean(a)))
                    .collect(),
                flags: Default::default(),
            },
        ));
        Ok(())
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
        filing: &Filing,
    ) -> Result<()> {
        if addresses.is_empty() {
            return Ok(());
        }
        let mut copies = self.prepare(id, addresses, filing, Origin::Module).await?;
        copies.external.append(&mut copies.relayed);
        for (_, d) in copies.deliveries {
            if !prepared
                .deliveries
                .iter()
                .any(|(_, x)| x.account == d.account)
            {
                if copies.junk.contains(&d.account) {
                    prepared.junk.insert(d.account);
                }
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
        filing: &Filing,
        received_at: OffsetDateTime,
        mut delivered: HashSet<AccountId>,
    ) -> Result<Vec<String>> {
        let mut external = Vec::new();
        while !forward.is_empty() {
            let p = self.prepare(id, &forward, filing, Origin::Module).await?;
            push_unique(&mut external, p.external);
            push_unique(&mut external, p.relayed);
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

    /// Пересылка наружу (SMTP-23): отправитель конверта — SRS (кроме локальных отправителей и уведомлений),
    /// письмо — с ARC-подписью узла; `ip` — адрес клиента, от которого письмо принято.
    async fn queue_forward(
        &self,
        id: &str,
        mail_from: &str,
        rcpts: Vec<String>,
        mut raw: Bytes,
        ip: Option<std::net::IpAddr>,
    ) -> Result<()> {
        if rcpts.is_empty() {
            return Ok(());
        }
        let Some(fwd) = &self.forwarding else {
            return self.queue_out(id, mail_from, rcpts, raw).await;
        };
        let mut sender = mail_from.to_owned();
        if let Some((_, domain)) = mail_from.rsplit_once('@')
            && !self.local_domain(domain).await?
            && let Some(srs) = fwd.srs().await?
            && let Some(s) = srs.forward(mail_from, OffsetDateTime::now_utc())
        {
            sender = s;
        }
        if let (Some(sealer), Some(ac)) = (fwd.sealer().await?, &self.deps.authcheck)
            && let Some(set) = ac.seal(&sealer, &raw, ip).await
        {
            let mut m = Vec::with_capacity(set.len() + raw.len());
            m.extend_from_slice(set.as_bytes());
            m.extend_from_slice(&raw);
            raw = Bytes::from(m);
        }
        self.queue_out(id, &sender, rcpts, raw).await
    }

    /// Правила потока почты (SMTP-22): тело — объектом BLOB на время вызова; изменённое тело — прочитано, ссылка на
    /// объект модуля снята. Без модуля — `None`.
    async fn mail_flow(
        &self,
        mut input: FlowInput,
        raw: &Bytes,
    ) -> Result<Option<(FlowOutcome, Option<Bytes>)>> {
        let Some(f) = &self.flow else {
            return Ok(None);
        };
        let key = crate::queue::store_blob(
            f.blobs.as_ref(),
            f.index.as_ref(),
            input.tenant,
            raw.clone(),
        )
        .await?;
        input.message = BlobRef {
            key,
            size: raw.len() as u64,
        };
        let outcome = f.flow.apply(&input).await;
        crate::queue::release_blob(f.index.as_ref(), &key).await;
        let outcome = outcome?;
        let body = match &outcome.message {
            Some(m) if m.key != key => {
                let read = read_blob(f.blobs.as_ref(), &m.key).await;
                crate::queue::release_blob(f.index.as_ref(), &m.key).await;
                Some(read?)
            }
            _ => None,
        };
        if let Some(route) = &outcome.route {
            // Очередь маршрут правила не хранит (CONTRACTS.md#Queue): политика — по домену назначения.
            tracing::info!(%route, "smtp: маршрут правила потока не применён, политика — по домену назначения");
        }
        Ok(Some((outcome, body)))
    }

    /// Принимает письмо: проверки, фильтры, доставка и очередь. Ответ фильтра (отказ, отброс) — `Ok(Some)`.
    async fn accept(&self, env: &Envelope, raw: Bytes, trail: &mut Trail) -> Result<Option<Reply>> {
        let received_at = OffsetDateTime::now_utc();
        trail.message_id = tracking::message_id(&raw);
        let app = sources::application(env.source.as_deref()).is_some();
        let check = match (&self.deps.authcheck, env.mode) {
            // Приложение организации: проверки отправителя не нужны.
            (Some(_), Mode::Inbound) if app => None,
            (Some(ac), Mode::Inbound) => {
                // Шлюз без данных об исходном клиенте — результаты его проверок.
                let gateway = match sources::gateway_authserv(env.source.as_deref()) {
                    Some(id) if env.gateway.is_none() => ac.gateway(&raw, id).await,
                    _ => None,
                };
                Some(match gateway {
                    Some(g) => g,
                    None => ac.check(env, &raw).await,
                })
            }
            _ => None,
        };
        // Отправитель конверта переписывается после проверок (SPF — по исходному).
        let rewritten = self
            .rewrite(self.deps.tenant, &env.mail_from, EnvelopeSide::Sender)
            .await?
            .map(|mail_from| Envelope {
                mail_from,
                ..env.clone()
            });
        let env = rewritten.as_ref().unwrap_or(env);
        // DMARC не пройден при политике quarantine/reject — в «Спам».
        let forged = check.as_ref().is_some_and(|c| c.junk);
        let mut junk = forged;
        let mut headers = self.trace_headers(env, received_at);
        if let Some(c) = &check {
            // Authentication-Results и Received-SPF — выше Received этого узла (RFC 8601, 7208).
            headers.insert_str(0, &c.headers);
        }
        let mut full = Vec::with_capacity(headers.len() + raw.len());
        full.extend_from_slice(headers.as_bytes());
        full.extend_from_slice(&raw);
        let mut full = Bytes::from(full);
        let mail = milter::Mail::of(env);
        if let Some(m) = self
            .deps
            .milter
            .as_ref()
            .filter(|m| m.applies(mail.direction))
        {
            let profile = self.message_policy(env).await?.milter_profile;
            let out = m.check(&mail, full, profile.as_deref()).await;
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
        // Правила потока — после Milter, до точек принятия решений, доставки и очереди; один раз на письмо.
        let input = FlowInput {
            tenant: self.deps.tenant,
            direction: match env.mode {
                Mode::Inbound => FlowDirection::Inbound,
                Mode::Submission => FlowDirection::Submission,
            },
            principal: env.principal.clone(),
            source: env.source.as_ref().map(|s| s.name.clone()),
            envelope: FlowEnvelope {
                mail_from: env.mail_from.clone(),
                rcpt_to: env.rcpts.clone(),
            },
            message: placeholder_blob(self.deps.tenant),
        };
        let flowed;
        let mut env = env;
        if let Some((outcome, body)) = self.mail_flow(input, &full).await? {
            if !outcome.matched.is_empty() {
                trail.steps.push(
                    Step::new(Ek::Receive, Dd::Accept, env.rcpts.clone())
                        .reason(format!("mail flow rules: {}", outcome.matched.join(", "))),
                );
            }
            match outcome.verdict {
                FlowVerdict::Accept if !outcome.envelope.rcpt_to.is_empty() => {}
                FlowVerdict::Accept | FlowVerdict::Drop => {
                    tracing::info!(id = %env.id, "smtp: письмо удалено правилом потока");
                    trail.decision = Some(Dd::Discard);
                    return Ok(Some(reply(250, [2, 0, 0], format!("OK id={}", env.id))));
                }
                FlowVerdict::Reject(text) => {
                    tracing::info!(id = %env.id, "smtp: письмо отклонено правилом потока");
                    let text = clean(&text);
                    let text = if text.is_empty() {
                        "Message rejected by mail flow rule".to_owned()
                    } else {
                        text
                    };
                    return Ok(Some(reply(550, [5, 7, 1], text)));
                }
                FlowVerdict::Moderate { .. } => {
                    // Очередь модерации (GroupStore::hold) — только для групп рассылки: письмо правила потока
                    // в неё не ставится (нужен контракт); временный отказ — письмо не теряется.
                    tracing::warn!(id = %env.id, "smtp: модерация правилом потока не поддержана");
                    return Ok(Some(reply(
                        451,
                        [4, 7, 0],
                        "Moderation by mail flow rule is not available, try again later",
                    )));
                }
            }
            if let Some(body) = body {
                full = body;
            }
            if outcome.envelope.mail_from != env.mail_from || outcome.envelope.rcpt_to != env.rcpts
            {
                flowed = Envelope {
                    mail_from: outcome.envelope.mail_from,
                    rcpts: outcome.envelope.rcpt_to,
                    ..env.clone()
                };
                env = &flowed;
            }
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
        let filing = Filing::new(junk, forged, &env.mail_from, &full);
        let filing = &filing;
        let mut prepared = self
            .prepare(&env.id, &env.rcpts, filing, Origin::Sender(sender))
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
        self.add_copies(&env.id, &mut prepared, &copy_to, filing)
            .await?;
        // Внешние получатели smtp-in от обычного клиента — только перенаправления правил потока: пересылка.
        let redirect = env.mode == Mode::Inbound && !app;
        let mut forwarded = Vec::new();
        let mut rcpts = prepared.relayed;
        if redirect {
            forwarded = prepared.external;
        } else {
            push_unique(&mut rcpts, prepared.external);
        }
        let mut failed = prepared.failed;
        // Учётные записи получателей — до правил: пересылка модулей им же письмо не доставляет (петли).
        let delivered = accounts_of(&prepared.deliveries);
        let mut forward = Vec::new();
        let Sieved {
            redirect: hops,
            mut replies,
            steps: mut sieve_steps,
        } = self
            .apply_rules(
                &env.id,
                &env.mail_from,
                &full,
                &mut prepared.deliveries,
                &prepared.junk,
                false,
            )
            .await?;
        let local = local_rcpts(&prepared.deliveries);
        let spam = junk_rcpts(&prepared.deliveries, &prepared.junk);
        // iTIP — входящей почте с проверками отправителя (iMIP) и письму вошедшего пользователя (решение 78), вне
        // «Спама»: адресу `From` без DMARC или без связи с отправителем доверять нельзя.
        let origin = match (&check, sender) {
            (Some(c), _) => Some(ItipOrigin::Imip {
                dmarc_pass: c.dmarc_pass,
            }),
            (None, Some(account)) => Some(ItipOrigin::Internal {
                account,
                principal: env.principal.as_ref(),
            }),
            (None, None) => None,
        }
        .filter(|_| self.itip.is_some());
        let itip_to = itip_targets(origin.is_some(), &prepared.deliveries, &prepared.junk);
        if !prepared.deliveries.is_empty() {
            let ingested = self
                .ingest_all(
                    &env.id,
                    &env.mail_from,
                    full.clone(),
                    prepared.deliveries,
                    received_at,
                    &mut failed,
                )
                .await?;
            push_unique(&mut forward, ingested.forward);
        }
        if let Some(origin) = origin.filter(|_| !itip_to.is_empty()) {
            self.apply_itip(&env.id, &full, origin, &itip_to, &failed)
                .await;
        }
        let external = self
            .forward(
                &env.id,
                &env.mail_from,
                &full,
                forward,
                filing,
                received_at,
                delivered,
            )
            .await?;
        push_unique(&mut forwarded, external);
        let relayed = self
            .relay(
                &env.id,
                &env.mail_from,
                hops,
                filing,
                received_at,
                Some(env.conn.client.ip()),
            )
            .await?;
        sieve::merge_replies(&mut replies, relayed.replies);
        sieve_steps.extend(relayed.steps);
        self.queue_out(&env.id, &env.mail_from, rcpts, full.clone())
            .await?;
        self.queue_forward(
            &env.id,
            &env.mail_from,
            forwarded,
            full.clone(),
            Some(env.conn.client.ip()),
        )
        .await?;
        outcome_steps(&mut trail.steps, &prepared.held, &local, &failed, &spam);
        trail.steps.extend(sieve_steps);
        self.send_replies(&env.id, &env.mail_from, &full, replies, &mut trail.steps)
            .await;
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

    /// Обработка письма отправки вне сессии SMTP до доставки и очереди: фильтры Milter исходящей почты
    /// (SMTP-27) из профиля политики организации (SMTP-40), правила потока (один раз на письмо) и переписывание
    /// отправителя. `Done` — судьба письма решена фильтром или правилом (отказ, удаление).
    async fn pre_submit(
        &self,
        id: &str,
        tenant: TenantId,
        mail_from: &str,
        rcpts: &[String],
        mut raw: Bytes,
    ) -> Result<Pre> {
        let outbound = MilterDirection::Outbound;
        if let Some(m) = self.deps.milter.as_ref().filter(|m| m.applies(outbound)) {
            let profile = match &self.policies {
                Some(p) => p.resolve(tenant, None, None, None).await?.milter_profile,
                None => None,
            };
            let mail = milter::Mail {
                direction: outbound,
                id,
                client: None,
                login: None,
                mail_from,
                rcpts,
            };
            let out = m.check(&mail, raw, profile.as_deref()).await;
            let refused = match out.verdict {
                Verdict::Accept => out.quarantine.map(|reason| {
                    tracing::info!(%id, %reason, "smtp: письмо в карантине фильтра");
                    reply(550, [5, 7, 1], "Message held by content filter")
                }),
                Verdict::Refuse(r) => Some(r),
                Verdict::Discard => {
                    let step = Step::new(Ek::Receive, Dd::Discard, rcpts.to_vec());
                    self.track(
                        mail_from,
                        tracking::message_id(&out.message).as_deref(),
                        vec![step],
                    )
                    .await;
                    return Ok(Pre::Done(SubmitStatus::Delivered));
                }
            };
            if let Some(r) = refused {
                let step = Step::new(Ek::Reject, tracking::decision(&r), rcpts.to_vec())
                    .reason(tracking::reason(&r));
                self.track(
                    mail_from,
                    tracking::message_id(&out.message).as_deref(),
                    vec![step],
                )
                .await;
                return Ok(Pre::Done(SubmitStatus::Failed(tracking::reason(&r))));
            }
            raw = out.message;
        }
        let mut flowed_rcpts = None;
        let input = FlowInput {
            tenant,
            direction: FlowDirection::Submission,
            principal: None,
            source: None,
            envelope: FlowEnvelope {
                mail_from: mail_from.to_owned(),
                rcpt_to: rcpts.to_vec(),
            },
            message: placeholder_blob(tenant),
        };
        let mut flowed_from = None;
        if let Some((outcome, body)) = self.mail_flow(input, &raw).await? {
            let status = match outcome.verdict {
                FlowVerdict::Accept if !outcome.envelope.rcpt_to.is_empty() => None,
                FlowVerdict::Accept | FlowVerdict::Drop => Some(SubmitStatus::Delivered),
                FlowVerdict::Reject(text) => {
                    Some(SubmitStatus::Failed(format!("550 5.7.1 {}", clean(&text))))
                }
                FlowVerdict::Moderate { .. } => Some(SubmitStatus::Failed(
                    "451 4.7.0 moderation by mail flow rule is not available".into(),
                )),
            };
            if !outcome.matched.is_empty() {
                let step = Step::new(Ek::Receive, Dd::Accept, rcpts.to_vec())
                    .reason(format!("mail flow rules: {}", outcome.matched.join(", ")));
                self.track(mail_from, tracking::message_id(&raw).as_deref(), vec![step])
                    .await;
            }
            if let Some(status) = status {
                return Ok(Pre::Done(status));
            }
            if let Some(body) = body {
                raw = body;
            }
            flowed_from = Some(outcome.envelope.mail_from);
            flowed_rcpts = Some(outcome.envelope.rcpt_to);
        }
        let rcpts = flowed_rcpts.as_deref().unwrap_or(rcpts);
        let mail_from = flowed_from.as_deref().unwrap_or(mail_from);
        let rewritten = self
            .rewrite(tenant, mail_from, EnvelopeSide::Sender)
            .await?;
        let mail_from = rewritten.as_deref().unwrap_or(mail_from);
        Ok(Pre::Go(Flowed {
            mail_from: mail_from.to_owned(),
            rcpts: rcpts.to_vec(),
            raw,
        }))
    }

    /// Доставка письма отправки после [`Delivery::pre_submit`]: локальным получателям — в ящики (группы, правила
    /// Sieve, пересылки), внешним — в очередь. Статус — по каждому адресу `rcpts`. `bounce` — уведомить отправителя
    /// о недоставленных; выпуск отложенной отправки ([`LocalRelease`]) уведомляет сам, по итогам записи очереди.
    async fn dispatch(
        &self,
        id: &str,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
        bounce: bool,
    ) -> Result<Vec<(String, SubmitStatus)>> {
        let sender = self.sender_by_address(mail_from).await?;
        let filing = Filing::new(false, false, mail_from, &raw);
        let filing = &filing;
        let mut prepared = self
            .prepare(id, rcpts, filing, Origin::Sender(sender))
            .await?;
        let received_at = OffsetDateTime::now_utc();
        self.hold_all(&prepared.held, mail_from, sender, &raw, received_at)
            .await?;
        let queue = self.deps.outbound.is_some();
        if !queue && !prepared.external.is_empty() {
            no_outbound(id, prepared.external.len());
        }
        let mut failed = prepared.failed;
        // Письмо локальным получателям — с заголовками трассировки; правила и пересылки — по нему.
        let full = if prepared.deliveries.is_empty() {
            raw.clone()
        } else {
            let mut full = self
                .submit_headers(id, mail_from, rcpts, received_at)
                .into_bytes();
            full.extend_from_slice(&raw);
            Bytes::from(full)
        };
        let delivered = accounts_of(&prepared.deliveries);
        let mut forward = Vec::new();
        let Sieved {
            redirect: hops,
            mut replies,
            steps: mut sieve_steps,
        } = self
            .apply_rules(
                id,
                mail_from,
                &full,
                &mut prepared.deliveries,
                &prepared.junk,
                false,
            )
            .await?;
        let local_list = local_rcpts(&prepared.deliveries);
        let spam = junk_rcpts(&prepared.deliveries, &prepared.junk);
        // Приглашение из почтового клиента (решение 78) — от учётной записи отправителя конверта.
        let itip_to = itip_targets(
            sender.is_some() && self.itip.is_some(),
            &prepared.deliveries,
            &prepared.junk,
        );
        if !prepared.deliveries.is_empty() {
            let local: Vec<String> = prepared.deliveries.iter().map(|(r, _)| r.clone()).collect();
            match self
                .ingest_all(
                    id,
                    mail_from,
                    full.clone(),
                    prepared.deliveries,
                    received_at,
                    &mut failed,
                )
                .await
            {
                Ok(ingested) => push_unique(&mut forward, ingested.forward),
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
        if let Some(account) = sender.filter(|_| !itip_to.is_empty()) {
            let origin = ItipOrigin::Internal {
                account,
                principal: None,
            };
            self.apply_itip(id, &full, origin, &itip_to, &failed).await;
        }
        let forward = self
            .forward(
                id,
                mail_from,
                &full,
                forward,
                filing,
                received_at,
                delivered,
            )
            .await?;
        let relayed = self
            .relay(id, mail_from, hops, filing, received_at, None)
            .await?;
        sieve::merge_replies(&mut replies, relayed.replies);
        sieve_steps.extend(relayed.steps);
        let statuses: Vec<(String, SubmitStatus)> = rcpts
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
        if queue {
            push_unique(&mut out, prepared.relayed);
        }
        self.queue_out(id, mail_from, out, raw.clone()).await?;
        self.queue_forward(id, mail_from, forward, raw.clone(), None)
            .await?;
        let mut steps = vec![Step::new(Ek::Receive, Dd::Accept, rcpts.to_vec())];
        outcome_steps(&mut steps, &prepared.held, &local_list, &failed, &spam);
        steps.extend(sieve_steps);
        self.send_replies(id, mail_from, &full, replies, &mut steps)
            .await;
        if bounce && !failed.is_empty() && !mail_from.is_empty() {
            let sent = self.bounce(id, mail_from, &failed, &raw, received_at).await;
            steps.push(dsn_step(mail_from, sent));
        }
        self.track(mail_from, tracking::message_id(&raw).as_deref(), steps)
            .await;
        tracing::info!(%id, count = rcpts.len(), "smtp: отправка вне сессии обработана");
        Ok(statuses)
    }

    /// Отправка от имени пользователя вне сессии SMTP (JMAP `EmailSubmission/set`, CMP-04): получатели в
    /// принятых доменах организации (адреса, псевдонимы и группы каталога)
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
    ///
    /// Лимиты отправки ([`Delivery::with_send_limits`], SMTP-43): письмо учётной записи, превысившее за скользящий
    /// час `send_per_hour` писем или `rcpts_per_hour` адресатов, не отправляется — все получатели `Failed` с
    /// `451 4.7.1`, событие отказа — в журнал доставки.
    pub async fn submit(
        &self,
        tenant: TenantId,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<Submitted> {
        if let Some(line) = self.over_send_limit(tenant, mail_from, rcpts, &raw).await? {
            return Ok(Submitted::all(rcpts, SubmitStatus::Failed(line)));
        }
        let id = crate::server::next_id();
        let requested = rcpts;
        let Flowed {
            mail_from,
            rcpts,
            raw,
        } = match self.pre_submit(&id, tenant, mail_from, rcpts, raw).await? {
            Pre::Done(status) => return Ok(Submitted::all(requested, status)),
            Pre::Go(m) => m,
        };
        let (mail_from, rcpts) = (mail_from.as_str(), rcpts.as_slice());
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
            return Ok(Submitted::all(requested, status));
        }
        let statuses = self.dispatch(&id, mail_from, rcpts, raw, true).await?;
        // Статус — по получателям запроса: адрес, убранный правилом потока, считается обработанным.
        let statuses = requested
            .iter()
            .map(|r| {
                let status = statuses
                    .iter()
                    .find(|(x, _): &&(String, SubmitStatus)| x.eq_ignore_ascii_case(r))
                    .map_or(SubmitStatus::Delivered, |(_, s)| s.clone());
                (r.clone(), status)
            })
            .collect();
        Ok(Submitted { rcpts: statuses })
    }

    /// Лимиты отправки в час вне сессии SMTP ([`Delivery::with_send_limits`], SMTP-43) — для [`Delivery::submit`] и
    /// [`Delivery::schedule`]: отправитель `mail_from` — учётная запись организации узла, письмо — `send_per_hour`
    /// писем и `rcpts_per_hour` адресатов её политики за скользящий час. Допущенное письмо учитывается сразу (до
    /// отправки или постановки). Превышен предел — ответ `451 4.7.1` и событие отказа в журнале доставки. Без
    /// лимитов, для другой организации и внешнего отправителя — без ограничения.
    async fn over_send_limit(
        &self,
        tenant: TenantId,
        mail_from: &str,
        rcpts: &[String],
        raw: &Bytes,
    ) -> Result<Option<String>> {
        let Some(l) = &self.send_limits else {
            return Ok(None);
        };
        if tenant != self.deps.tenant {
            return Ok(None);
        }
        let Some(account) = self.sender_by_address(mail_from).await? else {
            return Ok(None);
        };
        let policy = self.account_policy(tenant, account).await?;
        let key = account.to_string();
        let now = Instant::now();
        let exceeded = if !l.allow_message_at(policy.send_per_hour_or_default(), &key, now) {
            limits::MESSAGES_EXCEEDED
        } else if !l.allow_rcpts_at(policy.rcpts_per_hour_or_default(), &key, rcpts.len(), now) {
            limits::RCPTS_EXCEEDED
        } else {
            l.record_at(self.max_tracked, &key, rcpts.len(), now);
            return Ok(None);
        };
        let line = format!("451 4.7.1 {exceeded}");
        let step = Step::new(Ek::Reject, Dd::Defer, rcpts.to_vec()).reason(line.clone());
        self.track(mail_from, tracking::message_id(raw).as_deref(), vec![step])
            .await;
        tracing::info!(
            rcpts = rcpts.len(),
            "smtp: отправка вне сессии отклонена лимитом в час"
        );
        Ok(Some(line))
    }

    /// Правила Sieve получателей письма `raw` (SMTP-31): доставка с адресом получателя заменяется доставками в папки
    /// правил (несколько папок — несколько доставок одного `ingest`; `discard` и `redirect` без копии — ни одной);
    /// копии модулей (без адреса) — без правил. Сбой правил — доставка в папку по умолчанию («Входящие», при
    /// `junk` — «Спам»). Возвращает перенаправления, автоответы (не для писем в «Спам») и события журнала.
    ///
    /// Петля перенаправлений (SMTP-34, [`crate::sieve`]): ящик уже есть в цепочке письма, цепочка длиннее
    /// `delivery.redirect_hops` или исчерпан `delivery.redirect_total` (`over`) — правила ящика не применяются,
    /// письмо — в папку по умолчанию, запись в журнал доставки.
    async fn apply_rules(
        &self,
        id: &str,
        mail_from: &str,
        raw: &Bytes,
        deliveries: &mut Vec<(String, MailDelivery)>,
        junk: &HashSet<AccountId>,
        over: bool,
    ) -> Result<Sieved> {
        let mut out = Sieved::default();
        let Some(rules) = &self.rules else {
            return Ok(out);
        };
        let chain = sieve::chain(raw);
        let mut result = Vec::with_capacity(deliveries.len());
        for (rcpt, d) in std::mem::take(deliveries) {
            if rcpt.is_empty() {
                result.push((rcpt, d));
                continue;
            }
            let token = sieve::chain_token(d.account);
            let looped = if chain.contains(&token) {
                Some("mailbox already in redirect chain")
            } else if chain.len() > self.redirect_hops as usize {
                Some("redirect hop limit")
            } else if over {
                Some("redirect limit per message")
            } else {
                None
            };
            if let Some(why) = looped {
                tracing::info!(%id, account = %d.account, reason = why, "smtp: петля перенаправлений, правила Sieve не применены");
                out.steps.push(
                    Step::new(Ek::Deliver, Dd::Accept, vec![rcpt.clone()])
                        .reason(format!("sieve: rules skipped, {why}; default folder")),
                );
                result.push((rcpt, d));
                continue;
            }
            let scope = AccountScope {
                tenant: self.deps.tenant,
                account: d.account,
            };
            let input = RulesInput {
                scope,
                recipient: rcpt.clone(),
                mail_from: mail_from.to_owned(),
                message: raw.clone(),
                mailboxes: self.deps.mailboxes.list(scope).await?,
            };
            let mut notes = Vec::new();
            let outcome = match rules.evaluate(&input).await {
                Ok(o) => o,
                Err(e) => {
                    tracing::warn!(%id, account = %d.account, error = %e, "smtp: правила Sieve не выполнены, доставка во «Входящие»");
                    notes.push(format!("rules failed ({e}), default folder"));
                    RulesOutcome::implicit_keep()
                }
            };
            if !outcome.errors.is_empty() {
                tracing::info!(%id, account = %d.account, count = outcome.errors.len(), "smtp: ошибки выполнения правил Sieve");
                notes.push(format!("errors: {}", outcome.errors.join("; ")));
            }
            let mut kept: Vec<MailDelivery> = Vec::new();
            for keep in &outcome.keep {
                let mailbox = self
                    .keep_mailbox(id, scope, &input.mailboxes, &keep.mailbox, d.mailbox)
                    .await;
                match &keep.mailbox {
                    MailboxRef::Inbox => {}
                    MailboxRef::Id(m) => notes.push(format!("fileinto mailbox {m}")),
                    MailboxRef::Name { name, .. } => {
                        notes.push(format!("fileinto {}", clean(name)))
                    }
                }
                if let Some(k) = kept.iter_mut().find(|k| k.mailbox == mailbox) {
                    k.flags.extend(keep.flags.iter().cloned());
                    continue;
                }
                kept.push(MailDelivery {
                    account: d.account,
                    mailbox,
                    extra_headers: d.extra_headers.clone(),
                    flags: keep.flags.clone(),
                });
            }
            if outcome.discard {
                notes.push("discard".into());
            }
            let redirected = !outcome.redirect.is_empty();
            if redirected {
                notes.push(format!("redirect {}", outcome.redirect.join(", ")));
                out.redirect.push(Hop {
                    raw: sieve::stamp(raw, &token),
                    to: outcome.redirect,
                });
            }
            match outcome.vacation {
                Some(_) if junk.contains(&d.account) => {
                    notes.push("auto-reply skipped: junk".into())
                }
                Some(reply) => out.replies.push((scope, reply)),
                None => {}
            }
            if kept.is_empty() && notes.is_empty() {
                notes.push("no delivery".into());
            }
            if !notes.is_empty() {
                let decision = if kept.is_empty() && !redirected {
                    Dd::Discard
                } else {
                    Dd::Accept
                };
                out.steps.push(
                    Step::new(Ek::Deliver, decision, vec![rcpt.clone()])
                        .reason(format!("sieve: {}", notes.join("; "))),
                );
            }
            result.extend(kept.into_iter().map(|k| (rcpt.clone(), k)));
        }
        *deliveries = result;
        Ok(out)
    }

    /// Перенаправления правил и внутренние автоответы (SMTP-34, решение 61): письмо каждого перенаправления
    /// доставляется как любое письмо — локальным адресатам через их правила (их перенаправления — так же, пока
    /// цепочка не оборвётся: [`Delivery::apply_rules`]), внешним — в очередь с SRS и ARC. Неизвестный адрес и
    /// переполненный ящик — только в журнал. Возвращает автоответы и события журнала доставки.
    async fn relay(
        &self,
        id: &str,
        mail_from: &str,
        hops: Vec<Hop>,
        filing: &Filing,
        received_at: OffsetDateTime,
        ip: Option<std::net::IpAddr>,
    ) -> Result<Sieved> {
        let mut out = Sieved::default();
        let mut hops = VecDeque::from(hops);
        let mut done = 0u32;
        while let Some(hop) = hops.pop_front() {
            let over = done >= self.redirect_total;
            done = done.saturating_add(1);
            let mut p = self.prepare(id, &hop.to, filing, Origin::Module).await?;
            let mut external = std::mem::take(&mut p.external);
            push_unique(&mut external, std::mem::take(&mut p.relayed));
            let delivered = accounts_of(&p.deliveries);
            let sieved = self
                .apply_rules(id, mail_from, &hop.raw, &mut p.deliveries, &p.junk, over)
                .await?;
            if !p.deliveries.is_empty() {
                let mut ignored = Vec::new();
                match self
                    .ingest_all(
                        id,
                        mail_from,
                        hop.raw.clone(),
                        p.deliveries,
                        received_at,
                        &mut ignored,
                    )
                    .await
                {
                    Ok(ingested) => {
                        let more = self
                            .forward(
                                id,
                                mail_from,
                                &hop.raw,
                                ingested.forward,
                                filing,
                                received_at,
                                delivered,
                            )
                            .await?;
                        push_unique(&mut external, more);
                    }
                    Err(Error::QuotaExceeded) => {
                        tracing::warn!(%id, "smtp: перенаправление не доставлено, ящики переполнены");
                    }
                    Err(e) => return Err(e),
                }
            }
            self.queue_forward(id, mail_from, external, hop.raw, ip)
                .await?;
            hops.extend(sieved.redirect);
            sieve::merge_replies(&mut out.replies, sieved.replies);
            out.steps.extend(sieved.steps);
        }
        Ok(out)
    }

    /// Папка доставки правила: `Inbox` — папка по умолчанию `default` («Входящие» или «Спам»); неизвестная папка без
    /// `:create`, чужая папка и сбой создания — тоже она.
    async fn keep_mailbox(
        &self,
        id: &str,
        scope: AccountScope,
        list: &[Mailbox],
        target: &MailboxRef,
        default: MailboxId,
    ) -> MailboxId {
        match target {
            MailboxRef::Inbox => default,
            MailboxRef::Id(m) if list.iter().any(|x| x.id == *m) => *m,
            MailboxRef::Id(_) => default,
            MailboxRef::Name { name, create } => {
                if let Some(m) = sieve::find_path(list, name) {
                    return m;
                }
                if !*create {
                    return default;
                }
                match sieve::create_path(self.deps.mailboxes.as_ref(), scope, list, name).await {
                    Ok(Some(m)) => m,
                    Ok(None) => default,
                    Err(e) => {
                        tracing::warn!(%id, error = %e, "smtp: папка правила Sieve не создана, доставка в папку по умолчанию");
                        default
                    }
                }
            }
        }
    }

    /// Автоответы правил на письмо `raw` от `mail_from` (SMTP-32): защита от петель RFC 3834
    /// ([`sieve::suppressed`]) и частота `delivery.vacation_rate`, затем письмо [`sieve::build_reply`] с конвертом
    /// `<>`: внешнему адресату — в очередь, локальному — через его правила ([`Delivery::relay`]). Сбой — только в
    /// журналы: исходное письмо уже доставлено.
    async fn send_replies(
        &self,
        id: &str,
        mail_from: &str,
        raw: &[u8],
        replies: Vec<(AccountScope, AutoReply)>,
        steps: &mut Vec<Step>,
    ) {
        for (n, (scope, reply)) in replies.into_iter().enumerate() {
            let step = |decision, reason: String| {
                Step::new(Ek::Receive, decision, vec![reply.to.clone()])
                    .sender("")
                    .reason(reason)
            };
            let skip = sieve::suppressed(mail_from, raw, &reply)
                .or_else(|| (!self.vacation_rate.allow(scope.account)).then_some("rate limit"));
            if let Some(why) = skip {
                tracing::info!(%id, account = %scope.account, reason = why, "smtp: автоответ не отправлен");
                steps.push(step(Dd::Discard, format!("auto-reply suppressed: {why}")));
                continue;
            }
            let msg = sieve::build_reply(
                &reply,
                &self.hostname,
                &format!("{id}.{n}"),
                OffsetDateTime::now_utc(),
            );
            match self
                .deliver_reply(id, scope.account, &reply.to, Bytes::from(msg))
                .await
            {
                Ok((how, more)) => {
                    steps.push(step(Dd::Accept, format!("auto-reply {how}")));
                    steps.extend(more);
                }
                Err(e) => {
                    tracing::warn!(%id, account = %scope.account, error = %e, "smtp: автоответ не отправлен");
                    steps.push(step(Dd::Defer, format!("auto-reply not sent: {e}")));
                }
            }
        }
    }

    /// Письмо автоответа `raw` учётной записи `from` адресату `to` с конвертом `<>`: локальному — как любое письмо,
    /// через его правила (в цепочке перенаправлений — ящик отвечающего: вернуть ему ответ правилом можно, но его
    /// правила к ответу не применяются; на автоответ не отвечают), внешнему — в очередь исходящей почты. Возвращает,
    /// куда отправлено, и события журнала доставки правил получателя.
    async fn deliver_reply(
        &self,
        id: &str,
        from: AccountId,
        to: &str,
        raw: Bytes,
    ) -> Result<(&'static str, Vec<Step>)> {
        match self.resolve(to).await? {
            Resolved::Local(_) => {
                let hop = Hop {
                    raw: sieve::stamp(&raw, &sieve::chain_token(from)),
                    to: vec![to.to_owned()],
                };
                let filing = Filing::new(false, false, "", &hop.raw);
                let sieved = self
                    .relay(id, "", vec![hop], &filing, OffsetDateTime::now_utc(), None)
                    .await?;
                Ok(("delivered", sieved.steps))
            }
            Resolved::External if self.deps.outbound.is_some() => {
                self.send_dsn(self.deps.tenant, to, raw).await?;
                Ok(("queued", Vec::new()))
            }
            Resolved::External => Err(Error::UNAVAILABLE),
            Resolved::Unknown => Err(Error::NotFound),
        }
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

    async fn schedule(
        &self,
        tenant: TenantId,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
        release_at: OffsetDateTime,
    ) -> Result<QueueId> {
        Delivery::schedule(self, tenant, mail_from, rcpts, raw, release_at).await
    }

    async fn cancel_scheduled(&self, tenant: TenantId, queue: QueueId) -> Result<()> {
        Delivery::cancel_scheduled(self, tenant, queue).await
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
    async fn policy(&self, principal: &Principal) -> Result<Policy> {
        self.account_policy(principal.tenant, principal.account)
            .await
    }

    async fn limited(&self, env: &Envelope, reply: &Reply) {
        let step = Step::new(Ek::Reject, tracking::decision(reply), env.rcpts.clone())
            .reason(tracking::reason(reply));
        self.track(&env.mail_from, None, vec![step]).await;
    }

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
        let queue_ok = || {
            if self.deps.outbound.is_some() {
                reply(250, [2, 1, 5], "OK")
            } else {
                reply(451, [4, 3, 2], "Outbound delivery not available")
            }
        };
        match self.srs_reverse(rcpt).await {
            Ok(Some(Ok(_))) => return queue_ok(),
            Ok(Some(Err(e))) => {
                tracing::info!(id = %env.id, error = %e, "smtp: адрес SRS отклонён");
                return reply(550, [5, 1, 1], "Bad SRS address");
            }
            Ok(None) => {}
            Err(e) => return temporary(&e),
        }
        let target = match self.target(rcpt).await {
            Ok(t) => t,
            Err(e) => return temporary(&e),
        };
        if target.foreign {
            tracing::warn!(id = %env.id, "smtp: принятый домен другой организации на этом узле не обслуживается");
            return reply(451, [4, 3, 0], "Domain not served by this node");
        }
        if target.relay {
            return queue_ok();
        }
        let app = sources::application(env.source.as_deref());
        if let Some((s, _)) = app
            && s.tenant != Some(self.deps.tenant)
        {
            tracing::warn!(id = %env.id, source = %s.name, "smtp: источник-приложение другой организации");
            return reply(451, [4, 3, 0], "Source not served by this node");
        }
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
            // Приложение: внешние получатели — только по разрешению источника.
            Resolved::External if app.is_some_and(|(_, external)| !external) => {
                reply(550, [5, 7, 1], "Relaying denied for this source")
            }
            Resolved::External if app.is_some() => queue_ok(),
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

/// Получатели, письмо которым доставляется в «Спам» (учётные записи `junk`).
fn junk_rcpts(deliveries: &[(String, MailDelivery)], junk: &HashSet<AccountId>) -> Vec<String> {
    let mut out = Vec::new();
    push_unique(
        &mut out,
        deliveries
            .iter()
            .filter(|(r, d)| !r.is_empty() && junk.contains(&d.account))
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
    junk: &[String],
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
    let (spam, inbox): (Vec<String>, Vec<String>) =
        delivered.into_iter().partition(|r| junk.contains(r));
    if !inbox.is_empty() {
        steps.push(Step::new(Ek::Deliver, Dd::Accept, inbox));
    }
    if !spam.is_empty() {
        steps.push(Step::new(Ek::Deliver, Dd::Accept, spam).reason("junk folder"));
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

/// Отказ адресу SRS: подпись не сходится или срок истёк.
const BAD_SRS: &str = "550 5.1.1 bad SRS address";

/// Ссылка-заглушка до записи тела ([`Delivery::mail_flow`] заменяет её объектом письма).
fn placeholder_blob(tenant: TenantId) -> BlobRef {
    BlobRef {
        key: tp_contracts::BlobKey {
            tenant,
            hash: tp_contracts::BlobHash([0; 32]),
            generation: tp_contracts::Gen::INITIAL,
        },
        size: 0,
    }
}

/// Объект BLOB целиком.
pub(crate) async fn read_blob(blobs: &dyn BlobStore, key: &tp_contracts::BlobKey) -> Result<Bytes> {
    use futures::TryStreamExt;
    let chunks: Vec<Bytes> = blobs.get(key, None).await?.try_collect().await?;
    Ok(chunks.concat().into())
}

/// Откуда письмо с iTIP и чем подтверждается его `From` ([`Delivery::with_itip`]).
#[derive(Debug, Clone, Copy)]
enum ItipOrigin<'a> {
    /// smtp-in (iMIP, SMTP-33): письмо прошло DMARC.
    Imip { dmarc_pass: bool },
    /// Почтовый клиент (submission, [`Delivery::submit`]; решение 78): учётная запись отправителя и субъект сеанса
    /// submission.
    Internal {
        account: AccountId,
        principal: Option<&'a Principal>,
    },
}

/// Получатели сообщения iTIP: доставки с адресом получателя (не копии модулей) вне «Спама»; при `enabled` = ложь —
/// никого.
fn itip_targets(
    enabled: bool,
    deliveries: &[(String, MailDelivery)],
    junk: &HashSet<AccountId>,
) -> Vec<(String, AccountId)> {
    if !enabled {
        return Vec::new();
    }
    deliveries
        .iter()
        .filter(|(r, d)| !r.is_empty() && !junk.contains(&d.account))
        .map(|(r, d)| (r.clone(), d.account))
        .collect()
}

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

mod moderation;
mod scheduled;

#[cfg(test)]
mod tests;
