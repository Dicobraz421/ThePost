//! `thepost serve`: сборка узла по config.yaml и работа до сигнала остановки (SRS 2.1, 2.7; APP-01, APP-04).
//!
//! Порядок запуска: конфигурация (мягкий режим, неизвестные поля — предупреждение) → телеметрия → хранилище
//! секретов (OpenBao/Vault по секции `secrets`; секреты конфигурации читаются при запуске и перечитывании) →
//! PostgreSQL (поиск primary по `database.url` + `urls`, ротация динамических учётных данных; метрики переключения —
//! в `/metrics`) и миграции (от `database.owner`, если задан) → модули → членство узла и распределитель разделов
//! журнала → BLOB, шина → запросы узел → узел (`cluster.rpc`, mTLS; слушатель — на `cluster.addr`) → ACME (при
//! `acme.enabled`: слушатель HTTP-01, первый выпуск) → сертификаты TLS (продление ACME — в фоне) → сервисы ролей
//! (вход, хранилище писем, поиск, очередь SMTP; `services`) → монитор нагрузки узла (ресурсы cgroups, заполнение
//! кэша индексов) → роли (`protocols`, `roles`; maintenance — и истечение срока
//! корзины) → служебный HTTP (`/healthz/ready` — 200, когда готовы все роли узла). Остановка: роли проходят drain,
//! узел покидает членство, служебный HTTP и фоновые задачи останавливаются последними.
//!
//! Проверка учётной записи и ограничения ролей (seccomp, Landlock) — до рантайма tokio (`startup`, из `main`).

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tp_cluster::{NodeRpc, NodeTls, PartitionAssignerConfig, RendezvousAssigner};
use tp_contracts::events::{EventLog, PartitionAssigner};
use tp_contracts::lease::LeaseStore;
use tp_contracts::membership::{Membership, NodeId, NodeInfo};
use tp_contracts::role::{CancellationToken, Role, RoleService, fixed_config};
use tp_contracts::secrets::SecretStore;
use tp_contracts::tls::InternalTls;
use tp_contracts::{Error, Result};
use tp_meta::events::EventConsumer;
use tp_meta::{MetaConfig, PgMeta};
use tp_platform::roles::RssGauge;
use tp_platform::{
    Config, DbMetrics, Health, LoadConfig, LoadInputs, Metrics, NodeLoad, PressureConfig, Reloader,
    RoleSpec, Roles, RolesReport, RuntimeConfig, ServiceHttpConfig, Telemetry, TelemetryConfig,
    TlsCerts, TlsConfig, section_watch,
};
use tp_platform::{pressure, service_http};
use tp_search::{IndexCache, SearchConfig};
use tp_smtp::dkim_sign::DkimSign;
use tp_smtp::{Mode, SmtpConfig, SmtpOutService};
use tp_sys::{NodeResources, ResourceSnapshot, ResourcesConfig};

use crate::acme;
use crate::infra;
use crate::log_reader::LogSignals;
use crate::modules;
use crate::protocols::{
    Bound, HttpRole, ImapRole, InProcessRole, IndexerRole, LogPause, SmtpRole, Sockets,
};
use crate::roles::{Maintenance, PendingRole, Purge};
use crate::secrets;
use crate::sections::{self, BLOB, BlobSection, CLUSTER, ClusterSection, HttpSection};
use crate::services::{Base, Services};

/// Параметры запуска узла.
pub struct ServeOptions {
    /// Путь к config.yaml.
    pub config: PathBuf,
    /// Хранилище секретов вместо клиента по секции `secrets` (тесты); `None` — по секции `secrets`.
    pub secrets: Option<Arc<dyn SecretStore>>,
    /// Установить глобальный подписчик `tracing` по секции `telemetry` (процесс; в тестах — нет).
    pub telemetry: bool,
    /// Сигнал остановки узла.
    pub shutdown: CancellationToken,
    /// Модули сборки (`modules::BUILTIN`; тесты — свои модули с точками расширения).
    pub modules: &'static [modules::Builtin],
}

impl std::fmt::Debug for ServeOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServeOptions")
            .field("config", &self.config)
            .field("secrets", &self.secrets.is_some())
            .field("telemetry", &self.telemetry)
            .finish_non_exhaustive()
    }
}

/// Запущенный узел.
pub struct Running {
    /// Готовность узла и ролей.
    pub health: Health,
    /// Реестр метрик.
    pub metrics: Metrics,
    /// Имя узла в членстве.
    pub node: NodeId,
    /// Членство кластера (состав живых узлов).
    pub members: Arc<dyn Membership>,
    /// Адрес служебного HTTP.
    pub service_addr: SocketAddr,
    /// Фактические адреса слушателей ролей.
    pub listeners: Bound,
    /// Запросы узел → узел (`cluster.rpc`, CLU-12); `None` — транспорт не включён (нет `cluster.addr` и
    /// сертификата узла). Узел без `cluster.addr` запросов не принимает.
    pub rpc: Option<NodeRpc>,
    task: JoinHandle<RolesReport>,
}

impl std::fmt::Debug for Running {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Running")
            .field("node", &self.node)
            .field("service_addr", &self.service_addr)
            .finish_non_exhaustive()
    }
}

impl Running {
    /// Ждёт остановки узла (сигнал `shutdown` или превышение лимита перезапусков роли).
    pub async fn wait(self) -> Result<RolesReport> {
        self.task
            .await
            .map_err(|e| Error::Internal(format!("узел: задача прервана: {e}")))
    }
}

/// Запускает узел и возвращается, когда все компоненты созданы и роли запущены.
/// Ошибка конфигурации, секретов, СУБД, BLOB или служебного HTTP — до запуска ролей.
pub async fn start(opts: ServeOptions) -> Result<Running> {
    let registry = Arc::new(sections::registry_of(opts.modules)?);
    let path = opts.config.clone();
    let reloader = tokio::task::spawn_blocking(move || Reloader::start(registry, path))
        .await
        .map_err(|_| Error::internal("чтение конфигурации прервано"))??;
    let cfg = reloader.current();
    let store = secrets::store(&cfg, opts.secrets).await?;
    // Медленное хранилище секретов (OpenBao стартует или запечатан) — ждём, а не падаем в перезапуск (APP-31).
    secrets::wait_ready(&cfg, store.as_ref()).await?;
    // Секреты конфигурации — при запуске и каждом перечитывании; аренды продлеваются (PLT-12).
    let reloader = reloader.with_secret_store(store.clone()).await?;
    let cfg = reloader.current();

    let telemetry = if opts.telemetry {
        Some(Telemetry::init(
            &*cfg.section::<TelemetryConfig>("telemetry")?,
        )?)
    } else {
        None
    };
    let metrics = telemetry
        .as_ref()
        .map(|t| t.metrics().clone())
        .unwrap_or_default();
    // Предупреждения разбора записаны до установки подписчика логов — повторяем.
    for w in cfg.warnings() {
        tracing::warn!(warning = %w, "параметр конфигурации пропущен");
    }
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        profile = ?cfg.profile(),
        roles = ?cfg.role_entries(),
        "запуск узла"
    );

    let db = infra::connect_node_db(&cfg, store.clone()).await?;
    crate::metrics::register_failover(&metrics, db.clone());
    let db_metrics = DbMetrics::register(&metrics, &*cfg.section::<TelemetryConfig>("telemetry")?);
    let pool = db.pool().clone();
    let meta_cfg = (*cfg.section::<MetaConfig>(MetaConfig::SECTION)?).clone();
    let migrations = infra::migration_pool(&cfg, store.as_ref(), &pool).await?;
    if meta_cfg.migrate_on_start {
        tp_meta::schema::migrate(&migrations).await?;
    }
    let events_cfg = meta_cfg.events.clone();
    let blob_cfg = cfg.section::<BlobSection>(BLOB)?;
    // Шина сигналов — до хранилищ: после фиксации они публикуют `LogAppended` (будит читателей журнала событий),
    // `MembersChanged`, `TransportChanged` и др. (APP-08, SRS 2.8).
    let bus = infra::open_bus(&cfg, Some(&db), store.clone()).await?;
    let sample = bus.sample.clone();
    metrics.register_bus(move || sample());
    // Метрики Д.7 (APP-34): пул узла, слушатель LISTEN, время фиксаций с NOTIFY.
    // TODO(META-55): наблюдателя ожидания соединения передать пулу `tp-meta`, когда он начнёт его принимать, —
    // до тех пор `tp_db_pool_waiting`, `tp_db_pool_wait_seconds` и `tp_db_pool_acquire_errors_total` нулевые.
    let _pool_observer = crate::metrics::register_db(&metrics, &db_metrics, &db, bus.signals);
    let bus = bus.bus;
    let meta = PgMeta::new(pool, meta_cfg).with_bus(bus.clone());

    let ext = modules::activate_of(&cfg, &migrations, opts.modules).await?;
    let store = ext.secret_store().unwrap_or(store);

    // Членство: запись узла до чтения состава, иначе узлу не достанется ни одного раздела.
    let node = infra::node_id(&cfg)?;
    let cluster = cfg.section::<ClusterSection>(CLUSTER)?;
    // Роли модулей в записи узла не объявляются (`NodeInfo::roles` — роли ядра).
    let info = node_info(node.clone(), &cfg.roles().roles, &cluster)?;
    Membership::heartbeat(&meta, &info, events_cfg.node_ttl).await?;
    let blob = match ext.blob_store() {
        Some(blob) => blob,
        None => infra::open_blob(&cfg, store.as_ref()).await?,
    };
    // Состав — по сигналу `MembersChanged` и таймеру аренды записи узла (CLU-10).
    let assigner = RendezvousAssigner::start(
        Arc::new(meta.clone()) as Arc<dyn Membership>,
        bus.clone(),
        &PartitionAssignerConfig {
            node_ttl: events_cfg.node_ttl,
            ..PartitionAssignerConfig::default()
        },
    )
    .await?;
    let assigner: Arc<dyn PartitionAssigner> = Arc::new(assigner);
    // Читатели журнала событий — по сигналам и смене состава, без опроса (APP-08).
    let signals = LogSignals {
        bus: bus.clone(),
        members: Some(assigner.members()),
        lease_ttl: events_cfg.lease_ttl,
        resubscribe: events_cfg.retry_min,
    };

    // Фоновые задачи узла живут дольше ролей: служебный HTTP отвечает 503 во время drain. Ошибка запуска
    // останавливает уже запущенные (слушатель HTTP-01, перечитывание сертификатов).
    let aux = CancellationToken::new();
    let aux_guard = aux.clone().drop_guard();
    let service = (*cfg.section::<ServiceHttpConfig>("service_http")?).clone();
    let tls = cfg.section::<TlsConfig>("tls")?;
    let bound = Bound::default();
    // Сокеты от systemd (активация сокетов) — до открытия любых слушателей.
    let sockets = Sockets::adopt(&cfg, bound.clone())?;
    // Запросы узел → узел (APP-18): после записи узла в членстве (маршрут и роли вызывающего — по составу).
    let rpc = start_rpc(
        &cluster,
        &node,
        Arc::new(meta.clone()),
        store.clone(),
        &sockets,
        aux.clone(),
    )
    .await?;
    let acme = acme::prepare(&cfg, &sockets, aux.clone()).await?;
    // Сертификаты нужны служебному HTTP (вне dev) и протоколам (STARTTLS, неявный TLS, HTTPS jmap и admin).
    let certs = if !service.allow_plaintext || tls.cert_file.is_file() {
        // Ключ по ссылке `tls.key` читается из хранилища секретов узла и перечитывается по новой версии (APP-35).
        let certs = Arc::new(TlsCerts::load_with_secrets(&tls, store.clone()).await.map_err(|e| {
            infra::context(
                "tls",
                match e {
                    Error::Invalid(m) => Error::Invalid(format!(
                        "сертификаты TLS: {m} (служебный HTTP без TLS — только profile: dev и service_http.allow_plaintext)"
                    )),
                    other => other,
                },
            )
        })?);
        tokio::spawn(certs.clone().run(aux.clone()));
        Some(certs)
    } else {
        None
    };
    if let Some(acme) = &acme {
        acme.spawn(certs.clone(), aux.clone());
    }

    let services = Services::build(Base {
        cfg: &cfg,
        meta: meta.clone(),
        blob: blob.clone(),
        bus: bus.clone(),
        secrets: store.clone(),
        ext: &ext,
        metrics: &metrics,
        node: node.clone(),
    })
    .await?;
    // Блокировки перебора других узлов — по сигналам шины (AUTH-08).
    let (abuse, abuse_bus, abuse_stop) = (services.abuse.clone(), bus.clone(), aux.clone());
    tokio::spawn(async move {
        tokio::select! {
            () = abuse_stop.cancelled() => {}
            () = abuse.listen(abuse_bus) => {}
        }
    });
    // Настройки транспорта без перезапуска (APP-09): один снимок на узел, сигналы `TransportChanged` и `Resync`.
    tokio::spawn(services.routes.clone().watch(
        bus.clone(),
        aux.clone(),
        cfg.section::<SmtpConfig>(sections::SMTP)?.queue.resubscribe,
    ));
    // Ротация ключей DKIM без перезапуска (SMTP-08).
    tokio::spawn(apply_dkim(
        reloader.subscribe(),
        services.dkim.clone(),
        aux.clone(),
    ));

    // Уровень нагрузки узла по ресурсам cgroups (PLT-15): роли читают его из `RoleCtx::load`. Снимки ресурсов —
    // по триггерам PSI (секция `pressure`, PLT-17), без них — с интервалом секции `resources`.
    let resources = pressure::watch_resources(
        &*cfg.section::<ResourcesConfig>("resources")?,
        &*cfg.section::<PressureConfig>(pressure::PRESSURE)?,
    )
    .await;
    let load = NodeLoad::start(
        (*cfg.section::<LoadConfig>("load")?).clone(),
        resources.subscribe(),
        Some(reloader.subscribe()),
        &metrics,
        aux.clone(),
    );
    // Диски кэша (APP-10): заполнение кэша индексов поиска относительно `search.cache.max_bytes`.
    tokio::spawn(feed_cache_disk(
        load.inputs(),
        services.cache.clone(),
        cfg.section::<SearchConfig>(sections::SEARCH)?
            .cache
            .max_bytes,
        resources.subscribe(),
        aux.clone(),
    ));

    let holder = format!("{node}/{}", std::process::id());
    let runtime = (*cfg.section::<RuntimeConfig>("runtime")?).clone();
    let mut roles = Roles::new(
        runtime.clone(),
        cfg.roles().clone(),
        &NodeResources::detect(),
    )
    .with_updates(reloader.subscribe())
    .with_load(load.shared());
    let leases: Arc<dyn LeaseStore> = Arc::new(meta.clone());
    let events = EventConsumer::new(
        Arc::new(meta.clone()) as Arc<dyn EventLog>,
        leases.clone(),
        holder.clone(),
        events_cfg.clone(),
    )
    .with_assigner(assigner, node.clone());
    let maintenance = Maintenance {
        index: Arc::new(meta.clone()),
        store: blob.clone(),
        leases,
        holder,
        events: events.clone(),
        sinks: ext
            .event_sinks()
            .iter()
            .cloned()
            .chain(services.spam_learner(&*cfg.section::<SmtpConfig>(sections::SMTP)?)?)
            .collect(),
        signals: signals.clone(),
        gc: blob_cfg.gc.clone(),
        orphans: blob_cfg.orphans.clone(),
        orphan_probe: crate::metrics::register_orphans(&metrics),
        trash: Some(services.mail.clone()),
        purge: Some(Purge {
            meta: meta.clone(),
            interval: cfg
                .section::<sections::MaintenanceSection>(sections::MAINTENANCE)?
                .purge_interval,
        }),
    };
    let mut setup = RoleSetup {
        cfg: &cfg,
        services: &services,
        metrics: &metrics,
        ext: &ext,
        certs: certs.clone(),
        sockets: sockets.clone(),
        updates: reloader.subscribe(),
        events,
        signals,
        maintenance: Some(maintenance),
    };
    for role in Role::ALL.iter().copied() {
        if !cfg.roles().contains(role) {
            continue;
        }
        let spec = setup.spec(role)?;
        roles.add(role, move |_limits| spec);
    }
    for role in ext.roles() {
        roles.add_module(RoleSpec {
            service: role.clone(),
            config: fixed_config(()),
        });
    }
    drop(services);

    let health = Health::default();
    sockets.warn_unused(&listen_addrs(&cfg));
    let listener = sockets.take_or(service.listen, || service_http::bind(&service))?;
    let service_addr = listener
        .local_addr()
        .map_err(|e| Error::Internal(format!("service_http: {e}")))?;
    // Проверка секции и сертификатов — до запуска ролей: ошибка не оставляет полузапущенный узел.
    service
        .check(cfg.profile())
        .map_err(|e| infra::context("service_http", e))?;
    let router = service_http::router(health.clone(), metrics.clone());
    let profile = cfg.profile();
    let http_shutdown = aux.clone();
    let http = tokio::spawn(async move {
        if let Err(e) =
            service_http::serve(listener, router, &service, profile, certs, http_shutdown).await
        {
            tracing::error!(error = %e, "служебный HTTP остановлен");
        }
    });

    tokio::spawn(heartbeat(
        meta.clone(),
        info,
        events_cfg.node_ttl,
        events_cfg.heartbeat_interval(),
        aux.clone(),
    ));
    let rss = RssGauge::register(&metrics);
    let rss_shutdown = aux.clone();
    // RSS — с каждым снимком ресурсов по PSI, без PSI — с интервалом `runtime.rss_interval`.
    tokio::spawn(async move { rss.run_on(&resources, &runtime, rss_shutdown).await });
    let telemetry = telemetry.map(Arc::new);
    let telemetry_task = telemetry
        .clone()
        .map(|t| tokio::spawn(apply_telemetry(reloader.subscribe(), t, aux.clone())));
    tokio::spawn(reloader.run(aux.clone()));

    drop(aux_guard.disarm());
    let members: Arc<dyn Membership> = Arc::new(meta.clone());
    let task = {
        let health = health.clone();
        let metrics = metrics.clone();
        let node = node.clone();
        let shutdown = opts.shutdown.clone();
        tokio::spawn(async move {
            let report = roles.run(&health, &metrics, shutdown).await;
            if let Err(e) = Membership::leave(&meta, &node).await {
                tracing::warn!(error = %e, "запись узла в членстве не удалена");
            }
            aux.cancel();
            drop(bus);
            if let Err(e) = http.await {
                tracing::warn!(error = %e, "служебный HTTP: задача прервана");
            }
            if let Some(task) = telemetry_task {
                let _ = task.await;
            }
            if let Some(t) = telemetry.and_then(|t| Arc::try_unwrap(t).ok()) {
                t.shutdown();
            }
            tracing::info!(?report, "узел остановлен");
            report
        })
    };
    Ok(Running {
        health,
        metrics,
        node,
        members,
        service_addr,
        listeners: bound,
        rpc,
        task,
    })
}

/// Сборка сервисов включённых ролей ядра.
struct RoleSetup<'a> {
    cfg: &'a Config,
    services: &'a Services,
    metrics: &'a Metrics,
    ext: &'a tp_contracts::ext::Registry,
    certs: Option<Arc<TlsCerts>>,
    sockets: Sockets,
    updates: tokio::sync::watch::Receiver<Arc<Config>>,
    events: EventConsumer,
    signals: LogSignals,
    maintenance: Option<Maintenance>,
}

impl RoleSetup<'_> {
    /// Секция `dav` (лимиты CalDAV и CardDAV); применяется при создании маршрутов.
    fn dav_config(&self) -> Result<tp_dav::DavConfig> {
        Ok((*self.cfg.section::<tp_dav::DavConfig>(sections::DAV)?).clone())
    }

    /// Сервис роли `role` и источник её секции. Ленивая инициализация (SRS 2.7): только для включённых ролей.
    fn spec(&mut self, role: Role) -> Result<RoleSpec> {
        let section = |name: &str| section_watch(self.updates.clone(), name);
        let smtp = || self.cfg.section::<SmtpConfig>(sections::SMTP);
        let http = self.cfg.section::<HttpSection>(sections::HTTP)?;
        let profile = self.cfg.profile();
        let (service, config): (Arc<dyn RoleService>, _) = match role {
            Role::SmtpIn | Role::Submission => {
                let mode = if role == Role::SmtpIn {
                    Mode::Inbound
                } else {
                    Mode::Submission
                };
                let deps = self.services.smtp_server(&*smtp()?, self.certs.clone())?;
                (
                    Arc::new(
                        SmtpRole::new(mode, deps, self.sockets.clone())
                            .with_sources(self.services.sources()),
                    ),
                    section(sections::SMTP)?,
                )
            }
            Role::SmtpOut => {
                let worker = self.services.queue_worker(&*smtp()?)?;
                (
                    Arc::new(SmtpOutService::new(Arc::new(worker))),
                    section(sections::SMTP)?,
                )
            }
            Role::Imap => (
                Arc::new(
                    ImapRole::new(
                        self.services.imap(),
                        self.services.imap_extensions(),
                        self.certs.clone(),
                        self.sockets.clone(),
                    )
                    .with_managesieve(self.services.managesieve(profile)),
                ),
                section(sections::IMAP)?,
            ),
            Role::Jmap => {
                let cfg = (*self.cfg.section::<tp_jmap::JmapConfig>(sections::JMAP)?).clone();
                let smtp = smtp()?;
                let jmap = self
                    .services
                    .jmap(self.ext, cfg, &smtp, self.certs.clone())?;
                let jmap = Arc::new(jmap);
                let stop = jmap.clone();
                // Календари и контакты без `http.dav` — на этом же слушателе (один порт с JMAP, APP-29).
                let mut router = jmap.router();
                if self.cfg.roles().contains(Role::Dav) && http.dav.is_none() {
                    router = router.merge(self.services.dav(self.dav_config()?, &smtp, false)?);
                }
                let service = HttpRole::new(
                    role,
                    router,
                    http.jmap.clone(),
                    profile,
                    self.certs.clone(),
                    self.sockets.clone(),
                )
                .on_stop(Arc::new(move || stop.shutdown()));
                (Arc::new(service), fixed_config(()))
            }
            Role::Admin => {
                let cfg = self
                    .cfg
                    .section::<sections::AdminSection>(sections::ADMIN)?
                    .0
                    .clone();
                let admin = Arc::new(crate::local_admin::attach(
                    self.services.admin(self.ext, cfg)?,
                    self.services,
                ));
                crate::local_admin::serve(admin.clone());
                let service = HttpRole::new(
                    role,
                    admin.router(),
                    http.admin.clone(),
                    profile,
                    self.certs.clone(),
                    self.sockets.clone(),
                );
                (Arc::new(service), fixed_config(()))
            }
            Role::Dav => match http.dav_listener(self.cfg.roles()) {
                // Маршруты DAV — у роли jmap этого узла (ветка `Role::Jmap`).
                (_, true) => (Arc::new(InProcessRole(role)), fixed_config(())),
                (listener, false) => {
                    let service = HttpRole::new(
                        role,
                        self.services.dav(self.dav_config()?, &*smtp()?, true)?,
                        listener.cloned(),
                        profile,
                        self.certs.clone(),
                        self.sockets.clone(),
                    );
                    (Arc::new(service), fixed_config(()))
                }
            },
            Role::Search => (Arc::new(InProcessRole(role)), fixed_config(())),
            Role::Indexer => {
                let search = self.cfg.section::<SearchConfig>(sections::SEARCH)?;
                let indexer = Arc::new(self.services.indexer(&search));
                crate::metrics::register_indexer(self.metrics, indexer.clone());
                let service = IndexerRole::new(
                    self.events.clone(),
                    indexer,
                    self.services.rebuilder(&search),
                    self.services.meta.clone(),
                    self.signals.clone(),
                    self.cfg.section::<ClusterSection>(CLUSTER)?.rebuild_scan,
                    LogPause::from(&search.indexer),
                );
                (Arc::new(service), fixed_config(()))
            }
            Role::Maintenance => match self.maintenance.take() {
                Some(m) => (Arc::new(m), fixed_config(())),
                None => (Arc::new(PendingRole(role)), fixed_config(())),
            },
        };
        Ok(RoleSpec { service, config })
    }
}

/// Новые ключи и селекторы DKIM из перечитанной секции `smtp.dkim` (ротация без перезапуска, SMTP-08).
async fn apply_dkim(
    mut rx: tokio::sync::watch::Receiver<Arc<Config>>,
    dkim: Arc<DkimSign>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            changed = rx.changed() => if changed.is_err() { return },
        }
        let section = rx.borrow_and_update().section::<SmtpConfig>(sections::SMTP);
        match section {
            Ok(smtp) => dkim.update(smtp.dkim.clone()),
            Err(e) => tracing::warn!(error = %e, "секция smtp.dkim не применена"),
        }
    }
}

/// Показатель «диски кэша» монитора нагрузки: свободно — `max_bytes` минус занятое кэшем индексов поиска на узле
/// (`IndexCache::stats`), всего — `max_bytes` (сверх него кэш вытесняет индексы). Своего таймера нет (SRS 2.8):
/// значение обновляется с каждым снимком ресурсов узла (по PSI или с интервалом секции `resources`).
async fn feed_cache_disk(
    inputs: LoadInputs,
    cache: Arc<IndexCache>,
    max_bytes: u64,
    mut resources: tokio::sync::watch::Receiver<ResourceSnapshot>,
    shutdown: CancellationToken,
) {
    loop {
        let used = cache.stats().bytes;
        inputs.set_cache_disk(max_bytes.saturating_sub(used), max_bytes);
        tokio::select! {
            () = shutdown.cancelled() => return,
            changed = resources.changed() => if changed.is_err() { return },
        }
    }
}

/// Запись узла в членстве: роли ядра, внутренний адрес и площадка из секции `cluster`.
fn node_info(id: NodeId, roles: &BTreeSet<Role>, cluster: &ClusterSection) -> Result<NodeInfo> {
    Ok(NodeInfo {
        id,
        roles: roles.clone(),
        addr: cluster.addr.clone(),
        site: cluster.site_id().map_err(|e| infra::context(CLUSTER, e))?,
    })
}

/// Транспорт запросов узел → узел (CLU-12) по секции `cluster`: mTLS внутреннего канала (`cluster.rpc.tls`,
/// сертификат называет этот узел), слушатель — на `cluster.listen`, не задан — на `cluster.addr` (сокет systemd с
/// тем же адресом или новый) до `shutdown`; другим узлам объявляется `cluster.addr`. Транспорт не включён
/// ([`ClusterSection::rpc_enabled`]) — `None`; без адреса — только исходящие запросы. Ошибка сертификатов или
/// слушателя — до запуска ролей.
///
/// Ключ сертификата узла — файл `rpc.tls.key_file` либо секрет `rpc.tls.key` из хранилища `store` (APP-36): на
/// диск он не пишется. Сертификаты перечитываются без перезапуска ([`follow_rpc_tls`]).
async fn start_rpc(
    cluster: &ClusterSection,
    node: &NodeId,
    members: Arc<dyn Membership>,
    store: Arc<dyn SecretStore>,
    sockets: &Sockets,
    shutdown: CancellationToken,
) -> Result<Option<NodeRpc>> {
    if !cluster.rpc_enabled() {
        return Ok(None);
    }
    let ctx = |e| infra::context("cluster.rpc", e);
    let tls = NodeTls::load_with_secrets(&cluster.rpc.tls, store.as_ref())
        .await
        .map_err(ctx)?;
    let rpc = NodeRpc::new(node.clone(), &cluster.rpc, tls, members).map_err(ctx)?;
    if !cluster.rpc_tls_reload_interval.is_zero() {
        tokio::spawn(follow_rpc_tls(
            rpc.clone(),
            cluster.rpc.tls.clone(),
            store,
            cluster.rpc_tls_reload_interval,
            shutdown.clone(),
        ));
    }
    let Some(listen) = cluster.listen_addr() else {
        tracing::info!("cluster.rpc: адрес узла не задан — запросы других узлов не принимаются");
        return Ok(Some(rpc));
    };
    let key = cluster.listen_key();
    let addr = tokio::net::lookup_host(listen)
        .await
        .ok()
        .and_then(|mut a| a.next())
        .ok_or_else(|| Error::Invalid(format!("{key}: адрес {listen:?} не разрешается")))?;
    let tcp = sockets.take_or(addr, || {
        tp_sys::bind_tcp(addr, &tp_sys::ListenerConfig::default())
            .map_err(|e| Error::Invalid(format!("{key} {addr}: {e}")))
    })?;
    tracing::info!(%addr, announced = %cluster.addr, "cluster.rpc: приём запросов других узлов");
    let server = rpc.clone();
    tokio::spawn(async move {
        if let Err(e) = server
            .listen(tcp, async move { shutdown.cancelled().await })
            .await
        {
            tracing::error!(error = %e, "cluster.rpc: слушатель остановлен");
        }
    });
    Ok(Some(rpc))
}

/// Отпечаток источников сертификатов межузлового канала: содержимое файлов УЦ, сертификата и ключа и значение
/// секрета ключа (`rpc.tls.key`). Недоступный источник — тоже состояние: его появление меняет отпечаток.
/// Значения в отпечаток не восстановимы и никуда не выводятся.
async fn rpc_tls_fingerprint(tls: &InternalTls, store: &dyn SecretStore) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for file in [&tls.ca_file, &tls.cert_file, &tls.key_file] {
        match file {
            Some(path) => tokio::fs::read(path).await.ok().hash(&mut hasher),
            None => None::<Vec<u8>>.hash(&mut hasher),
        }
    }
    match &tls.key {
        Some(secret) => store
            .get(secret)
            .await
            .ok()
            .map(|s| s.expose().to_owned())
            .hash(&mut hasher),
        None => None::<String>.hash(&mut hasher),
    }
    hasher.finish()
}

/// Перечитывание сертификатов межузлового канала без перезапуска (APP-36, хвост CLU-16): раз в `period`
/// сверяется отпечаток файлов и секрета ключа ([`rpc_tls_fingerprint`]); при изменении —
/// `NodeRpc::follow_tls_with_secrets` читает новую пару (ошибка — `warn`, действует прежняя). До `shutdown`.
async fn follow_rpc_tls(
    rpc: NodeRpc,
    tls: InternalTls,
    store: Arc<dyn SecretStore>,
    period: Duration,
    shutdown: CancellationToken,
) {
    let seen = rpc_tls_fingerprint(&tls, store.as_ref()).await;
    let state = (seen, tls.clone(), store.clone(), shutdown);
    let changes =
        futures::stream::unfold(state, move |(mut seen, tls, store, shutdown)| async move {
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => return None,
                    () = tokio::time::sleep(period) => {}
                }
                let now = rpc_tls_fingerprint(&tls, store.as_ref()).await;
                if now != seen {
                    seen = now;
                    return Some(((), (seen, tls, store, shutdown)));
                }
            }
        });
    rpc.follow_tls_with_secrets(&tls, store, changes).await;
}

/// Продление записи узла с периодом `period` до остановки.
async fn heartbeat(
    meta: PgMeta,
    node: NodeInfo,
    ttl: Duration,
    period: Duration,
    shutdown: CancellationToken,
) {
    let mut tick = tokio::time::interval(period.max(Duration::from_millis(1)));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            _ = tick.tick() => {}
        }
        if let Err(e) = Membership::heartbeat(&meta, &node, ttl).await {
            tracing::warn!(error = %e, "запись узла в членстве не продлена");
        }
    }
}

/// Адреса слушателей узла по конфигурации: служебный HTTP, запросы узел → узел (`cluster.listen`, не задан —
/// `cluster.addr`), HTTP-01 (`acme.enabled`) и слушатели включённых ролей (smtp-in, submission, imap и POP3, jmap,
/// dav, admin) — для проверки сокетов systemd ([`Sockets::warn_unused`]).
fn listen_addrs(cfg: &Config) -> Vec<SocketAddr> {
    let roles = cfg.roles();
    let mut out = Vec::new();
    if let Ok(s) = cfg.section::<ServiceHttpConfig>("service_http") {
        out.push(s.listen);
    }
    // Запросы узел → узел: адрес — IP (имя хоста разрешается при запуске).
    if let Ok(c) = cfg.section::<ClusterSection>(CLUSTER)
        && let Some(addr) = c.listen_addr().and_then(|a| a.parse().ok())
    {
        out.push(addr);
    }
    if let Ok(http) = cfg.section::<HttpSection>(sections::HTTP) {
        if cfg
            .section::<tp_platform::AcmeConfig>("acme")
            .is_ok_and(|a| a.enabled)
        {
            out.push(http.acme.listen);
        }
        for (role, l) in [(Role::Jmap, &http.jmap), (Role::Admin, &http.admin)] {
            if let Some(l) = l.as_ref().filter(|_| roles.contains(role)) {
                out.push(l.listen);
            }
        }
        // Роль dav: свой сокет — `http.dav` либо `http.jmap` на узле без роли jmap.
        if roles.contains(Role::Dav)
            && let (Some(l), false) = http.dav_listener(roles)
        {
            out.push(l.listen);
        }
    }
    if let Ok(smtp) = cfg.section::<SmtpConfig>(sections::SMTP) {
        for (role, eps) in [
            (Role::SmtpIn, &smtp.inbound),
            (Role::Submission, &smtp.submission),
        ] {
            if roles.contains(role) {
                out.extend(eps.iter().map(|e| e.listen));
            }
        }
    }
    if roles.contains(Role::Imap)
        && let Ok(imap) = cfg.section::<tp_imap::ImapConfig>(sections::IMAP)
    {
        out.extend(imap.listeners.iter().map(|e| e.listen));
        if imap.pop3.enabled {
            out.extend(imap.pop3.listeners.iter().map(|e| e.listen));
        }
    }
    // ManageSieve — в роли imap.
    if roles.contains(Role::Imap)
        && let Ok(sieve) = cfg.section::<tp_sieve::SieveConfig>(sections::SIEVE)
    {
        out.extend(sieve.managesieve.listen.iter().map(|e| e.listen));
    }
    out
}

/// Новый уровень логов из перечитанной секции `telemetry`.
async fn apply_telemetry(
    mut rx: tokio::sync::watch::Receiver<Arc<Config>>,
    telemetry: Arc<Telemetry>,
    shutdown: CancellationToken,
) {
    loop {
        tokio::select! {
            () = shutdown.cancelled() => return,
            changed = rx.changed() => if changed.is_err() { return },
        }
        let section = rx
            .borrow_and_update()
            .section::<TelemetryConfig>("telemetry");
        if let Ok(section) = section
            && let Err(e) = telemetry.apply(&section)
        {
            tracing::warn!(error = %e, "секция telemetry не применена");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
    use tp_contracts::secrets::SecretRef;
    use tp_testkit::MemSecretStore;

    use crate::testutil::{dev_url, split_password};

    fn write_config(dir: &Path, yaml: &str) -> PathBuf {
        let path = dir.join("config.yaml");
        std::fs::write(&path, yaml).expect("write");
        path
    }

    fn opts(
        config: PathBuf,
        secrets: Option<Arc<dyn SecretStore>>,
    ) -> (ServeOptions, CancellationToken) {
        let shutdown = CancellationToken::new();
        (
            ServeOptions {
                config,
                secrets,
                telemetry: false,
                shutdown: shutdown.clone(),
                modules: crate::modules::BUILTIN,
            },
            shutdown,
        )
    }

    /// GET по служебному HTTP: код ответа.
    async fn http_status(addr: SocketAddr, path: &str) -> u16 {
        let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
        s.write_all(
            format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes(),
        )
        .await
        .expect("write");
        let mut buf = Vec::new();
        s.read_to_end(&mut buf).await.expect("read");
        let head = String::from_utf8_lossy(&buf);
        head.split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0)
    }

    /// Первая строка ответа сервера после соединения (приветствие).
    async fn first_line(addr: SocketAddr) -> String {
        let s = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let mut line = String::new();
        tokio::io::BufReader::new(s)
            .read_line(&mut line)
            .await
            .expect("read");
        line
    }

    #[test]
    fn node_info_takes_roles_addr_and_site_from_config() {
        let id = NodeId::new("node-a").expect("node");
        let roles: BTreeSet<Role> = [Role::Imap, Role::SmtpIn].into_iter().collect();
        let info = node_info(id.clone(), &roles, &ClusterSection::default()).expect("info");
        assert_eq!(info.id, id);
        assert_eq!(info.roles, roles);
        assert_eq!(info.site.as_str(), "default");
        assert!(info.addr.is_empty());

        let cluster = ClusterSection {
            site: "dc-2".into(),
            addr: "https://10.0.0.2:7443".into(),
            ..ClusterSection::default()
        };
        let info = node_info(id.clone(), &roles, &cluster).expect("info");
        assert_eq!(info.site.as_str(), "dc-2");
        assert_eq!(info.addr, "https://10.0.0.2:7443");

        let bad = ClusterSection {
            site: "dc 2".into(),
            ..ClusterSection::default()
        };
        let e = node_info(id, &roles, &bad).expect_err("site").to_string();
        assert!(e.contains("cluster") && e.contains("site"), "{e}");
    }

    #[tokio::test]
    async fn secret_reference_without_store_is_explicit_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(
            dir.path(),
            "domain: example.org\nprofile: dev\nservice_http:\n  allow_plaintext: true\n\
             database:\n  url: postgres://thepost@127.0.0.1:1/thepost?sslmode=disable\n  password: { secret: \"thepost/db#password\" }\n  tls: { allow_plaintext: true }\n\
             blob:\n  url: file:///tmp/thepost-blob\n",
        );
        let (o, _) = opts(path, None);
        let e = start(o).await.expect_err("no secret store").to_string();
        assert!(e.contains("хранилище секретов не настроено"), "{e}");
        assert!(e.contains("thepost/db#password"), "{e}");
    }

    #[tokio::test]
    async fn invalid_config_is_rejected_before_start() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = write_config(dir.path(), "domain: example.org\n");
        let (o, _) = opts(path, None);
        let e = start(o).await.expect_err("invalid").to_string();
        assert!(e.contains("database.url") && e.contains("blob.url"), "{e}");
    }

    /// Выход Ф0: минимальная конфигурация с `roles: [all]` против PostgreSQL доходит до ready.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn minimal_config_reaches_ready() {
        let Some(url) = tp_testkit::pg_url() else {
            eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let (url, password) = split_password(&dev_url(&url));
        let secrets = Arc::new(MemSecretStore::default());
        // Без пароля в адресе (trust) пароль серверу не нужен, но ссылка всё равно разрешается.
        let db: SecretRef = "thepost/db#password".parse().expect("ref");
        let node = format!("app-test-{}", std::process::id());
        let blob = dir.path().join("blob");
        let path = write_config(
            dir.path(),
            &format!(
                "domain: example.org\nprofile: dev\nroles: [all]\n\
                 service_http:\n  listen: 127.0.0.1:0\n  allow_plaintext: true\n\
                 database:\n  url: \"{url}\"\n  password: {{ secret: \"{db}\" }}\n  tls: {{ allow_plaintext: true }}\n\
                 meta:\n  events:\n    node_ttl: 3s\n\
                 cluster:\n  node_id: {node}\n  site: dc-test\n\
                 runtime:\n  drain_deadline: 5s\n\
                 imap:\n  listeners:\n    - listen: 127.0.0.1:0\n  pop3:\n    enabled: true\n    listeners:\n      - listen: 127.0.0.1:0\n\
                 blob:\n  url: file://{}\n",
                blob.display()
            ),
        );
        // Секреты конфигурации читаются все при запуске: пароля базы нет в хранилище — узел не стартует.
        let (o, _) = opts(path.clone(), Some(secrets.clone()));
        let e = start(o).await.expect_err("no db secret").to_string();
        assert!(e.contains("thepost/db#password"), "{e}");
        secrets.set(&db, password.unwrap_or_else(|| "unused".to_owned()));
        let (o, shutdown) = opts(path, Some(secrets));
        let running = start(o).await.expect("start");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !running.health.is_ready() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "узел не готов: {:?}",
                running.health.roles_status()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let roles: Vec<String> = running
            .health
            .roles_status()
            .into_iter()
            .map(|(r, _)| r)
            .collect();
        assert_eq!(roles.len(), Role::ALL.len(), "{roles:?}");
        assert_eq!(
            http_status(running.service_addr, "/healthz/ready").await,
            200
        );
        assert_eq!(
            http_status(running.service_addr, "/healthz/live").await,
            200
        );
        // APP-13: IMAP объявляет QUOTA и ACL (расширения подключены), POP3 по секции `imap.pop3` отвечает `+OK`.
        let imap = running.listeners.get(Role::Imap.as_str());
        let greeting = first_line(imap[0]).await;
        for cap in ["QUOTA", "ACL"] {
            assert!(
                greeting.split([' ', ']']).any(|c| c == cap),
                "{cap}: {greeting}"
            );
        }
        let pop3 = running.listeners.get(crate::protocols::POP3);
        assert!(first_line(pop3[0]).await.starts_with("+OK"));
        // APP-15: запись узла в членстве — роли ядра и площадка из секции `cluster`; без `cluster.addr` адреса
        // нет и запросов других узлов узел не принимает (APP-18).
        let live = running.members.live_info().await.expect("live_info");
        let me = live
            .iter()
            .find(|n| n.id == running.node)
            .expect("узел в составе");
        assert_eq!(me.roles, Role::ALL.iter().copied().collect(), "{me:?}");
        assert_eq!(me.site.as_str(), "dc-test");
        assert!(me.addr.is_empty(), "{me:?}");
        assert!(running.rpc.is_none());
        let metrics = running.metrics.encode().expect("metrics");
        assert!(metrics.contains("tp_bus_connected"), "{metrics}");
        assert!(metrics.contains("tp_roles_running"), "{metrics}");

        shutdown.cancel();
        let report = running.wait().await.expect("wait");
        assert!(report.failed.is_empty(), "{report:?}");
        assert!(report.cancelled.is_empty(), "{report:?}");
    }

    /// TLS-соединение со слушателем, доверяя только сертификату `cert` (PEM): `true` — сервер предъявил его.
    async fn presents(addr: SocketAddr, cert: &[u8]) -> bool {
        use tokio_rustls::rustls::pki_types::pem::PemObject;
        use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};
        use tokio_rustls::rustls::{ClientConfig, RootCertStore, crypto::aws_lc_rs};
        let mut roots = RootCertStore::empty();
        roots
            .add(CertificateDer::from_pem_slice(cert).expect("pem"))
            .expect("root");
        let cfg = ClientConfig::builder_with_provider(Arc::new(aws_lc_rs::default_provider()))
            .with_safe_default_protocol_versions()
            .expect("versions")
            .with_root_certificates(roots)
            .with_no_client_auth();
        let tcp = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let name = ServerName::try_from("localhost").expect("name");
        tokio_rustls::TlsConnector::from(Arc::new(cfg))
            .connect(name, tcp)
            .await
            .is_ok()
    }

    /// APP-35: ключ TLS по ссылке `tls.key` из хранилища секретов узла (файла `key_file` нет) — слушатель с
    /// неявным TLS стартует; новая версия секрета с новой цепочкой подхватывается без перезапуска.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn tls_key_from_secret_store_and_rotation() {
        const CERT: &[u8] = include_bytes!("testdata/cert.pem");
        const KEY: &str = include_str!("testdata/key.pem");
        // Вторая пара на то же имя (`localhost`) — из тестовых данных tp-platform.
        let other = Path::new(env!("CARGO_MANIFEST_DIR")).join("../tp-platform/src/testdata");
        let cert2 = std::fs::read(other.join("cert-b.pem")).expect("cert-b");
        let key2 = std::fs::read_to_string(other.join("key-b.pem")).expect("key-b");
        let Some(url) = tp_testkit::pg_url() else {
            eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let (url, password) = split_password(&dev_url(&url));
        let secrets = Arc::new(MemSecretStore::default());
        let db: SecretRef = "thepost/db#password".parse().expect("ref");
        let key: SecretRef = "thepost/tls#key".parse().expect("ref");
        secrets.set(&db, password.unwrap_or_else(|| "unused".to_owned()));
        let cert = dir.path().join("cert.pem");
        std::fs::write(&cert, CERT).expect("cert");
        let path = write_config(
            dir.path(),
            &format!(
                "domain: example.org\nprofile: dev\nroles: [imap]\n\
                 service_http:\n  listen: 127.0.0.1:0\n  allow_plaintext: true\n\
                 database:\n  url: \"{url}\"\n  password: {{ secret: \"{db}\" }}\n  tls: {{ allow_plaintext: true }}\n\
                 meta:\n  events:\n    node_ttl: 3s\n\
                 cluster:\n  node_id: app-tls-{pid}\n\
                 runtime:\n  drain_deadline: 5s\n\
                 tls:\n  cert_file: \"{cert}\"\n  key: {{ secret: \"{key}\" }}\n  key_poll_interval: 100ms\n\
                 imap:\n  listeners:\n    - {{ listen: \"127.0.0.1:0\", implicit_tls: true }}\n\
                 blob:\n  url: file://{blob}\n",
                pid = std::process::id(),
                cert = cert.display(),
                blob = dir.path().join("blob").display(),
            ),
        );
        // Ключа в хранилище нет — узел не стартует и называет секрет.
        let (o, _) = opts(path.clone(), Some(secrets.clone()));
        let e = start(o).await.expect_err("no tls key").to_string();
        assert!(e.contains("thepost/tls#key"), "{e}");
        secrets.set(&key, KEY);
        let (o, shutdown) = opts(path, Some(secrets.clone()));
        let running = start(o).await.expect("start");
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !running.health.is_ready() {
            assert!(
                tokio::time::Instant::now() < deadline,
                "узел не готов: {:?}",
                running.health.roles_status()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let imap = running.listeners.get(Role::Imap.as_str())[0];
        assert!(presents(imap, CERT).await);
        assert!(!presents(imap, &cert2).await);

        // Новая пара: цепочка в файле, ключ — новой версией секрета; опрос секрета (`key_poll_interval`)
        // перечитывает пару, прежняя действует до успешной загрузки.
        std::fs::write(&cert, &cert2).expect("cert");
        secrets.set(&key, key2);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while !presents(imap, &cert2).await {
            assert!(
                tokio::time::Instant::now() < deadline,
                "новая версия ключа не подхвачена"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        shutdown.cancel();
        let report = running.wait().await.expect("wait");
        assert!(report.failed.is_empty(), "{report:?}");
    }

    /// Сертификаты узлов внутреннего УЦ из `tp-cluster` (CN `a`, `b`; SAN `127.0.0.1`).
    fn cluster_testdata() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../tp-cluster/src/testdata")
    }

    /// Свободный порт на 127.0.0.1 (адрес узла объявляется до запуска слушателя).
    async fn free_port() -> u16 {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        l.local_addr().expect("addr").port()
    }

    /// Проброс порта: новый порт на 127.0.0.1, соединения с ним пересылаются на `127.0.0.1:to`; счётчик —
    /// принятые соединения.
    async fn forward_port(to: u16) -> (u16, Arc<AtomicUsize>) {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = l.local_addr().expect("addr").port();
        let count = Arc::new(AtomicUsize::new(0));
        let seen = count.clone();
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = l.accept().await {
                seen.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    if let Ok(mut out) = tokio::net::TcpStream::connect(("127.0.0.1", to)).await {
                        let _ = tokio::io::copy_bidirectional(&mut inbound, &mut out).await;
                    }
                });
            }
        });
        (port, count)
    }

    struct Echo;

    #[async_trait::async_trait]
    impl tp_contracts::rpc::RpcHandler for Echo {
        async fn handle(&self, r: tp_contracts::rpc::RpcRequest) -> Result<bytes::Bytes> {
            Ok(bytes::Bytes::from(format!(
                "{}|{}|{}",
                r.caller,
                r.key,
                String::from_utf8_lossy(&r.body)
            )))
        }
    }

    /// APP-36: отпечаток источников сертификатов межузлового канала меняется при смене секрета ключа и файла и
    /// не меняется без изменений; недоступный секрет — отдельное состояние.
    #[tokio::test]
    async fn rpc_tls_fingerprint_follows_files_and_secret() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert = dir.path().join("node.pem");
        std::fs::write(&cert, "cert-1").expect("cert");
        let key: SecretRef = "thepost/node#key".parse().expect("ref");
        let store = MemSecretStore::default();
        let tls = InternalTls {
            cert_file: Some(cert.clone()),
            key: Some(key.clone()),
            ..InternalTls::default()
        };
        let missing = rpc_tls_fingerprint(&tls, &store).await;
        store.set(&key, "key-1".to_owned());
        let first = rpc_tls_fingerprint(&tls, &store).await;
        assert_ne!(missing, first);
        assert_eq!(first, rpc_tls_fingerprint(&tls, &store).await);
        store.set(&key, "key-2".to_owned());
        let rotated = rpc_tls_fingerprint(&tls, &store).await;
        assert_ne!(first, rotated);
        std::fs::write(&cert, "cert-2").expect("cert");
        assert_ne!(rotated, rpc_tls_fingerprint(&tls, &store).await);
    }

    /// APP-18: два узла в процессе над одной базой — `cluster.addr` объявлен в составе и слушается, вызов идёт по
    /// HTTPS с mTLS на узел с ролью службы, вызывающий опознан по сертификату; матрица `cluster.rpc.permissions`
    /// на принимающем узле отказывает роли без разрешения. APP-30: узел `b` слушает `cluster.listen`, а объявляет
    /// `cluster.addr` с другим портом (проброс порта) — `a` вызывает `b` по объявленному адресу через проброс;
    /// `a` без `listen` слушает свой `addr`. APP-36: ключ узла `b` — из хранилища секретов
    /// (`cluster.rpc.tls.key`), файла ключа у него нет; ключ узла `a` — в файле.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn two_nodes_call_each_other_over_rpc() {
        use tp_contracts::rpc::{Rpc, RpcError, RpcService};

        let Some(url) = tp_testkit::pg_url() else {
            eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
            return;
        };
        let url = dev_url(&url);
        let (db_name, url_db) = crate::testutil::create_database(&url).await;
        let (db_url, password) = split_password(&url_db);
        let secrets = Arc::new(MemSecretStore::default());
        let db: SecretRef = "thepost/db#password".parse().expect("ref");
        secrets.set(&db, password.unwrap_or_else(|| "unused".to_owned()));
        let dir = tempfile::tempdir().expect("tempdir");
        let certs = cluster_testdata();
        let mut nodes = Vec::new();
        // Узел `a` — maintenance (ему разрешены службы search), `b` — search. Запуск по очереди: миграции.
        let mut forwards = None;
        for (id, role) in [("a", "maintenance"), ("b", "search")] {
            let (port, listen) = if id == "b" {
                // Проброс: объявленный порт принимает пересылка на порт слушателя узла.
                let listen = free_port().await;
                let (port, count) = forward_port(listen).await;
                forwards = Some(count);
                (port, format!("  listen: 127.0.0.1:{listen}\n"))
            } else {
                (free_port().await, String::new())
            };
            let home = dir.path().join(id);
            std::fs::create_dir_all(&home).expect("dir");
            let tls = if id == "b" {
                // Ключ узла — в хранилище секретов; в каталоге узла — только УЦ и сертификат.
                for file in ["ca.pem", "node-b.pem"] {
                    std::fs::copy(certs.join(file), home.join(file)).expect("copy");
                }
                let key: SecretRef = "thepost/node-b#key".parse().expect("ref");
                secrets.set(
                    &key,
                    std::fs::read_to_string(certs.join("node-b-key.pem")).expect("key"),
                );
                format!(
                    "      ca_file: {h}/ca.pem\n      cert_file: {h}/node-b.pem\n      \
                     key: {{ secret: \"{key}\" }}\n",
                    h = home.display(),
                )
            } else {
                format!(
                    "      ca_file: {c}/ca.pem\n      cert_file: {c}/node-{id}.pem\n      \
                     key_file: {c}/node-{id}-key.pem\n",
                    c = certs.display(),
                )
            };
            let path = write_config(
                &home,
                &format!(
                    "domain: example.org\nprofile: dev\nroles: [{role}]\n\
                     service_http:\n  listen: 127.0.0.1:0\n  allow_plaintext: true\n\
                     database:\n  url: \"{db_url}\"\n  password: {{ secret: \"{db}\" }}\n  tls: {{ allow_plaintext: true }}\n\
                     cluster:\n  node_id: {id}\n  addr: https://127.0.0.1:{port}\n{listen}  rpc:\n    members_ttl: 0s\n\
                     \x20   permissions:\n      maintenance: [search/*]\n    tls:\n{tls}\
                     runtime:\n  drain_deadline: 5s\n\
                     blob:\n  url: file://{}\n",
                    home.join("blob").display(),
                ),
            );
            let (o, shutdown) = opts(path, Some(secrets.clone()));
            let running = start(o).await.expect("start");
            assert!(running.rpc.is_some(), "{id}: транспорт не запущен");
            nodes.push((running, shutdown, port));
        }
        let rpc = |i: usize| nodes[i].0.rpc.clone().expect("rpc");
        let (a, b) = (rpc(0), rpc(1));

        // Адреса узлов — в составе кластера.
        let live = nodes[0].0.members.live_info().await.expect("live_info");
        for (running, _, port) in &nodes {
            let me = live
                .iter()
                .find(|n| n.id == running.node)
                .expect("узел в составе");
            assert_eq!(me.addr, format!("https://127.0.0.1:{port}"));
        }

        let search = RpcService::new(Role::Search, "echo").expect("service");
        b.serve(search.clone(), Arc::new(Echo)).expect("serve");
        let out = a
            .call(
                &search,
                "k1",
                bytes::Bytes::from_static(b"ping"),
                Duration::from_secs(10),
            )
            .await
            .expect("call a -> b");
        assert_eq!(&out[..], b"a|k1|ping");
        // Вызов дошёл по объявленному адресу `b` — через проброс на его `cluster.listen`.
        let forwards = forwards.expect("проброс");
        assert!(forwards.load(Ordering::SeqCst) > 0, "вызов мимо проброса");

        // Роли search службы maintenance не разрешены: отказ на принимающем узле `a`.
        let maintenance = RpcService::new(Role::Maintenance, "echo").expect("service");
        a.serve(maintenance.clone(), Arc::new(Echo)).expect("serve");
        let refused = b
            .call(
                &maintenance,
                "k2",
                bytes::Bytes::new(),
                Duration::from_secs(10),
            )
            .await;
        assert_eq!(refused, Err(RpcError::Failed(Error::Forbidden)));

        // APP-36: у узла `b` закрытого ключа на диске нет — ни в конфигурации, ни в его каталоге.
        fn private_keys(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
            for entry in std::fs::read_dir(dir).expect("read_dir").flatten() {
                let path = entry.path();
                if path.is_dir() {
                    private_keys(&path, out);
                } else if std::fs::read(&path)
                    .is_ok_and(|b| String::from_utf8_lossy(&b).contains("PRIVATE KEY"))
                {
                    out.push(path);
                }
            }
        }
        let mut found = Vec::new();
        private_keys(&dir.path().join("b"), &mut found);
        assert!(found.is_empty(), "{found:?}");

        for (running, shutdown, _) in nodes {
            shutdown.cancel();
            let report = running.wait().await.expect("wait");
            assert!(report.failed.is_empty(), "{report:?}");
        }
        crate::testutil::drop_database(&url, &db_name).await;
    }
}
