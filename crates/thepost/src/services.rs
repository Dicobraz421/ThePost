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
//! (`tp_dav::ItipInbox`) — у CalDAV, JMAP и доставки узла (iMIP извне и письма-приглашения из почтового клиента,
//! `Delivery::with_itip`, права отправки — `Delivery::with_authz`); приглашения
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
use tp_contracts::ids::MessageId;
use tp_contracts::mailstore::MailStore;
use tp_contracts::membership::NodeId;
use tp_contracts::meta::{AccountScope, AccountStore, MailboxStore, MessageQuery, MessageStore};
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
    /// Шина JMAP с системным `GalChanged` в подписках учётных записей (APP-49); `None` — без каталога книги GAL
    /// нет, JMAP подписан на [`Services::bus`]. Системные сигналы читает `GalPush::run` (запускает `serve`).
    pub gal_push: Option<Arc<crate::gal::GalPush>>,
    /// Издатель токенов OIDC (`auth.oidc.issuer`); `None` — вход Bearer OIDC не настроен.
    pub oidc_issuer: Option<String>,
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
    /// Секция `sieve` на момент запуска: слушатели и лимиты сеансов ManageSieve (APP-27); её пределы разбора
    /// (`SieveConfig::limits`, те же, что при доставке) проверяют текст скрипта в ManageSieve, JMAP и Admin API.
    pub sieve: Arc<tp_sieve::SieveConfig>,
    /// Приём iTIP в календари учётных записей (APP-28): общий для CalDAV, JMAP и доставки узла.
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
                .map_err(|e| infra::context("auth.oidc", e))?
                .with_secrets(secrets.as_ref())
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

        // Очередь исходящей почты: письмо и уведомления подписываются DKIM при постановке; селектор подписи —
        // политика организации принятого домена `From` (SMTP-40).
        let smtp = cfg.section::<SmtpConfig>(SMTP)?;
        let routes = Arc::new(Routes::new(Arc::new(meta.clone())));
        let dkim = Arc::new(
            tp_smtp::dkim_sign::DkimSign::new(secrets.clone(), smtp.dkim.clone())
                .with_policies(Arc::new(meta.clone()), routes.clone()),
        );
        let queue = Arc::new(Queue::new(queue_deps(&meta, &blob)).with_dkim(dkim.clone()));
        let smtp_metrics = Arc::new(SmtpMetrics::new());
        smtp_metrics.register(metrics);
        let limiter = Arc::new(RateLimiter::new(
            (*cfg.section::<RateLimitConfig>(tp_platform::ratelimit::RATELIMIT)?).clone(),
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
        // Метка узла в `Received` (`by <smtp.hostname>`) — защита `redirect` от петли.
        let sieve = (*cfg.section::<tp_sieve::SieveConfig>(SIEVE)?).clone();
        let sieve_section = Arc::new(sieve.clone());
        let rules = Arc::new(
            tp_sieve::Rules::new(Arc::new(meta.clone()), Arc::new(meta.clone()), sieve)
                .with_node(smtp.hostname.clone()),
        );

        let gal_push = directory
            .is_some()
            .then(|| Arc::new(crate::gal::GalPush::new(bus.clone())));

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
            gal_push,
            directory,
            oidc_issuer: auth.oidc.as_ref().map(|o| o.issuer.clone()),
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
            sieve: sieve_section,
            itip,
        })
    }

    /// Доставка и обработчик транзакций SMTP (smtp-in, submission): проверки SPF/DKIM/DMARC/ARC, Milter, очередь,
    /// группы рассылки с модерацией, журнал доставки, настройки транспорта, пересылка (SRS, ARC), политики
    /// организации (профиль Milter, лимиты сеанса submission — `PgMeta`, SMTP-40), лимиты отправки в час — общие с
    /// submission и для отправки из JMAP (SMTP-43) — и правила потока почты модуля (если включён).
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
    /// Приглашения и ответы в письмах после доставки в ящик применяются к календарю получателя (`Delivery::with_itip`):
    /// извне — iMIP с DMARC (SMTP-33), из submission, JMAP и DAV — если `From` принадлежит отправителю: свой адрес,
    /// псевдоним или право Send As / Send on Behalf из ACL узла (`Delivery::with_authz`, SMTP-36, APP-62).
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
            .with_rules(self.rules.clone())
            .with_policies(Arc::new(self.meta.clone()))
            .with_send_limits(self.send_limits.clone())
            .with_itip(self.itip.clone())
            .with_authz(self.authz.clone()))
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
            audit: Arc::new(self.meta.clone()),
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
            // Push узнаёт об изменении книги GAL организации (`GalChanged`) через `GalPush`.
            bus: match &self.gal_push {
                Some(push) => push.clone() as Arc<dyn EventBus>,
                None => self.bus.clone(),
            },
            accounts: Arc::new(self.meta.clone()),
            authz: self.authz.clone(),
            outbound: Some(outbound.clone()),
            submissions: Arc::new(self.meta.clone()),
            delivery_log: Arc::new(self.meta.clone()),
            audit: Arc::new(self.meta.clone()),
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
            // Пределы разбора скриптов в `SieveScript/set` и `/validate` — те же, что у ManageSieve (APP-65).
            .with_sieve_limits(self.sieve.limits())
            // Контакты и календари (JMAP-25…29) — то же хранилище, что у CalDAV и CardDAV; занятость — JMAP-30.
            .with_dav(Some(Arc::new(self.meta.clone())))
            .with_freebusy(Some(Arc::new(self.meta.clone())))
            // Приглашения из JMAP (`sendSchedulingMessages`, JMAP-31) — теми же зависимостями, что у CalDAV.
            .with_schedule(Some(self.schedule(outbound))))
    }

    /// Маршруты календарей и контактов (CalDAV, CardDAV; APP-29): хранилище — `PgMeta`, вход — тот же, что у
    /// JMAP (с защитой от перебора). Глобальная адресная книга «Организация» читается из того же хранилища, её
    /// заполняет роль maintenance ([`Services::gal`]).
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
        let outbound: Arc<dyn Outbound> = Arc::new(self.base_delivery(smtp)?);
        // Книгу GAL маршруты читают из хранилища, заполняет её `Services::gal`.
        let router = tp_dav::router_with(deps, Some(self.schedule(outbound)), cfg);
        if !well_known {
            return Ok(router);
        }
        let context = || async { axum::response::Redirect::permanent(tp_dav::PREFIX) };
        Ok(router
            .route("/.well-known/caldav", axum::routing::any(context))
            .route("/.well-known/carddav", axum::routing::any(context)))
    }

    /// Заполнение глобальной адресной книги «Организация» (APP-49, решение 72) для роли maintenance: из каталога
    /// организации (`auth.ldap`) — учётные записи и группы, кроме скрытых из адресной книги (группы — по признаку в
    /// `PgMeta`), — в хранилище `PgMeta`, откуда её читают CardDAV и JMAP Contacts. Раз в `interval`, под лизой
    /// держателя `holder` (одна запись на организацию в кластере); имя книги и размер порции — секция `dav`.
    /// Без каталога книги нет (SRS 4.4) — `None`.
    pub fn gal(
        &self,
        cfg: tp_dav::DavConfig,
        leases: Arc<dyn tp_contracts::lease::LeaseStore>,
        holder: String,
        interval: std::time::Duration,
    ) -> Option<crate::gal::GalSync> {
        let directory = self.directory.clone()?;
        Some(crate::gal::GalSync::new(
            tp_dav::GalDeps {
                directory,
                groups: Some(Arc::new(self.meta.clone())),
            },
            Arc::new(self.meta.clone()),
            self.tenant,
            cfg,
            leases,
            holder,
            interval,
        ))
    }

    /// Admin API. Решения модерации (одобрение и отказ очереди модерации, FIX-02) — доставка узла `tp-smtp`
    /// (`impl Moderation for Delivery`, FIX-01) без правил потока почты: письмо прошло их при приёме.
    /// Журнал аудита — только в базу; в SIEM его выгружает роль maintenance ([`crate::audit_export`]).
    pub fn admin(
        &self,
        ext: &Registry,
        cfg: tp_admin::AdminConfig,
        smtp: &SmtpConfig,
    ) -> Result<tp_admin::Admin> {
        let deps = tp_admin::Deps {
            auth: self.authn.clone(),
            audit_log: Arc::new(self.meta.clone()),
            delivery: Arc::new(self.meta.clone()),
            groups: Arc::new(self.meta.clone()),
            moderation: Arc::new(self.base_delivery(smtp)?),
            modules: ext.admin_routes().to_vec(),
            accounts: Arc::new(self.meta.clone()),
            quota: Arc::new(self.meta.clone()),
            policies: Arc::new(self.meta.clone()),
            resolver: Arc::new(self.meta.clone()),
            trash: Arc::new(self.meta.clone()),
            store: self.store.clone(),
            app_passwords: self.app_passwords.clone(),
            // Соответствие Exchange (ADM-10): адресная книга, списки отправителей, аудит ящиков, остановка синхронизации.
            contacts: Arc::new(self.meta.clone()),
            org: Arc::new(self.meta.clone()),
            dynamic: Arc::new(self.meta.clone()),
            senders: Arc::new(self.meta.clone()),
            mailbox_audit: Arc::new(self.meta.clone()),
            sync: Arc::new(self.meta.clone()),
            // Издатель для входа консоли (`GET /api/v1/session`) — тот, чьи токены проверяет узел; `admin.oidc` без
            // `auth.oidc` — ошибка `Admin::new`.
            oidc_issuer: self.oidc_issuer.clone(),
        };
        let limits = self.sieve.limits();
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
pub struct Guarded {
    inner: Arc<dyn Authenticator>,
    guard: tp_auth::AbuseGuard,
    accounts: Arc<dyn AccountStore>,
    tenant: TenantId,
}

impl std::fmt::Debug for Guarded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Guarded")
            .field("tenant", &self.tenant)
            .finish_non_exhaustive()
    }
}

impl Guarded {
    /// Проверка `inner` под защитой `guard`; учётная запись попытки — по имени входа из `accounts` в `tenant`.
    pub fn new(
        inner: Arc<dyn Authenticator>,
        guard: tp_auth::AbuseGuard,
        accounts: Arc<dyn AccountStore>,
        tenant: TenantId,
    ) -> Self {
        Self {
            inner,
            guard,
            accounts,
            tenant,
        }
    }

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

    /// APP-40 (SRS 6.3): неудачи с разными несуществующими именами входа (учётной записи нет) засчитываются адресу —
    /// порог блокирует адрес, в том числе для существующей учётной записи с верным паролем; вход с другого адреса
    /// работает.
    #[tokio::test]
    async fn guarded_blocks_address_after_unknown_logins() {
        let mut g = guarded(MemAbuseBlocks::default(), 1000, false).await;
        let cfg = tp_auth::AbuseConfig {
            ip_failures: 3,
            account_failures: 1000,
            ..tp_auth::AbuseConfig::default()
        };
        g.guard = tp_auth::AbuseGuard::new(cfg, Arc::new(MemAbuseBlocks::default()));
        let peer = Some(IpAddr::from([192, 0, 2, 7]));
        let other = Some(IpAddr::from([198, 51, 100, 9]));
        for i in 0..3 {
            let unknown = Credentials::Password {
                login: format!("nobody{i}@example.org"),
                secret: "guess".into(),
            };
            assert_eq!(g.verify(unknown, peer).await, Err(Error::Forbidden));
        }
        assert_eq!(
            g.verify(password("right"), peer).await,
            Err(Error::Forbidden),
            "адрес заблокирован"
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

        use tp_contracts::ids::DomainId;
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
        let pool = crate::testutil::db_pool(&url_db).await;
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

    /// APP-56: узел с `auth.oidc` и mTLS к поставщику стартует с ключом клиента `tls.key` из хранилища секретов;
    /// секрета нет в хранилище — ошибка запуска с путём секрета.
    #[tokio::test]
    async fn oidc_client_key_from_secret_store() {
        use tp_contracts::secrets::SecretRef;

        let Some(url) = tp_testkit::pg_url() else {
            eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
            return;
        };
        let url = crate::testutil::dev_url(&url);
        let (db_name, url_db) = crate::testutil::create_database(&url).await;
        let pool = crate::testutil::db_pool(&url_db).await;
        tp_meta::schema::migrate(&pool).await.expect("migrate");
        let bus: Arc<dyn EventBus> = Arc::new(tp_cluster::LocalBus::default());
        let meta = PgMeta::new(pool.clone(), tp_meta::MetaConfig::default()).with_bus(bus.clone());
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/testdata");
        let cfg = crate::sections::registry_of(crate::modules::BUILTIN)
            .expect("registry")
            .parse(
                &format!(
                    "domain: example.org\nprofile: dev\nservice_http:\n  allow_plaintext: true\n\
                     database:\n  url: postgres://thepost@localhost/thepost?sslmode=disable\n  tls: {{ allow_plaintext: true }}\n\
                     blob:\n  url: file:///var/lib/thepost/blob\n\
                     auth:\n  oidc:\n    issuer: https://sso.example.test\n    audience: [thepost]\n    tls:\n\
                     \x20     cert_file: \"{}\"\n      key: {{ secret: \"thepost/oidc#tls_key\" }}\n",
                    dir.join("cert.pem").display()
                ),
                Vec::new(),
            )
            .expect("config");
        let ext = Registry::default();
        let metrics = tp_platform::Metrics::default();
        let store = Arc::new(tp_testkit::MemSecretStore::default());
        let build = |secrets: Arc<dyn SecretStore>| {
            Services::build(Base {
                cfg: &cfg,
                meta: meta.clone(),
                blob: Arc::new(tp_testkit::MemBlobStore::default()),
                bus: bus.clone(),
                secrets,
                ext: &ext,
                metrics: &metrics,
                node: NodeId::new("app-56").expect("node"),
            })
        };

        let e = build(store.clone()).await.expect_err("нет секрета");
        assert!(e.to_string().contains("thepost/oidc#tls_key"), "{e}");

        store.set(
            &SecretRef::new("thepost/oidc", "tls_key").expect("ref"),
            std::fs::read_to_string(dir.join("key.pem")).expect("key"),
        );
        let services = build(store).await.expect("services");
        drop((services, meta));
        crate::testutil::drop_database(&url, &db_name).await;
    }

    /// APP-62: приглашение-письмо, отправленное через доставку JMAP и DAV (`Outbound` из `base_delivery`) от имени
    /// boss учётной записью alice с правом Send on Behalf (ACL узла, `PgMeta`), — в календаре bob (`ItipInbox` узла);
    /// от имени boss без права — только письмо.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn jmap_invite_on_behalf_reaches_calendar() {
        use tp_contracts::acl::AclStore;
        use tp_contracts::auth::SendRight;
        use tp_contracts::dav::{CollectionKind, DavStore};
        use tp_contracts::ids::DomainId;
        use tp_contracts::meta::{AccountKind, AccountUpsert, SpecialUse};
        use tp_contracts::queue::SubmitStatus;
        use tp_contracts::role::CancellationToken;
        use tp_contracts::transport::{AcceptedDomain, DomainKind, TransportStore};
        use tp_smtp::deliver::Resolved;

        const ALICE: &str = "alice@corp.example";
        const BOSS: &str = "boss@corp.example";
        const BOB: &str = "bob@corp.example";
        const DAVE: &str = "dave@corp.example";

        let Some(url) = tp_testkit::pg_url() else {
            eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
            return;
        };
        let url = crate::testutil::dev_url(&url);
        let (db_name, url_db) = crate::testutil::create_database(&url).await;
        let pool = crate::testutil::db_pool(&url_db).await;
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
            node: NodeId::new("app-62").expect("node"),
        })
        .await
        .expect("services");
        let stop = CancellationToken::new();
        tokio::spawn(services.routes.clone().watch(
            bus.clone(),
            stop.clone(),
            std::time::Duration::from_millis(50),
        ));
        let t = TenantId::DEFAULT;

        // Полномочный домен и учётные записи; boss доверил alice отправку от своего имени (Send on Behalf).
        let version = TransportStore::cluster(&meta)
            .await
            .expect("cluster")
            .version;
        let domain = DomainId::new();
        TransportStore::put_domain(
            &meta,
            version,
            &AcceptedDomain {
                id: domain,
                tenant: t,
                name: "corp.example".into(),
                kind: DomainKind::Authoritative,
            },
        )
        .await
        .expect("domain");
        let mut scope = std::collections::HashMap::new();
        for login in [ALICE, BOSS, BOB, DAVE] {
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
                    kind: AccountKind::User,
                },
            )
            .await
            .expect("account");
            scope.insert(
                login,
                AccountScope {
                    tenant: t,
                    account: acc.id,
                },
            );
        }
        AclStore::set_send_delegation(&meta, scope[BOSS], ALICE, Some(SendRight::SendOnBehalf))
            .await
            .expect("delegate");

        let smtp = cfg.section::<SmtpConfig>(SMTP).expect("smtp");
        // Та же доставка, что получают JMAP (`EmailSubmission/set`) и DAV (письма iMIP).
        let outbound: Arc<dyn Outbound> =
            Arc::new(services.base_delivery(&smtp).expect("delivery"));
        let resolver = services.delivery(&smtp).expect("delivery");
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let r = resolver.resolve(BOB).await.expect("resolve");
            if matches!(r, Resolved::Local(_)) {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "домен не принят: {r:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }

        // Приглашение от имени boss (`From`), отправитель — `sender` (`Sender`, конверт — как у JMAP Send on Behalf).
        let invite = |sender: &str, uid: &str| {
            format!(
                "From: <{BOSS}>\r\nSender: <{sender}>\r\nTo: {BOB}\r\nSubject: meeting\r\nMIME-Version: 1.0\r\n\
                 Content-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\n\
                 meeting\r\n--b\r\nContent-Type: text/calendar; method=REQUEST; charset=utf-8\r\n\r\n\
                 BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//app-62//EN\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\n\
                 UID:{uid}\r\nDTSTAMP:20261112T100000Z\r\nDTSTART:20261120T100000Z\r\nDTEND:20261120T110000Z\r\n\
                 SUMMARY:meeting\r\nORGANIZER:mailto:{BOSS}\r\nATTENDEE:mailto:{BOB}\r\nEND:VEVENT\r\n\
                 END:VCALENDAR\r\n--b--\r\n"
            )
        };
        for (sender, uid) in [(ALICE, "app62-on-behalf"), (DAVE, "app62-no-right")] {
            let sent = outbound
                .submit(
                    t,
                    sender,
                    &[BOB.to_owned()],
                    bytes::Bytes::from(invite(sender, uid)),
                )
                .await
                .expect("submit");
            assert_eq!(sent.rcpts, [(BOB.to_owned(), SubmitStatus::Delivered)]);
        }

        let bob = scope[BOB];
        let in_calendar = |uid: &'static str| {
            let meta = meta.clone();
            async move {
                for c in DavStore::list_collections(&meta, bob, CollectionKind::Calendar)
                    .await
                    .expect("calendars")
                {
                    if DavStore::object_by_uid(&meta, bob, c.id, uid)
                        .await
                        .expect("object")
                        .is_some()
                    {
                        return true;
                    }
                }
                false
            }
        };
        assert!(
            in_calendar("app62-on-behalf").await,
            "приглашение с правом Send on Behalf не в календаре"
        );
        assert!(
            !in_calendar("app62-no-right").await,
            "приглашение без права — только письмо"
        );
        // Письма — во «Входящих» bob при любом исходе.
        let inbox = MailboxStore::list(&meta, bob)
            .await
            .expect("mailboxes")
            .into_iter()
            .find(|m| m.role == Some(SpecialUse::Inbox))
            .expect("inbox")
            .id;
        assert_eq!(
            MessageStore::list(&meta, bob, inbox, MessageQuery::All, 10)
                .await
                .expect("messages")
                .len(),
            2
        );

        stop.cancel();
        drop((services, outbound, resolver, meta));
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
        let pool = crate::testutil::db_pool(&url_db).await;
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
        // Вход консоли через OIDC (`admin.oidc`) без поставщика `auth.oidc` — ошибка запуска Admin API.
        let console_oidc = tp_admin::AdminConfig {
            oidc: Some(tp_admin::OidcClient {
                client_id: "thepost-console".into(),
                scope: None,
            }),
            ..admin_cfg.clone()
        };
        let e = services
            .admin(&ext, console_oidc, &smtp)
            .map(|_| ())
            .expect_err("admin.oidc без auth.oidc");
        assert!(
            matches!(&e, tp_contracts::Error::Invalid(m) if m.contains("auth.oidc")),
            "{e:?}"
        );
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
                    kind: tp_contracts::meta::AccountKind::User,
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

    /// Ответ JMAP на один вызов `method` (APP-65).
    async fn jmap_call(
        jmap: &tp_jmap::Jmap,
        p: &Principal,
        method: &str,
        args: serde_json::Value,
    ) -> serde_json::Value {
        let body = serde_json::json!({
            "using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:sieve"],
            "methodCalls": [[method, args, "c0"]],
        });
        let resp = jmap
            .api(p, body.to_string().as_bytes())
            .await
            .unwrap_or_else(|e| panic!("{method}: {e:?}"));
        let resp = serde_json::to_value(resp).expect("json");
        assert_eq!(resp["methodResponses"][0][0], method, "{resp}");
        resp["methodResponses"][0][1].clone()
    }

    /// APP-65: пределы секции `sieve` узла одни для ManageSieve и JMAP. Скрипт сверх уменьшенной вложенности
    /// отклоняет проверка ManageSieve (`Services::managesieve`), а JMAP (`Services::jmap`) — `invalidSieve` и в
    /// `SieveScript/validate`, и в `/set`; capability объявляет `max_redirects` секции.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn jmap_sieve_limits_match_managesieve() {
        use tp_contracts::ids::DomainId;
        use tp_contracts::meta::{AccountKind, AccountUpsert};
        use tp_contracts::sieve::SieveStore;
        use tp_contracts::transport::{AcceptedDomain, DomainKind, TransportStore};

        const ALICE: &str = "alice@corp.example";
        const SIEVE_CAP: &str = "urn:ietf:params:jmap:sieve";
        const DEEP: &str = "if true { if true { if true { if true { keep; } } } }\n";

        let Some(url) = tp_testkit::pg_url() else {
            eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
            return;
        };
        let url = crate::testutil::dev_url(&url);
        let (db_name, url_db) = crate::testutil::create_database(&url).await;
        let pool = crate::testutil::db_pool(&url_db).await;
        tp_meta::schema::migrate(&pool).await.expect("migrate");
        let bus: Arc<dyn EventBus> = Arc::new(tp_cluster::LocalBus::default());
        let meta = PgMeta::new(pool, tp_meta::MetaConfig::default()).with_bus(bus.clone());
        let cfg = crate::sections::registry_of(crate::modules::BUILTIN)
            .expect("registry")
            .parse(
                "domain: example.org\nprofile: dev\nservice_http:\n  allow_plaintext: true\n\
                 database:\n  url: postgres://thepost@localhost/thepost?sslmode=disable\n  tls: { allow_plaintext: true }\n\
                 blob:\n  url: file:///var/lib/thepost/blob\n\
                 sieve:\n  max_nesting: 2\n  max_redirects: 2\n",
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
            node: NodeId::new("app-65").expect("node"),
        })
        .await
        .expect("services");
        let t = TenantId::DEFAULT;

        let version = TransportStore::cluster(&meta)
            .await
            .expect("cluster")
            .version;
        let domain = DomainId::new();
        TransportStore::put_domain(
            &meta,
            version,
            &AcceptedDomain {
                id: domain,
                tenant: t,
                name: "corp.example".into(),
                kind: DomainKind::Authoritative,
            },
        )
        .await
        .expect("domain");
        let acc = AccountStore::upsert(
            &meta,
            t,
            AccountUpsert {
                domain,
                login: ALICE.into(),
                display_name: ALICE.into(),
                external_id: None,
                enabled: true,
                aliases: Vec::new(),
                kind: AccountKind::User,
            },
        )
        .await
        .expect("account");
        let scope = AccountScope {
            tenant: t,
            account: acc.id,
        };
        let p = Principal {
            tenant: t,
            account: acc.id,
            login: ALICE.into(),
            groups: BTreeSet::new(),
            scopes: None,
        };

        // ManageSieve (`CHECKSCRIPT`, `PUTSCRIPT`) проверяет текст пределами своей секции; с пределами по умолчанию
        // скрипт верен.
        let managesieve = services.managesieve(tp_contracts::tls::Profile::Dev);
        assert!(tp_sieve::check(DEEP).is_ok());
        assert!(tp_sieve::compile(DEEP.as_bytes(), &managesieve.config.limits()).is_err());

        // Скрипт уже в хранилище (как записанный до уменьшения предела): его blob — текст для JMAP.
        SieveStore::put(&meta, scope, "deep", DEEP.into())
            .await
            .expect("put");
        let smtp = cfg.section::<SmtpConfig>(SMTP).expect("smtp");
        let jmap_cfg = (*cfg.section::<tp_jmap::JmapConfig>(JMAP).expect("jmap")).clone();
        let jmap = services.jmap(&ext, jmap_cfg, &smtp, None).expect("jmap");
        let session = jmap.session(&p, "https://mail.corp.example");
        let account = session["primaryAccounts"][SIEVE_CAP]
            .as_str()
            .expect("account")
            .to_owned();
        let caps = &session["accounts"][&account]["accountCapabilities"][SIEVE_CAP];
        assert_eq!(caps["maxNumberRedirects"], 2, "{caps}");

        let got = jmap_call(
            &jmap,
            &p,
            "SieveScript/get",
            serde_json::json!({"accountId": account}),
        )
        .await;
        let blob = got["list"][0]["blobId"]
            .as_str()
            .expect("blobId")
            .to_owned();
        let v = jmap_call(
            &jmap,
            &p,
            "SieveScript/validate",
            serde_json::json!({"accountId": account, "blobId": blob}),
        )
        .await;
        assert_eq!(v["error"]["type"], "invalidSieve", "{v}");
        let r = jmap_call(
            &jmap,
            &p,
            "SieveScript/set",
            serde_json::json!({"accountId": account, "create": {"a": {"name": "copy", "blobId": blob}}}),
        )
        .await;
        assert_eq!(r["notCreated"]["a"]["type"], "invalidSieve", "{r}");

        jmap.shutdown();
        drop((services, managesieve, jmap, meta));
        crate::testutil::drop_database(&url, &db_name).await;
    }
}
