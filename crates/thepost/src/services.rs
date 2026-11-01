//! Сервисы ролей узла (APP-04): реализации контрактов поверх `PgMeta`, хранилища писем, BLOB и шины, выбранные
//! по config.yaml, — вход (каталог, OIDC, пароли приложений, ACL), поиск, очередь и доставка SMTP, JMAP, Admin API.
//!
//! Организация — `TenantId::DEFAULT`: несколько организаций на узле — модуль «Мультитенантность» (SRS 1.3).

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::TryStreamExt;
use mail_parser::{MessageParser, MimeHeaders};
use tp_contracts::auth::{Authenticator, Authorizer, Directory};
use tp_contracts::blob::{BlobIndex, BlobStore};
use tp_contracts::bus::EventBus;
use tp_contracts::ext::{DecisionHooks, Registry};
use tp_contracts::ids::{DomainId, MessageId};
use tp_contracts::mailstore::MailStore;
use tp_contracts::meta::{AccountScope, MailboxStore, MessageQuery, MessageStore, Policy};
use tp_contracts::sasl::SaslServer;
use tp_contracts::secrets::SecretStore;
use tp_contracts::{Error, Result, TenantId};
use tp_meta::PgMeta;
use tp_platform::{Config, Metrics, RateLimitConfig, RateLimiter, TlsCerts};
use tp_search::{
    DocumentSource, Extractor, IndexCache, IndexDoc, IndexManager, IndexStore, Indexer, Rebuilder,
    Search, SearchConfig,
};
use tp_smtp::queue::{QueueDeps, system_now};
use tp_smtp::{
    AuthCheck, Delivery, Milters, Queue, QueueWorker, SendLimits, SmtpConfig, SmtpMetrics,
};

use crate::infra;
use crate::sections::{ADMIN, AUTH, AuthSection, JMAP, MAILSTORE, SEARCH, SMTP};

/// Общие сервисы ролей узла.
pub struct Services {
    /// Организация узла.
    pub tenant: TenantId,
    /// Метаданные.
    pub meta: PgMeta,
    /// Хранилище BLOB.
    pub blob: Arc<dyn BlobStore>,
    /// Шина уведомлений.
    pub bus: Arc<dyn EventBus>,
    /// Почтовое хранилище.
    pub store: Arc<dyn MailStore>,
    /// Точки принятия решений модулей.
    pub hooks: DecisionHooks,
    /// Проверка учётных данных: пароль, Bearer, Kerberos.
    pub authn: Arc<dyn Authenticator>,
    /// SASL для SMTP и IMAP.
    pub sasl: Arc<dyn SaslServer>,
    /// Права на папки и отправку от имени.
    pub authz: Arc<dyn Authorizer>,
    /// Каталог LDAP; `None` — учётные записи только из PostgreSQL.
    pub directory: Option<Arc<dyn Directory>>,
    /// Пароли приложений.
    pub app_passwords: tp_auth::AppPasswords,
    /// Полнотекстовый поиск (роль search в процессе).
    pub search: Arc<Search>,
    /// Индексы ящиков на узле.
    pub indexes: Arc<IndexManager>,
    /// Сегменты индексов в BlobStore.
    pub index_store: Arc<IndexStore>,
    /// Кэш индексов узла.
    pub cache: Arc<IndexCache>,
    /// Содержимое писем для индекса.
    pub documents: Arc<dyn DocumentSource>,
    /// Очередь исходящей почты с DKIM-подписью.
    pub queue: Arc<Queue>,
    /// DKIM-подпись (ротация ключей — по перечитанной секции `smtp.dkim`).
    pub dkim: Arc<tp_smtp::dkim_sign::DkimSign>,
    /// Метрики SMTP.
    pub smtp_metrics: Arc<SmtpMetrics>,
    /// Лимиты отправки учётной записи в час.
    pub send_limits: Arc<SendLimits>,
    /// Ограничитель частоты узла.
    pub limiter: Arc<RateLimiter>,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services")
            .field("tenant", &self.tenant)
            .field("directory", &self.directory.is_some())
            .finish_non_exhaustive()
    }
}

/// Хранилища, открытые узлом до сборки сервисов.
#[allow(missing_debug_implementations)]
pub struct Base<'a> {
    /// Конфигурация.
    pub cfg: &'a Config,
    /// Метаданные.
    pub meta: PgMeta,
    /// BLOB.
    pub blob: Arc<dyn BlobStore>,
    /// Шина.
    pub bus: Arc<dyn EventBus>,
    /// Хранилище секретов.
    pub secrets: Arc<dyn SecretStore>,
    /// Точки расширения модулей.
    pub ext: &'a Registry,
    /// Реестр метрик.
    pub metrics: &'a Metrics,
}

impl Services {
    /// Сборка сервисов. Ошибка секции, каталога или OIDC — до запуска ролей.
    pub async fn build(b: Base<'_>) -> Result<Self> {
        let Base {
            cfg,
            meta,
            blob,
            bus,
            secrets,
            ext,
            metrics,
        } = b;
        let tenant = TenantId::DEFAULT;
        let profile = cfg.profile();
        let hooks = ext.decision_hooks();
        let store: Arc<dyn MailStore> = Arc::new(tp_mailstore::Store::with_config(
            tp_mailstore::Deps {
                blobs: blob.clone(),
                index: Arc::new(meta.clone()),
                messages: Arc::new(meta.clone()),
                changes: Arc::new(meta.clone()),
                trash: Arc::new(meta.clone()),
                quota: Arc::new(meta.clone()),
                bus: bus.clone(),
                hooks: hooks.clone(),
            },
            (*cfg.section::<tp_mailstore::Config>(MAILSTORE)?).clone(),
        ));

        // Вход: пароль приложения или каталога, Bearer OIDC, Kerberos; права — ACL папок.
        let auth = cfg.section::<AuthSection>(AUTH)?;
        let mut accounts = tp_auth::Accounts::new(tenant, Arc::new(meta.clone()));
        let ldap = match &auth.ldap {
            Some(l) => Some(Arc::new(
                tp_auth::LdapDirectory::connect(tenant, l.clone(), profile, secrets.as_ref())
                    .await
                    .map_err(|e| infra::context("auth.ldap", e))?,
            )),
            None => None,
        };
        if let Some(ldap) = &ldap {
            accounts = accounts.with_groups(ldap.clone());
        }
        let app_passwords =
            tp_auth::AppPasswords::new(Arc::new(meta.clone()), auth.passwords.clone())
                .map_err(|e| infra::context("auth.passwords", e))?;
        let mut password = tp_auth::PasswordAuthenticator::new(accounts.clone())
            .with_app_passwords(app_passwords.clone());
        if let Some(ldap) = &ldap {
            password = password.with_directory(ldap.clone());
        }
        let mut router = tp_auth::CredentialsRouter::default().password(Arc::new(password));
        if let Some(oidc) = &auth.oidc {
            let bearer = tp_auth::OidcAuthenticator::new(oidc.clone(), profile, accounts.clone())
                .await
                .map_err(|e| infra::context("auth.oidc", e))?;
            router = router.bearer(Arc::new(bearer));
        }
        if let Some(gssapi) = &auth.gssapi {
            router = router.gssapi(Arc::new(tp_auth::KerberosAuthenticator::new(
                accounts.clone(),
                gssapi.clone(),
            )));
        }
        let authn: Arc<dyn Authenticator> = Arc::new(router);
        // TODO(AUTH-05): механизм GSSAPI (`AuthSasl::with_gssapi(Gssapi::system(..))`) — после включения feature
        // `gssapi` в сборке (нужна libkrb5 в CI и образе).
        let sasl: Arc<dyn SaslServer> = Arc::new(tp_auth::AuthSasl::new(authn.clone()));
        let authz: Arc<dyn Authorizer> = Arc::new(tp_auth::AclAuthorizer::new(
            Arc::new(meta.clone()),
            Arc::new(meta.clone()),
        ));
        let directory = ldap.map(|l| l as Arc<dyn Directory>);

        // Поиск: индексы на узле, сегменты в BlobStore, кэш; счётчики кэша — в /metrics.
        let search_cfg = cfg.section::<SearchConfig>(SEARCH)?;
        let indexes = Arc::new(IndexManager::new(search_cfg.index.clone()));
        let index_store = Arc::new(IndexStore::new(
            blob.clone(),
            Arc::new(meta.clone()),
            search_cfg.persist.clone(),
        ));
        let cache = Arc::new(IndexCache::new(
            indexes.clone(),
            index_store.clone(),
            search_cfg.cache.clone(),
        ));
        crate::metrics::register_search_cache(metrics, cache.clone());
        let search = Arc::new(
            Search::new(indexes.clone(), search_cfg.query.clone()).with_cache(cache.clone()),
        );
        let extractor = match Extractor::worker_command() {
            Ok(cmd) => Some(Arc::new(Extractor::new(search_cfg.extract.clone(), cmd))),
            Err(e) => {
                tracing::warn!(error = %e, "search: текст вложений не извлекается (нет пути к исполняемому файлу)");
                None
            }
        };
        let documents: Arc<dyn DocumentSource> = Arc::new(MailDocuments {
            messages: Arc::new(meta.clone()),
            mailboxes: Arc::new(meta.clone()),
            store: store.clone(),
            extractor,
        });

        // Очередь исходящей почты: письмо и уведомления подписываются DKIM при постановке.
        let smtp = cfg.section::<SmtpConfig>(SMTP)?;
        let dkim = Arc::new(tp_smtp::dkim_sign::DkimSign::new(
            secrets.clone(),
            smtp.dkim.clone(),
        ));
        let queue = Arc::new(Queue::new(queue_deps(&meta, &blob)).with_dkim(dkim.clone()));
        let smtp_metrics = Arc::new(SmtpMetrics::new());
        smtp_metrics.register(metrics);
        let limiter = Arc::new(RateLimiter::new(
            (*cfg.section::<RateLimitConfig>("ratelimit")?).clone(),
        ));

        // Секции, читаемые ролями при запуске, проверяются здесь: ошибка — до запуска ролей.
        cfg.section::<tp_jmap::JmapConfig>(JMAP)?;
        cfg.section::<tp_admin::AdminConfig>(ADMIN)?;

        Ok(Self {
            tenant,
            meta,
            blob,
            bus,
            store,
            hooks,
            authn,
            sasl,
            authz,
            directory,
            app_passwords,
            search,
            indexes,
            index_store,
            cache,
            documents,
            queue,
            dkim,
            smtp_metrics,
            send_limits: Arc::new(SendLimits::new()),
            limiter,
        })
    }

    /// Доставка и обработчик транзакций SMTP (smtp-in, submission): проверки SPF/DKIM/DMARC/ARC, Milter, очередь.
    pub fn delivery(&self, smtp: &SmtpConfig) -> Result<Arc<Delivery>> {
        let resolver = system_resolver()?;
        let milter = (!smtp.milter.filters.is_empty())
            .then(|| Arc::new(Milters::new(smtp.milter.clone(), smtp.hostname.clone())));
        let deps = tp_smtp::deliver::Deps {
            tenant: self.tenant,
            store: self.store.clone(),
            accounts: Arc::new(self.meta.clone()),
            mailboxes: Arc::new(self.meta.clone()),
            directory: self.directory.clone(),
            quota: Some(Arc::new(self.meta.clone())),
            outbound: Some(self.queue.clone()),
            hooks: self.hooks.clone(),
            authcheck: Some(Arc::new(AuthCheck::new(resolver, smtp))),
            milter,
        };
        Ok(Arc::new(Delivery::new(deps, smtp)))
    }

    /// Зависимости ролей smtp-in и submission.
    pub fn smtp_server(
        &self,
        smtp: &SmtpConfig,
        tls: Option<Arc<TlsCerts>>,
    ) -> Result<tp_smtp::ServerDeps> {
        Ok(tp_smtp::ServerDeps {
            handler: self.delivery(smtp)?,
            sasl: Some(self.sasl.clone()),
            limiter: Some(self.limiter.clone()),
            tls,
            send_limits: Some(self.send_limits.clone()),
            metrics: Some(self.smtp_metrics.clone()),
        })
    }

    /// Обработчик очереди роли smtp-out: доставка на MX с MTA-STS и DANE, уведомления — в ту же очередь.
    pub fn queue_worker(&self, smtp: &SmtpConfig) -> Result<QueueWorker> {
        let transport = tp_smtp::queue::send::MxTransport::new(
            system_resolver()?,
            smtp.authcheck.dns_cache_size,
        )
        .map_err(|e| Error::Internal(format!("smtp.queue: {e}")))?;
        let policy = tp_smtp::policy::TlsPolicy::system()
            .map_err(|e| Error::Internal(format!("smtp.queue.policy: {e}")))?;
        let transport = transport.with_policy(Arc::new(policy));
        Ok(QueueWorker::new(
            queue_deps(&self.meta, &self.blob),
            Arc::new(transport),
            smtp.hostname.clone(),
        )
        .with_dsn(self.queue.clone())
        .with_metrics(self.smtp_metrics.clone()))
    }

    /// Зависимости IMAP.
    pub fn imap(&self) -> tp_imap::Deps {
        tp_imap::Deps {
            sasl: self.sasl.clone(),
            auth: self.authn.clone(),
            mailboxes: Arc::new(self.meta.clone()),
            messages: Arc::new(self.meta.clone()),
            store: self.store.clone(),
            search: self.search.clone(),
            changes: Arc::new(self.meta.clone()),
            bus: self.bus.clone(),
        }
    }

    /// Сервер JMAP; отправка — в очередь SMTP с DKIM-подписью.
    pub fn jmap(&self, ext: &Registry, cfg: tp_jmap::JmapConfig) -> Result<tp_jmap::Jmap> {
        let deps = tp_jmap::Deps {
            auth: self.authn.clone(),
            mailboxes: Arc::new(self.meta.clone()),
            messages: Arc::new(self.meta.clone()),
            changes: Arc::new(self.meta.clone()),
            store: self.store.clone(),
            search: self.search.clone(),
            blobs: self.blob.clone(),
            blob_index: Arc::new(self.meta.clone()),
            bus: self.bus.clone(),
            accounts: Arc::new(self.meta.clone()),
            authz: self.authz.clone(),
            outbound: Some(Arc::new(JmapOutbound(self.queue.clone()))),
            hooks: self.hooks.clone(),
            capabilities: ext.jmap_capabilities().to_vec(),
        };
        tp_jmap::Jmap::new(deps, cfg).map_err(|e| infra::context("jmap", e))
    }

    /// Admin API.
    pub fn admin(&self, ext: &Registry, cfg: tp_admin::AdminConfig) -> Result<tp_admin::Admin> {
        let deps = tp_admin::Deps {
            auth: self.authn.clone(),
            audit: Arc::new(tp_admin::LogAudit),
            modules: ext.admin_routes().to_vec(),
            accounts: Arc::new(self.meta.clone()),
            quota: Arc::new(self.meta.clone()),
            policies: Arc::new(PgPolicies(self.meta.clone())),
            resolver: Arc::new(self.meta.clone()),
            domains: Arc::new(NoDomains),
            trash: Arc::new(self.meta.clone()),
            store: self.store.clone(),
            app_passwords: self.app_passwords.clone(),
        };
        tp_admin::Admin::new(deps, cfg).map_err(|e| infra::context("admin", e))
    }

    /// Индексатор журнала событий (подписчик `indexer`).
    pub fn indexer(&self, cfg: &SearchConfig) -> Indexer {
        Indexer::new(
            self.indexes.clone(),
            self.documents.clone(),
            cfg.indexer.clone(),
        )
        .with_cache(self.cache.clone())
    }

    /// Перестройка индексов по запросам и при смене схемы.
    pub fn rebuilder(&self, cfg: &SearchConfig) -> Rebuilder {
        Rebuilder::new(
            self.indexes.clone(),
            self.documents.clone(),
            cfg.rebuild.clone(),
        )
        .with_store(self.index_store.clone())
    }
}

fn queue_deps(meta: &PgMeta, blob: &Arc<dyn BlobStore>) -> QueueDeps {
    QueueDeps {
        store: Arc::new(meta.clone()),
        blobs: blob.clone(),
        index: Arc::new(meta.clone()) as Arc<dyn BlobIndex>,
        now: system_now(),
    }
}

/// Системный резолвер DNS (`/etc/resolv.conf`).
pub fn system_resolver() -> Result<mail_auth::MessageAuthenticator> {
    mail_auth::MessageAuthenticator::new_system_conf()
        .map_err(|e| Error::Internal(format!("резолвер DNS: {e}")))
}

/// Отправка JMAP (`EmailSubmission/set`) — в очередь SMTP: `Queue` подписывает письмо DKIM при постановке.
struct JmapOutbound(Arc<Queue>);

#[async_trait]
impl tp_jmap::Outbound for JmapOutbound {
    async fn enqueue(
        &self,
        tenant: TenantId,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<()> {
        tp_smtp::Outbound::enqueue(self.0.as_ref(), tenant, mail_from, rcpts, raw).await
    }
}

/// Политики уровней Admin API поверх `PgMeta` (до контракта CTR-25).
struct PgPolicies(PgMeta);

fn policy_target(t: tp_admin::PolicyTarget) -> tp_meta::retention::PolicyTarget {
    match t {
        tp_admin::PolicyTarget::Tenant => tp_meta::retention::PolicyTarget::Tenant,
        tp_admin::PolicyTarget::Domain(d) => tp_meta::retention::PolicyTarget::Domain(d),
        tp_admin::PolicyTarget::Account(a) => tp_meta::retention::PolicyTarget::Account(a),
    }
}

#[async_trait]
impl tp_admin::PolicyStore for PgPolicies {
    async fn policy(&self, tenant: TenantId, target: tp_admin::PolicyTarget) -> Result<Policy> {
        self.0.policy(tenant, policy_target(target)).await
    }

    async fn set_policy(
        &self,
        tenant: TenantId,
        target: tp_admin::PolicyTarget,
        policy: &Policy,
    ) -> Result<()> {
        self.0
            .set_policy(tenant, policy_target(target), policy)
            .await
    }
}

/// Домены Admin API: хранения доменов организации в контрактах ещё нет (CTR-25) — операции недоступны.
struct NoDomains;

const NO_DOMAINS: &str = "домены организации: хранилище ещё не подключено (CTR-25)";

#[async_trait]
impl tp_admin::DomainStore for NoDomains {
    async fn list(&self, _tenant: TenantId) -> Result<Vec<tp_admin::Domain>> {
        Err(Error::Internal(NO_DOMAINS.to_owned()))
    }

    async fn create(&self, _tenant: TenantId, _name: &str) -> Result<tp_admin::Domain> {
        Err(Error::Internal(NO_DOMAINS.to_owned()))
    }

    async fn delete(&self, _tenant: TenantId, _id: DomainId) -> Result<()> {
        Err(Error::Internal(NO_DOMAINS.to_owned()))
    }
}

/// Содержимое писем для индекса поиска: конверт — из метаданных приёма, текст и вложения — разбор письма
/// (`mail-parser`), текст вложений — в изолированных подпроцессах (`search-extract`).
struct MailDocuments {
    messages: Arc<dyn MessageStore>,
    mailboxes: Arc<dyn MailboxStore>,
    store: Arc<dyn MailStore>,
    extractor: Option<Arc<Extractor>>,
}

/// Адреса через запятую: «Имя <адрес>».
fn addresses(list: &[tp_contracts::meta::Address]) -> String {
    list.iter()
        .map(|a| match &a.name {
            Some(n) => format!("{n} <{}>", a.email),
            None => a.email.clone(),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[async_trait]
impl DocumentSource for MailDocuments {
    async fn document(&self, scope: AccountScope, message: MessageId) -> Result<Option<IndexDoc>> {
        let Some(m) = self.messages.get(scope, message).await? else {
            return Ok(None);
        };
        let raw: Vec<u8> = match self.store.fetch_raw(scope, message, None).await {
            Ok(stream) => {
                stream
                    .try_fold(Vec::new(), |mut acc, chunk| async move {
                        acc.extend_from_slice(&chunk);
                        Ok(acc)
                    })
                    .await?
            }
            Err(Error::NotFound) => return Ok(None),
            Err(e) => return Err(e),
        };
        let env = &m.meta.envelope;
        let mut doc = IndexDoc {
            message,
            mailbox: m.mailbox,
            received_at: m.received_at,
            subject: env.subject.clone().unwrap_or_default(),
            from: addresses(&env.from),
            to: addresses(&env.to),
            cc: addresses(&env.cc),
            bcc: addresses(&env.bcc),
            body: String::new(),
            attachments: String::new(),
            headers: Vec::new(),
        };
        // Разбор и извлечение — вне рантайма: письмо может быть большим.
        let parts = tokio::task::spawn_blocking(move || parse(&raw))
            .await
            .map_err(|e| Error::Internal(format!("search: разбор письма прерван: {e}")))?;
        doc.body = parts.body;
        doc.headers = parts.headers;
        let mut attachments = Vec::new();
        for a in parts.attachments {
            attachments.push(a.name.clone());
            if let Some(x) = &self.extractor {
                match x.extract(&a.content_type, &a.name, &a.data).await {
                    Ok(text) => attachments.push(text),
                    Err(e) => tracing::debug!(error = %e, "search: текст вложения не извлечён"),
                }
            }
        }
        doc.attachments = attachments
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        Ok(Some(doc))
    }

    async fn messages(&self, scope: AccountScope) -> Result<Vec<MessageId>> {
        let mut out = Vec::new();
        for mailbox in self.mailboxes.list(scope).await? {
            let list = self
                .messages
                .list(scope, mailbox.id, MessageQuery::All, usize::MAX)
                .await?;
            out.extend(list.into_iter().map(|m| m.id));
        }
        Ok(out)
    }
}

/// Вложение письма.
struct Attachment {
    name: String,
    content_type: String,
    data: Vec<u8>,
}

/// Разобранное письмо.
struct Parsed {
    body: String,
    headers: Vec<(String, String)>,
    attachments: Vec<Attachment>,
}

fn parse(raw: &[u8]) -> Parsed {
    let Some(msg) = MessageParser::default().parse(raw) else {
        return Parsed {
            body: String::from_utf8_lossy(raw).into_owned(),
            headers: Vec::new(),
            attachments: Vec::new(),
        };
    };
    let mut body = Vec::new();
    let mut i = 0;
    while let Some(text) = msg.body_text(i) {
        body.push(text.into_owned());
        i += 1;
    }
    let headers = msg
        .headers_raw()
        .map(|(n, v)| (n.to_owned(), v.trim().to_owned()))
        .collect();
    let attachments = msg
        .attachments()
        .map(|a| Attachment {
            name: a.attachment_name().unwrap_or_default().to_owned(),
            content_type: a
                .content_type()
                .map(|c| match c.subtype() {
                    Some(sub) => format!("{}/{sub}", c.ctype()),
                    None => c.ctype().to_owned(),
                })
                .unwrap_or_default(),
            data: a.contents().to_vec(),
        })
        .collect();
    Parsed {
        body: body.join("\n"),
        headers,
        attachments,
    }
}
