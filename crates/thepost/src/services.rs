//! Сервисы ролей узла (APP-04): реализации контрактов поверх `PgMeta`, хранилища писем, BLOB и шины, выбранные
//! по config.yaml, — вход (каталог, OIDC, пароли приложений, ACL), поиск, очередь и доставка SMTP, JMAP, Admin API.
//!
//! Доставка SMTP пишет журнал доставки (`tp_smtp::Tracking`, SMTP-17) и раскрывает группы рассылки с модерацией
//! (`tp_smtp::Groups`, SMTP-16); обучение антиспама по перемещению писем — подписчик журнала событий роли
//! maintenance (`tp_smtp::learn::SpamLearner`, SMTP-18). Проверка учётных данных всех протоколов (SMTP AUTH, IMAP,
//! POP3, JMAP, Admin API) — через защиту от перебора (`tp_auth::AbuseGuard`, AUTH-08): [`Guarded`].
//!
//! Настройки транспорта (APP-09, SRS 4.1) — один снимок на узел ([`Services::routes`], `tp_smtp::routes::Routes`
//! поверх `PgMeta`): доверенные источники и принятые домены smtp-in и submission, переписывание адресов доставки,
//! политики доменов назначения smtp-out; перечитывается по сигналам `TransportChanged` и `Resync`
//! (`Routes::watch` запускает `serve`). Правила потока почты — модуль (`Registry::mail_flow`): один раз на письмо в
//! доставке smtp-in и submission и в JMAP `EmailSubmission/set` (доставка JMAP их не применяет повторно).
//!
//! Организация — `TenantId::DEFAULT`: несколько организаций на узле — модуль «Мультитенантность» (SRS 1.3).

use std::sync::Arc;

use async_trait::async_trait;
use futures::TryStreamExt;
use mail_parser::{MessageParser, MimeHeaders};
use tp_contracts::auth::{Authenticator, Authorizer, Credentials, Directory, Principal};
use tp_contracts::blob::{BlobIndex, BlobStore};
use tp_contracts::bus::EventBus;
use tp_contracts::events::EventSink;
use tp_contracts::ext::{DecisionHooks, Registry};
use tp_contracts::ids::{DomainId, MessageId};
use tp_contracts::mailstore::MailStore;
use tp_contracts::membership::NodeId;
use tp_contracts::meta::{
    AccountScope, AccountStore, MailboxStore, MessageQuery, MessageStore, Policy,
};
use tp_contracts::queue::Outbound;
use tp_contracts::sasl::SaslServer;
use tp_contracts::secrets::SecretStore;
use tp_contracts::transport::MailFlow;
use tp_contracts::{Error, Result, TenantId};
use tp_meta::PgMeta;
use tp_platform::{Config, Metrics, RateLimitConfig, RateLimiter, TlsCerts};
use tp_search::{
    DocumentSource, Extractor, IndexCache, IndexDoc, IndexManager, IndexStore, Indexer, Rebuilder,
    Search, SearchConfig,
};
use tp_smtp::forward::Forwarding;
use tp_smtp::queue::{QueueDeps, system_now};
use tp_smtp::routes::Routes;
use tp_smtp::sources::Sources;
use tp_smtp::{
    AuthCheck, Delivery, Groups, Milters, Queue, QueueWorker, SendLimits, SmtpConfig, SmtpMetrics,
    Tracking,
};

use crate::infra;
use crate::sections::{ADMIN, AUTH, AdminSection, AuthSection, JMAP, MAILSTORE, SEARCH, SMTP};

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
    /// То же хранилище: воркер истечения корзины роли maintenance (`Store::run_trash_expiry`); клоны разделяют
    /// сигнал о помещении в корзину этим узлом.
    pub mail: tp_mailstore::Store,
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
    /// Журнал доставки (SMTP-17): события писем от имени узла.
    pub tracking: Tracking,
    /// Защита от перебора (AUTH-08): счётчики неудач узла, блокировки — на всех узлах; сигналы блокировок —
    /// `AbuseGuard::listen` (запускает `serve`).
    pub abuse: tp_auth::AbuseGuard,
    /// Хранилище секретов узла (модуля, если подключён): пароль Rspamd controller, ключи SRS и ARC пересылки.
    pub secrets: Arc<dyn SecretStore>,
    /// Снимки настроек транспорта узла (`PgMeta`), общие для smtp-in, submission, smtp-out и JMAP; сигналы
    /// `TransportChanged` и `Resync` — `Routes::watch` (запускает `serve`).
    pub routes: Arc<Routes>,
    /// Правила потока почты модуля (`Registry::mail_flow`); `None` — модуль не включён.
    pub mail_flow: Option<Arc<dyn MailFlow>>,
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
    /// Имя узла (журнал доставки).
    pub node: NodeId,
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
            node,
        } = b;
        let tenant = TenantId::DEFAULT;
        let profile = cfg.profile();
        let hooks = ext.decision_hooks();
        // Запись — со сжатием по `blob.compression`; чтение распаковывает само хранилище (MS-15). JMAP
        // (upload/download) читает объекты через `Decompressing` (JMAP-13): загрузка может совпасть по содержимому
        // со сжатой частью письма.
        let writer = tp_blob::WriterConfig {
            compression: cfg
                .section::<crate::sections::BlobSection>(crate::sections::BLOB)?
                .compression
                .clone(),
            ..tp_blob::WriterConfig::default()
        };
        let mail = tp_mailstore::Store::with_writer(
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
            writer,
        );
        let store: Arc<dyn MailStore> = Arc::new(mail.clone());

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
        // Защита от перебора; хранилище блокировок недоступно — по `auth.abuse.fail_closed` (решение владельца #253).
        let abuse = tp_auth::AbuseGuard::new(auth.abuse.clone(), Arc::new(meta.clone()));
        let authn: Arc<dyn Authenticator> = Arc::new(Guarded {
            inner: Arc::new(router),
            guard: abuse.clone(),
            accounts: Arc::new(meta.clone()),
            tenant,
        });
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
        cfg.section::<AdminSection>(ADMIN)?;
        let tracking = Tracking::new(Arc::new(meta.clone()), node);
        let routes = Arc::new(Routes::new(Arc::new(meta.clone())));

        Ok(Self {
            tenant,
            meta,
            blob,
            bus,
            store,
            mail,
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
            tracking,
            abuse,
            secrets,
            routes,
            mail_flow: ext.mail_flow(),
        })
    }

    /// Доставка и обработчик транзакций SMTP (smtp-in, submission): проверки SPF/DKIM/DMARC/ARC, Milter, очередь,
    /// группы рассылки с модерацией, журнал доставки, настройки транспорта, пересылка (SRS, ARC) и правила потока
    /// почты модуля (если включён).
    pub fn delivery(&self, smtp: &SmtpConfig) -> Result<Arc<Delivery>> {
        let delivery = self.base_delivery(smtp)?;
        Ok(Arc::new(match &self.mail_flow {
            Some(flow) => delivery.with_mail_flow(
                flow.clone(),
                self.blob.clone(),
                Arc::new(self.meta.clone()),
            ),
            None => delivery,
        }))
    }

    /// Доставка без правил потока почты: JMAP применяет их сам до `DecisionHooks::on_submission`
    /// (`Jmap::with_mail_flow`), повторный вызов нарушил бы «один раз на письмо».
    fn base_delivery(&self, smtp: &SmtpConfig) -> Result<Delivery> {
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
        let groups = Groups::new(
            Arc::new(self.meta.clone()),
            self.blob.clone(),
            Arc::new(self.meta.clone()),
        );
        let forwarding = Forwarding::new(self.secrets.clone(), smtp.forward.clone());
        Ok(Delivery::new(deps, smtp)
            .with_groups(groups)
            .with_tracking(self.tracking.clone())
            .with_transport(self.routes.clone())
            .with_forwarding(Arc::new(forwarding)))
    }

    /// Доверенные источники приёма smtp-in и submission (шлюз, приложения) из снимков узла.
    pub fn sources(&self) -> Sources {
        Sources::new(self.routes.clone())
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

    /// Обработчик очереди роли smtp-out: доставка на MX с MTA-STS и DANE, уведомления — в ту же очередь; политики
    /// доменов назначения организаций (SMTP-15) и внутренняя ретрансляция — из снимков узла [`Services::routes`]
    /// (перечитывает их `Routes::watch` узла, у роли своей подписки нет).
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
        .with_metrics(self.smtp_metrics.clone())
        .with_tracking(self.tracking.clone())
        .with_routes(self.routes.clone(), None))
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
            trash: Arc::new(self.meta.clone()),
            hooks: self.hooks.clone(),
        }
    }

    /// Зависимости расширений IMAP: QUOTA (учётные записи), ACL и общие папки (ACL папок, единый слой
    /// авторизации). Без них `tp-imap` эти возможности не объявляет.
    pub fn imap_extensions(&self) -> tp_imap::Extensions {
        tp_imap::Extensions {
            accounts: Some(Arc::new(self.meta.clone())),
            acl: Some(Arc::new(self.meta.clone())),
            authz: Some(self.authz.clone()),
        }
    }

    /// Сервер JMAP; отправка — через доставку SMTP (CMP-07): локальным адресатам — сразу в ящики, внешним — в
    /// очередь SMTP с DKIM-подписью. Правила потока почты модуля — в `EmailSubmission/set` (`Jmap::with_mail_flow`).
    /// Профиль `.mobileconfig` подписывается текущим сертификатом TLS узла `certs` (APP-23); без TLS — без подписи.
    pub fn jmap(
        &self,
        ext: &Registry,
        cfg: tp_jmap::JmapConfig,
        smtp: &SmtpConfig,
        certs: Option<Arc<TlsCerts>>,
    ) -> Result<tp_jmap::Jmap> {
        // `Delivery` — `tp_contracts::queue::Outbound` (CTR-28): статус по получателю — в `deliveryStatus`.
        let outbound: Arc<dyn Outbound> = Arc::new(self.base_delivery(smtp)?);
        let deps = tp_jmap::Deps {
            auth: self.authn.clone(),
            mailboxes: Arc::new(self.meta.clone()),
            messages: Arc::new(self.meta.clone()),
            changes: Arc::new(self.meta.clone()),
            store: self.store.clone(),
            search: self.search.clone(),
            blobs: Arc::new(tp_blob::Decompressing::new(
                self.blob.clone(),
                Arc::new(self.meta.clone()),
            )),
            blob_index: Arc::new(self.meta.clone()),
            bus: self.bus.clone(),
            accounts: Arc::new(self.meta.clone()),
            authz: self.authz.clone(),
            outbound: Some(outbound),
            hooks: self.hooks.clone(),
            capabilities: ext.jmap_capabilities().to_vec(),
            trash: Arc::new(self.meta.clone()),
            policies: Arc::new(self.meta.clone()),
        };
        Ok(tp_jmap::Jmap::new(deps, cfg)
            .map_err(|e| infra::context("jmap", e))?
            .with_mail_flow(self.mail_flow.clone())
            .with_profile_signer(crate::profile_signer::profile_signer(certs)))
    }

    /// Admin API.
    pub fn admin(&self, ext: &Registry, cfg: tp_admin::AdminConfig) -> Result<tp_admin::Admin> {
        let deps = tp_admin::Deps {
            auth: self.authn.clone(),
            audit: Arc::new(tp_admin::LogAudit),
            // TODO(ADM-05): чтение журнала аудита — нет реализации `AuditLog` поверх `audit_log` (META-22).
            audit_log: None,
            delivery: Arc::new(self.meta.clone()),
            groups: Arc::new(self.meta.clone()),
            moderation: Arc::new(NoModeration),
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

    /// Обучение антиспама (подписчик журнала событий `smtp-learn` роли maintenance): письмо, перенесённое в «Спам»
    /// или из него, — в Rspamd controller; пароль — из хранилища секретов узла. `smtp.learn.enabled: false` — `None`.
    pub fn spam_learner(&self, smtp: &SmtpConfig) -> Result<Option<Arc<dyn EventSink>>> {
        if !smtp.learn.enabled {
            return Ok(None);
        }
        let controller = tp_smtp::learn::RspamdController::new(&smtp.learn)?;
        Ok(Some(Arc::new(tp_smtp::learn::SpamLearner::new(
            smtp.learn.clone(),
            self.store.clone(),
            Arc::new(self.meta.clone()),
            Some(self.secrets.clone()),
            Arc::new(controller),
        ))))
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

/// Проверка учётных данных под защитой от перебора: `AbuseGuard::check` до проверки, `failure` после отказа
/// (`Forbidden`), `success` после входа. Попытка — учётная запись по имени входа (пароль); адреса клиента контракт
/// `Authenticator` не передаёт — счётчики адреса и подсети ждут его в протоколах. Заблокированная попытка —
/// `Forbidden` (протокол отвечает как на неверный пароль, учётные данные не проверяются).
///
/// Хранилище блокировок недоступно — решение по `auth.abuse.fail_closed` принимает `AbuseGuard` (AUTH-11,
/// решение владельца #253): `false` (по умолчанию) — вход проверяется по блокировкам, известным узлу; `true` —
/// ошибка `AbuseGuard::check` возвращается вызывающему (`Unavailable`: протокол отвечает временным отказом).
struct Guarded {
    inner: Arc<dyn Authenticator>,
    guard: tp_auth::AbuseGuard,
    accounts: Arc<dyn AccountStore>,
    tenant: TenantId,
}

impl Guarded {
    /// Попытка входа по предъявленным данным: для пароля — учётная запись по имени входа (нет или ошибка
    /// каталога — без учётной записи); токен и Kerberos — субъект известен только после проверки.
    async fn attempt(&self, credentials: &Credentials) -> tp_auth::Attempt {
        let account = match credentials {
            Credentials::Password { login, .. } => {
                match self.accounts.by_login(self.tenant, login).await {
                    Ok(a) => a.map(|a| a.id),
                    Err(e) => {
                        tracing::debug!(error = %e, "защита от перебора: учётная запись не найдена");
                        None
                    }
                }
            }
            _ => None,
        };
        tp_auth::Attempt { ip: None, account }
    }
}

#[async_trait]
impl Authenticator for Guarded {
    async fn verify(&self, credentials: Credentials) -> Result<Principal> {
        let attempt = self.attempt(&credentials).await;
        match self.guard.check(self.tenant, &attempt).await {
            Ok(Some(_)) => return Err(Error::Forbidden),
            Ok(None) => {}
            Err(e) => return Err(e),
        }
        match self.inner.verify(credentials).await {
            Ok(p) => {
                let ok = tp_auth::Attempt {
                    ip: attempt.ip,
                    account: Some(p.account),
                };
                self.guard.success(p.tenant, &ok).await;
                Ok(p)
            }
            Err(Error::Forbidden) => {
                if let Err(e) = self.guard.failure(self.tenant, &attempt).await {
                    tracing::warn!(error = %e, "защита от перебора: блокировка не записана");
                }
                Err(Error::Forbidden)
            }
            Err(e) => Err(e),
        }
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

/// Решения модерации групп: доставки одобренного письма участникам и DSN отказа в сборке ещё нет.
struct NoModeration;

const NO_MODERATION: &str = "модерация групп рассылки: доставка решений ещё не подключена";

#[async_trait]
impl tp_admin::groups::Moderation for NoModeration {
    async fn deliver(
        &self,
        _tenant: TenantId,
        _group: &tp_contracts::groups::DistributionGroup,
        _message: &tp_contracts::groups::HeldMessage,
    ) -> Result<()> {
        Err(Error::Internal(NO_MODERATION.to_owned()))
    }

    async fn reject(
        &self,
        _tenant: TenantId,
        _message: &tp_contracts::groups::HeldMessage,
        _reason: &str,
    ) -> Result<()> {
        Err(Error::Internal(NO_MODERATION.to_owned()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use tp_testkit::{FakeAuthenticator, MemAbuseBlocks, MemMetadata, SystemClock};

    const LOGIN: &str = "alice@example.org";

    fn password(secret: &str) -> Credentials {
        Credentials::Password {
            login: LOGIN.into(),
            secret: secret.into(),
        }
    }

    async fn guarded(blocks: MemAbuseBlocks, account_failures: u32, fail_closed: bool) -> Guarded {
        let meta = MemMetadata::new(Arc::new(SystemClock));
        let scope = meta.account(LOGIN).await.expect("account");
        let fake = FakeAuthenticator::default();
        fake.add_password(
            LOGIN,
            "right",
            Principal {
                tenant: scope.tenant,
                account: scope.account,
                login: LOGIN.into(),
                groups: BTreeSet::new(),
                scopes: None,
            },
        );
        let cfg = tp_auth::AbuseConfig {
            account_failures,
            fail_closed,
            ..tp_auth::AbuseConfig::default()
        };
        Guarded {
            inner: Arc::new(fake),
            guard: tp_auth::AbuseGuard::new(cfg, Arc::new(blocks)),
            accounts: Arc::new(meta),
            tenant: TenantId::DEFAULT,
        }
    }

    /// Неудачи считаются по учётной записи из имени входа; успешный вход сбрасывает счётчик; порог — блокировка,
    /// после которой отвергается и верный пароль.
    #[tokio::test]
    async fn guarded_blocks_account_after_failures() {
        let g = guarded(MemAbuseBlocks::default(), 3, false).await;
        for _ in 0..2 {
            assert_eq!(g.verify(password("wrong")).await, Err(Error::Forbidden));
        }
        g.verify(password("right")).await.expect("login");
        for _ in 0..2 {
            assert_eq!(g.verify(password("wrong")).await, Err(Error::Forbidden));
        }
        g.verify(password("right"))
            .await
            .expect("login after reset");
        for _ in 0..3 {
            assert_eq!(g.verify(password("wrong")).await, Err(Error::Forbidden));
        }
        assert_eq!(g.verify(password("right")).await, Err(Error::Forbidden));
    }

    /// Хранилище блокировок недоступно, `auth.abuse.fail_closed: false` — вход проверяется (решение владельца #253,
    /// по умолчанию пускать).
    #[tokio::test]
    async fn guarded_lets_in_when_blocks_unavailable() {
        let blocks = MemAbuseBlocks::default();
        blocks.fail_next(Error::UNAVAILABLE);
        let g = guarded(blocks, 3, false).await;
        g.verify(password("right")).await.expect("login");
        assert_eq!(g.verify(password("wrong")).await, Err(Error::Forbidden));
    }

    /// Хранилище блокировок недоступно, `auth.abuse.fail_closed: true` — вход запрещён (временная ошибка, учётные
    /// данные не проверяются); хранилище снова доступно — вход проверяется.
    #[tokio::test]
    async fn guarded_refuses_when_blocks_unavailable_and_fail_closed() {
        let blocks = MemAbuseBlocks::default();
        blocks.fail_next(Error::UNAVAILABLE);
        let g = guarded(blocks, 3, true).await;
        let refused = g.verify(password("right")).await;
        assert!(
            matches!(refused, Err(Error::Unavailable { .. })),
            "{refused:?}"
        );
        g.verify(password("right"))
            .await
            .expect("login after store recovered");
    }
}
