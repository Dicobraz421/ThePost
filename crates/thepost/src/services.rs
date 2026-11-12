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
//! Планирование встреч (APP-28, SRS 4.4) — одни зависимости на узел ([`Services::schedule`]): приём iTIP в календари
//! (`tp_dav::ItipInbox`) — у CalDAV, JMAP и доставки smtp-in (iMIP извне, `Delivery::with_itip`); приглашения
//! внешним участникам — письмом (`tp_dav::ImipSender`) через ту же доставку, что у JMAP; занятость — `PgMeta`.
//!
//! Организация — `TenantId::DEFAULT`: несколько организаций на узле — модуль «Мультитенантность» (SRS 1.3).

use std::net::IpAddr;
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
use tp_contracts::transport::{AcceptedDomain, DomainKind, MailFlow};
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
use crate::sections::{
    ADMIN, AUTH, AdminSection, AuthSection, DAV, JMAP, MAILSTORE, SEARCH, SIEVE, SMTP,
};

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
    /// Правила Sieve и автоответ при доставке (APP-26): скрипты и журнал автоответов — `PgMeta`, кэш программ —
    /// общий для smtp-in, submission и JMAP.
    pub rules: Arc<tp_sieve::Rules>,
    /// Лимиты проверки текста скрипта в Admin API — секция `sieve` (те же, что при доставке).
    pub sieve_limits: tp_sieve::Limits,
    /// Секция `sieve` на момент запуска: слушатели и лимиты сеансов ManageSieve (APP-27).
    pub sieve: Arc<tp_sieve::SieveConfig>,
    /// Приём iTIP в календари учётных записей (APP-28): общий для CalDAV, JMAP и доставки smtp-in.
    pub itip: Arc<tp_dav::ItipInbox>,
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
        // Приём iTIP — с настройками секции `dav` (календарь по умолчанию, входящие планирования, лимиты).
        let itip = Arc::new(tp_dav::ItipInbox::new(
            Arc::new(meta.clone()),
            (*cfg.section::<tp_dav::DavConfig>(DAV)?).clone(),
        ));
        let tracking = Tracking::new(Arc::new(meta.clone()), node);
        let routes = Arc::new(Routes::new(Arc::new(meta.clone())));
        // Метка узла в `Received` (`by <smtp.hostname>`) — защита `redirect` от петли.
        let sieve = (*cfg.section::<tp_sieve::SieveConfig>(SIEVE)?).clone();
        let sieve_limits = sieve.limits();
        let sieve_section = Arc::new(sieve.clone());
        let rules = Arc::new(
            tp_sieve::Rules::new(Arc::new(meta.clone()), Arc::new(meta.clone()), sieve)
                .with_node(smtp.hostname.clone()),
        );

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
            rules,
            sieve_limits,
            sieve: sieve_section,
            itip,
        })
    }

    /// Доставка и обработчик транзакций SMTP (smtp-in, submission): проверки SPF/DKIM/DMARC/ARC, Milter, очередь,
    /// группы рассылки с модерацией, журнал доставки, настройки транспорта, пересылка (SRS, ARC) и правила потока
    /// почты модуля (если включён).
    pub fn delivery(&self, smtp: &SmtpConfig) -> Result<Arc<Delivery>> {
        // Приглашения и ответы извне (iMIP, SMTP-33) после доставки в ящик применяются к календарю получателя.
        let delivery = self.base_delivery(smtp)?.with_itip(self.itip.clone());
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
            .with_forwarding(Arc::new(forwarding))
            .with_rules(self.rules.clone()))
    }

    /// Планирование встреч (APP-28) для CalDAV (`tp_dav::router_with`) и JMAP (`Jmap::with_schedule`): учётным
    /// записям организации сообщения iTIP кладутся прямо в календарь ([`Services::itip`]), остальным адресам —
    /// письмом iMIP через `outbound` (локальные получатели — в ящики, внешние — в очередь с подписью DKIM);
    /// занятость — из `PgMeta`.
    fn schedule(&self, outbound: Arc<dyn Outbound>) -> tp_dav::ScheduleDeps {
        tp_dav::ScheduleDeps {
            accounts: Arc::new(self.meta.clone()),
            delivery: self.itip.clone(),
            sender: Some(Arc::new(tp_dav::ImipSender::new(outbound))),
            freebusy: Arc::new(self.meta.clone()),
        }
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

    /// ManageSieve в роли imap (APP-27): вход — SASL узла (служба `sieve` задаётся сеансом `tp-sieve`; проверка —
    /// через защиту от перебора [`Guarded`], как у IMAP), скрипты — `PgMeta`.
    pub fn managesieve(
        &self,
        profile: tp_contracts::tls::Profile,
    ) -> crate::protocols::ManageSieveSetup {
        crate::protocols::ManageSieveSetup {
            deps: tp_sieve::managesieve::Deps {
                sasl: self.sasl.clone(),
                store: Arc::new(self.meta.clone()),
            },
            config: self.sieve.clone(),
            profile,
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
            outbound: Some(outbound.clone()),
            hooks: self.hooks.clone(),
            capabilities: ext.jmap_capabilities().to_vec(),
            trash: Arc::new(self.meta.clone()),
            policies: Arc::new(self.meta.clone()),
        };
        Ok(tp_jmap::Jmap::new(deps, cfg)
            .map_err(|e| infra::context("jmap", e))?
            .with_mail_flow(self.mail_flow.clone())
            .with_profile_signer(crate::profile_signer::profile_signer(certs))
            // VacationResponse и SieveScript (JMAP-23, JMAP-24): автоответ из Postman.
            .with_sieve(Some(Arc::new(self.meta.clone())))
            // Контакты и календари (JMAP-25…29) — то же хранилище, что у CalDAV и CardDAV; занятость — JMAP-30.
            .with_dav(Some(Arc::new(self.meta.clone())))
            .with_freebusy(Some(Arc::new(self.meta.clone())))
            // Приглашения из JMAP (`sendSchedulingMessages`, JMAP-31) — теми же зависимостями, что у CalDAV.
            .with_schedule(Some(self.schedule(outbound))))
    }

    /// Маршруты календарей и контактов (CalDAV, CardDAV; APP-29): хранилище — `PgMeta`, вход — тот же, что у
    /// JMAP (с защитой от перебора). С каталогом организации (`auth.ldap`) — глобальная адресная книга
    /// «Организация» (группы, скрытые из адресной книги, в неё не попадают); без каталога её нет (SRS 4.4).
    /// `well_known` — добавить перенаправления `/.well-known/caldav` и `/.well-known/carddav` на `/dav/`
    /// (RFC 6764 §5): для слушателя без JMAP, который отдаёт их сам по `jmap.autoconfig`.
    /// Планирование встреч (APP-28): приглашения, ответы и отмены рассылает сервер ([`Services::schedule`]);
    /// письма iMIP уходят через доставку SMTP с настройками `smtp`.
    pub fn dav(
        &self,
        cfg: tp_dav::DavConfig,
        smtp: &SmtpConfig,
        well_known: bool,
    ) -> Result<axum::Router> {
        let deps = tp_dav::Deps {
            auth: self.authn.clone(),
            store: Arc::new(self.meta.clone()),
        };
        let gal = self.directory.as_ref().map(|directory| tp_dav::GalDeps {
            directory: directory.clone(),
            groups: Some(Arc::new(self.meta.clone())),
        });
        let outbound: Arc<dyn Outbound> = Arc::new(self.base_delivery(smtp)?);
        let router = tp_dav::router_with(deps, gal, Some(self.schedule(outbound)), cfg);
        if !well_known {
            return Ok(router);
        }
        let context = || async { axum::response::Redirect::permanent(tp_dav::PREFIX) };
        Ok(router
            .route("/.well-known/caldav", axum::routing::any(context))
            .route("/.well-known/carddav", axum::routing::any(context)))
    }

    /// Admin API. Решения модерации (одобрение и отказ очереди модерации, FIX-02) — доставка узла `tp-smtp`
    /// (`impl Moderation for Delivery`, FIX-01) без правил потока почты: письмо прошло их при приёме.
    pub fn admin(
        &self,
        ext: &Registry,
        cfg: tp_admin::AdminConfig,
        smtp: &SmtpConfig,
    ) -> Result<tp_admin::Admin> {
        let deps = tp_admin::Deps {
            auth: self.authn.clone(),
            audit: Arc::new(tp_admin::LogAudit),
            // TODO(ADM-05): чтение журнала аудита — нет реализации `AuditLog` поверх `audit_log` (META-22).
            audit_log: None,
            delivery: Arc::new(self.meta.clone()),
            groups: Arc::new(self.meta.clone()),
            moderation: Arc::new(self.base_delivery(smtp)?),
            modules: ext.admin_routes().to_vec(),
            accounts: Arc::new(self.meta.clone()),
            quota: Arc::new(self.meta.clone()),
            policies: Arc::new(PgPolicies(self.meta.clone())),
            resolver: Arc::new(self.meta.clone()),
            domains: Arc::new(PgDomains(self.meta.clone())),
            trash: Arc::new(self.meta.clone()),
            store: self.store.clone(),
            app_passwords: self.app_passwords.clone(),
        };
        let limits = self.sieve_limits;
        let check = move |text: &str| match tp_sieve::compile(text.as_bytes(), &limits) {
            Ok(_) => Vec::new(),
            Err(diags) => diags
                .into_iter()
                .map(|d| tp_admin::ScriptError {
                    line: d.line,
                    column: d.column,
                    message: d.message,
                })
                .collect(),
        };
        Ok(tp_admin::Admin::new(deps, cfg)
            .map_err(|e| infra::context("admin", e))?
            // Правила Sieve и автоответ за пользователя (ADM-14).
            .with_sieve(Arc::new(self.meta.clone()), check))
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
/// (`Forbidden`), `success` после входа. Попытка — адрес клиента соединения (`peer`, CTR-57: счётчики адреса и
/// подсети) и учётная запись по имени входа (пароль). Заблокированная попытка —
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
    /// Попытка входа с адреса `peer` по предъявленным данным: для пароля — учётная запись по имени входа (нет или
    /// ошибка каталога — без учётной записи); токен и Kerberos — субъект известен только после проверки.
    async fn attempt(&self, credentials: &Credentials, peer: Option<IpAddr>) -> tp_auth::Attempt {
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
        tp_auth::Attempt { ip: peer, account }
    }
}

#[async_trait]
impl Authenticator for Guarded {
    async fn verify(&self, credentials: Credentials, peer: Option<IpAddr>) -> Result<Principal> {
        let attempt = self.attempt(&credentials, peer).await;
        match self.guard.check(self.tenant, &attempt).await {
            Ok(Some(_)) => return Err(Error::Forbidden),
            Ok(None) => {}
            Err(e) => return Err(e),
        }
        match self.inner.verify(credentials, peer).await {
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

/// Домены Admin API поверх принятых доменов `TransportStore` (`PgMeta`, таблица `domains`; до контракта CTR-25):
/// список — принятые домены организации, создание — полномочный домен, удаление — только без учётных записей.
/// Версия снимка кластера устарела между чтением и записью — запись повторяется по свежему снимку.
struct PgDomains(PgMeta);

/// Попыток записи при гонке версий снимка кластера.
const DOMAIN_ATTEMPTS: usize = 5;

#[async_trait]
impl tp_admin::DomainStore for PgDomains {
    async fn list(&self, tenant: TenantId) -> Result<Vec<tp_admin::Domain>> {
        let cluster = tp_contracts::transport::TransportStore::cluster(&self.0).await?;
        Ok(cluster
            .domains
            .into_iter()
            .filter(|d| d.tenant == tenant)
            .map(|d| tp_admin::Domain {
                id: d.id,
                name: d.name,
            })
            .collect())
    }

    async fn create(&self, tenant: TenantId, name: &str) -> Result<tp_admin::Domain> {
        let domain = AcceptedDomain {
            id: DomainId::new(),
            tenant,
            name: name.to_owned(),
            kind: DomainKind::Authoritative,
        };
        for _ in 0..DOMAIN_ATTEMPTS {
            let cluster = tp_contracts::transport::TransportStore::cluster(&self.0).await?;
            // Имя занято (в любой организации) — `Conflict`; иначе `Conflict` записи — устаревшая версия.
            if cluster.domain(name).is_some() {
                return Err(Error::Conflict);
            }
            match tp_contracts::transport::TransportStore::put_domain(
                &self.0,
                cluster.version,
                &domain,
            )
            .await
            {
                Ok(_) => {
                    return Ok(tp_admin::Domain {
                        id: domain.id,
                        name: domain.name,
                    });
                }
                Err(Error::Conflict) => continue,
                Err(e) => return Err(e),
            }
        }
        Err(Error::Conflict)
    }

    async fn delete(&self, tenant: TenantId, id: DomainId) -> Result<()> {
        for _ in 0..DOMAIN_ATTEMPTS {
            let cluster = tp_contracts::transport::TransportStore::cluster(&self.0).await?;
            if !cluster
                .domains
                .iter()
                .any(|d| d.id == id && d.tenant == tenant)
            {
                return Err(Error::NotFound);
            }
            match tp_contracts::transport::TransportStore::delete_domain(
                &self.0,
                cluster.version,
                id,
            )
            .await
            {
                Ok(_) => return Ok(()),
                // Устаревшая версия снимка — повтор по свежему.
                Err(Error::Conflict) => continue,
                // Внешний ключ `accounts` → `domains`: у домена есть учётные записи.
                Err(Error::Invalid(_)) => return Err(Error::Conflict),
                Err(e) => return Err(e),
            }
        }
        Err(Error::Conflict)
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
            assert_eq!(
                g.verify(password("wrong"), None).await,
                Err(Error::Forbidden)
            );
        }
        g.verify(password("right"), None).await.expect("login");
        for _ in 0..2 {
            assert_eq!(
                g.verify(password("wrong"), None).await,
                Err(Error::Forbidden)
            );
        }
        g.verify(password("right"), None)
            .await
            .expect("login after reset");
        for _ in 0..3 {
            assert_eq!(
                g.verify(password("wrong"), None).await,
                Err(Error::Forbidden)
            );
        }
        assert_eq!(
            g.verify(password("right"), None).await,
            Err(Error::Forbidden)
        );
    }

    /// CTR-57: неудачи считаются и по адресу клиента — порог блокирует адрес (и верный пароль с него), вход с
    /// другого адреса проверяется.
    #[tokio::test]
    async fn guarded_blocks_address_after_failures() {
        let mut g = guarded(MemAbuseBlocks::default(), 1000, false).await;
        let cfg = tp_auth::AbuseConfig {
            ip_failures: 2,
            account_failures: 1000,
            ..tp_auth::AbuseConfig::default()
        };
        g.guard = tp_auth::AbuseGuard::new(cfg, Arc::new(MemAbuseBlocks::default()));
        let peer = Some(IpAddr::from([192, 0, 2, 7]));
        let other = Some(IpAddr::from([198, 51, 100, 9]));
        for _ in 0..2 {
            assert_eq!(
                g.verify(password("wrong"), peer).await,
                Err(Error::Forbidden)
            );
        }
        assert_eq!(
            g.verify(password("right"), peer).await,
            Err(Error::Forbidden)
        );
        g.verify(password("right"), other)
            .await
            .expect("login from another address");
    }

    /// Хранилище блокировок недоступно, `auth.abuse.fail_closed: false` — вход проверяется (решение владельца #253,
    /// по умолчанию пускать).
    #[tokio::test]
    async fn guarded_lets_in_when_blocks_unavailable() {
        let blocks = MemAbuseBlocks::default();
        blocks.fail_next(Error::UNAVAILABLE);
        let g = guarded(blocks, 3, false).await;
        g.verify(password("right"), None).await.expect("login");
        assert_eq!(
            g.verify(password("wrong"), None).await,
            Err(Error::Forbidden)
        );
    }

    /// Хранилище блокировок недоступно, `auth.abuse.fail_closed: true` — вход запрещён (временная ошибка, учётные
    /// данные не проверяются); хранилище снова доступно — вход проверяется.
    #[tokio::test]
    async fn guarded_refuses_when_blocks_unavailable_and_fail_closed() {
        let blocks = MemAbuseBlocks::default();
        blocks.fail_next(Error::UNAVAILABLE);
        let g = guarded(blocks, 3, true).await;
        let refused = g.verify(password("right"), None).await;
        assert!(
            matches!(refused, Err(Error::Unavailable { .. })),
            "{refused:?}"
        );
        g.verify(password("right"), None)
            .await
            .expect("login after store recovered");
    }

    /// APP-18: доставка из сборки сервисов переписывает адреса по таблице организации из `TransportStore`
    /// (`PgMeta`), снимок узла ([`Services::routes`]) перечитывается по сигналу `TransportChanged` шины
    /// (`Routes::watch`, как в `serve`). Отдельная база: организация по умолчанию и принятые домены — только теста.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn delivery_rewrites_addresses_from_transport_store() {
        use std::time::Duration;

        use tp_contracts::role::CancellationToken;
        use tp_contracts::transport::{
            AcceptedDomain, AddressPattern, DomainKind, RewriteRule, TransportStore,
        };
        use tp_smtp::deliver::Resolved;

        let Some(url) = tp_testkit::pg_url() else {
            eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
            return;
        };
        let url = crate::testutil::dev_url(&url);
        let (db_name, url_db) = crate::testutil::create_database(&url).await;
        let pool = sqlx::PgPool::connect(&url_db).await.expect("pool");
        tp_meta::schema::migrate(&pool).await.expect("migrate");
        let bus: Arc<dyn EventBus> = Arc::new(tp_cluster::LocalBus::default());
        let meta = PgMeta::new(pool, tp_meta::MetaConfig::default()).with_bus(bus.clone());
        let cfg = crate::sections::registry_of(crate::modules::BUILTIN)
            .expect("registry")
            .parse(
                "domain: example.org\nprofile: dev\nservice_http:\n  allow_plaintext: true\n\
                 database:\n  url: postgres://thepost@localhost/thepost?sslmode=disable\n  tls: { allow_plaintext: true }\n\
                 blob:\n  url: file:///var/lib/thepost/blob\n",
                Vec::new(),
            )
            .expect("config");
        let ext = Registry::default();
        let metrics = tp_platform::Metrics::default();
        let services = Services::build(Base {
            cfg: &cfg,
            meta: meta.clone(),
            blob: Arc::new(tp_testkit::MemBlobStore::default()),
            bus: bus.clone(),
            secrets: Arc::new(tp_testkit::MemSecretStore::default()),
            ext: &ext,
            metrics: &metrics,
            node: NodeId::new("app-18").expect("node"),
        })
        .await
        .expect("services");
        let stop = CancellationToken::new();
        tokio::spawn(services.routes.clone().watch(
            bus.clone(),
            stop.clone(),
            Duration::from_millis(50),
        ));

        // Полномочный домен организации; `legacy.example` — не принятый (внешний).
        let version = TransportStore::cluster(&meta)
            .await
            .expect("cluster")
            .version;
        TransportStore::put_domain(
            &meta,
            version,
            &AcceptedDomain {
                id: DomainId::new(),
                tenant: TenantId::DEFAULT,
                name: "corp.example".into(),
                kind: DomainKind::Authoritative,
            },
        )
        .await
        .expect("domain");
        let smtp = cfg.section::<SmtpConfig>(SMTP).expect("smtp");
        let delivery = services.delivery(&smtp).expect("delivery");
        assert_eq!(
            delivery
                .resolve("old@legacy.example")
                .await
                .expect("resolve"),
            Resolved::External
        );
        assert_eq!(
            delivery
                .resolve("nobody@corp.example")
                .await
                .expect("resolve"),
            Resolved::Unknown
        );

        // Правило организации: получатели `@legacy.example` — в `@corp.example`; снимок уже прочитан — новое
        // правило видно после сигнала `TransportChanged`.
        let version = TransportStore::tenant(&meta, TenantId::DEFAULT)
            .await
            .expect("tenant")
            .version;
        TransportStore::put_rewrite(
            &meta,
            TenantId::DEFAULT,
            version,
            &RewriteRule {
                from: AddressPattern::new("@legacy.example").expect("from"),
                to: AddressPattern::new("@corp.example").expect("to"),
                sender: false,
                recipient: true,
            },
        )
        .await
        .expect("rewrite");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let r = delivery
                .resolve("old@legacy.example")
                .await
                .expect("resolve");
            if r == Resolved::Unknown {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "правило не применилось: {r:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        // Отправитель по правилу только для получателей не переписывается: снимок — тот же.
        let snap = services
            .routes
            .tenant(TenantId::DEFAULT)
            .await
            .expect("snapshot");
        assert_eq!(
            snap.rewrite(
                "old@legacy.example",
                tp_contracts::transport::EnvelopeSide::Recipient
            )
            .as_deref(),
            Some("old@corp.example")
        );
        assert_eq!(
            snap.rewrite(
                "old@legacy.example",
                tp_contracts::transport::EnvelopeSide::Sender
            ),
            None
        );

        stop.cancel();
        drop((services, delivery, meta));
        crate::testutil::drop_database(&url, &db_name).await;
    }

    /// FIX-02: письмо внешнего отправителя в модерируемую группу узла (`PgMeta`) → Admin API узла (`Services::admin`,
    /// сессия `admin init`): `approve` — 204 (было 500 «доставка решений ещё не подключена»), письмо у каждого
    /// участника, повтор — 404; `reject` — 204, DSN отправителю в очереди исходящей почты.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn moderation_decisions_through_admin_api() {
        use tp_contracts::groups::{DistributionGroup, GroupMember, GroupStore, SenderRestriction};
        use tp_contracts::meta::{AccountUpsert, SpecialUse};
        use tp_contracts::queue::SubmitStatus;
        use tp_contracts::transport::TransportStore;
        use tp_contracts::{GroupId, HeldId};

        let Some(url) = tp_testkit::pg_url() else {
            eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
            return;
        };
        let url = crate::testutil::dev_url(&url);
        let (db_name, url_db) = crate::testutil::create_database(&url).await;
        let pool = sqlx::PgPool::connect(&url_db).await.expect("pool");
        tp_meta::schema::migrate(&pool).await.expect("migrate");
        let bus: Arc<dyn EventBus> = Arc::new(tp_cluster::LocalBus::default());
        let meta = PgMeta::new(pool.clone(), tp_meta::MetaConfig::default()).with_bus(bus.clone());
        let cfg = crate::sections::registry_of(crate::modules::BUILTIN)
            .expect("registry")
            .parse(
                "domain: example.org\nprofile: dev\nservice_http:\n  allow_plaintext: true\n\
                 database:\n  url: postgres://thepost@localhost/thepost?sslmode=disable\n  tls: { allow_plaintext: true }\n\
                 blob:\n  url: file:///var/lib/thepost/blob\n",
                Vec::new(),
            )
            .expect("config");
        let ext = Registry::default();
        let metrics = tp_platform::Metrics::default();
        let services = Services::build(Base {
            cfg: &cfg,
            meta: meta.clone(),
            blob: Arc::new(tp_testkit::MemBlobStore::default()),
            bus: bus.clone(),
            secrets: Arc::new(tp_testkit::MemSecretStore::default()),
            ext: &ext,
            metrics: &metrics,
            node: NodeId::new("fix-02").expect("node"),
        })
        .await
        .expect("services");
        let smtp = cfg.section::<SmtpConfig>(SMTP).expect("smtp");
        let t = TenantId::DEFAULT;
        let stop = tp_contracts::role::CancellationToken::new();
        tokio::spawn(services.routes.clone().watch(
            bus.clone(),
            stop.clone(),
            std::time::Duration::from_millis(50),
        ));

        // Admin API узла с локальным входом (как роль admin в `serve`): администратор `admin init` — модератор.
        let admin_cfg = cfg.section::<AdminSection>(ADMIN).expect("admin").0.clone();
        let admin = Arc::new(crate::local_admin::attach(
            services.admin(&ext, admin_cfg, &smtp).expect("admin"),
            &services,
        ));
        let granted = admin
            .init(&tp_admin::InitRequest {
                email: "admin@example.org".into(),
                display_name: None,
                force: false,
            })
            .await
            .expect("admin init");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let base = format!("http://{}", listener.local_addr().expect("addr"));
        let router = admin.clone().router();
        let server = tokio::spawn(async move { axum::serve(listener, router).await });
        let http = reqwest::Client::new();
        let r = http
            .post(format!("{base}/api/v1/session"))
            .bearer_auth(&granted.token)
            .send()
            .await
            .expect("session");
        assert_eq!(r.status(), reqwest::StatusCode::CREATED);
        let session: serde_json::Value =
            serde_json::from_slice(&r.bytes().await.expect("body")).expect("json");
        let session = session["session"].as_str().expect("session").to_owned();

        // Группа news@ — alice и bob, модератор — администратор.
        let domain = TransportStore::cluster(&meta)
            .await
            .expect("cluster")
            .domain("example.org")
            .expect("domain")
            .id;
        let mut scopes = Vec::new();
        for login in ["alice@example.org", "bob@example.org"] {
            let acc = AccountStore::upsert(
                &meta,
                t,
                AccountUpsert {
                    domain,
                    login: login.into(),
                    display_name: login.into(),
                    external_id: None,
                    enabled: true,
                    aliases: Vec::new(),
                },
            )
            .await
            .expect("account");
            scopes.push(AccountScope {
                tenant: t,
                account: acc.id,
            });
        }
        let group = DistributionGroup {
            id: GroupId::new(),
            tenant: t,
            name: "News".into(),
            address: Some("news@example.org".into()),
            external_id: None,
            members: scopes
                .iter()
                .map(|s| GroupMember::Account(s.account))
                .collect(),
            senders: SenderRestriction::Anyone,
            moderators: vec![granted.account],
            hidden_from_gal: false,
        };
        GroupStore::put(&meta, group.clone()).await.expect("group");

        // Два письма внешнего отправителя ждут модератора (домен `admin init` — в снимке транспорта узла).
        let delivery = services.delivery(&smtp).expect("delivery");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let r = delivery
                .resolve("alice@example.org")
                .await
                .expect("resolve");
            if matches!(r, tp_smtp::deliver::Resolved::Local(_)) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "домен не принят: {r:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        for tag in ["fix02-approve", "fix02-reject"] {
            let raw = format!(
                "From: ext@remote.example\r\nTo: news@example.org\r\nSubject: {tag}\r\n\
                 Message-ID: <{tag}@remote.example>\r\n\r\nbody\r\n"
            );
            let sent = delivery
                .submit(
                    t,
                    "ext@remote.example",
                    &["news@example.org".to_owned()],
                    bytes::Bytes::from(raw),
                )
                .await
                .expect("submit");
            assert_eq!(sent.rcpts[0].1, SubmitStatus::Queued, "{sent:?}");
        }
        let held = GroupStore::held(&meta, t, Some(group.id), None, 10)
            .await
            .expect("held");
        assert_eq!(held.len(), 2, "{held:?}");
        let id_of = |tag: &str| {
            let mid = format!("{tag}@remote.example");
            held.iter()
                .find(|h| h.message_id.as_deref() == Some(mid.as_str()))
                .expect("held message")
                .id
        };
        let (approve, reject) = (id_of("fix02-approve"), id_of("fix02-reject"));
        let decide = |id: HeldId, what: &str, body: &'static str| {
            http.post(format!("{base}/api/v1/orgs/{t}/moderation/{id}/{what}"))
                .bearer_auth(&session)
                .body(body)
                .send()
        };
        let inbox_count = |scope: AccountScope| {
            let meta = meta.clone();
            async move {
                let inbox = MailboxStore::list(&meta, scope)
                    .await
                    .expect("mailboxes")
                    .into_iter()
                    .find(|m| m.role == Some(SpecialUse::Inbox))
                    .expect("inbox")
                    .id;
                MessageStore::list(&meta, scope, inbox, MessageQuery::All, 10)
                    .await
                    .expect("messages")
                    .len()
            }
        };

        // Одобрение: доставка участникам; повтор по тому же письму — 404.
        let r = decide(approve, "approve", "").await.expect("approve");
        let status = r.status();
        let body = r.text().await.unwrap_or_default();
        assert_eq!(status, reqwest::StatusCode::NO_CONTENT, "approve: {body}");
        for scope in &scopes {
            assert_eq!(inbox_count(*scope).await, 1, "{scope:?}");
        }
        let r = decide(approve, "approve", "").await.expect("approve again");
        assert_eq!(r.status(), reqwest::StatusCode::NOT_FOUND);

        // Отказ: DSN внешнему отправителю — в очереди исходящей почты, участникам ничего.
        let r = decide(reject, "reject", r#"{"reason":"off-topic"}"#)
            .await
            .expect("reject");
        let status = r.status();
        let body = r.text().await.unwrap_or_default();
        assert_eq!(status, reqwest::StatusCode::NO_CONTENT, "reject: {body}");
        for scope in &scopes {
            assert_eq!(inbox_count(*scope).await, 1, "{scope:?}");
        }
        let mut tx = tp_meta::schema::begin(&pool, t).await.expect("begin");
        let queued: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM queue_rcpts WHERE address = 'ext@remote.example'",
        )
        .fetch_one(&mut *tx)
        .await
        .expect("queue");
        drop(tx);
        assert_eq!(queued, 1, "DSN отказа не в очереди");
        assert!(
            GroupStore::held(&meta, t, Some(group.id), None, 10)
                .await
                .expect("held")
                .is_empty()
        );

        server.abort();
        stop.cancel();
        drop((services, delivery, admin, meta, pool));
        crate::testutil::drop_database(&url, &db_name).await;
    }
}
