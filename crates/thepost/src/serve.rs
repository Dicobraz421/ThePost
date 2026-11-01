//! `thepost serve`: сборка узла по config.yaml и работа до сигнала остановки (SRS 2.1, 2.7; APP-01, APP-04).
//!
//! Порядок запуска: конфигурация (мягкий режим, неизвестные поля — предупреждение) → телеметрия → хранилище
//! секретов (OpenBao/Vault по секции `secrets`; секреты конфигурации читаются при запуске и перечитывании) →
//! PostgreSQL (с ротацией динамических учётных данных) и миграции (от `database.owner`, если задан) → модули →
//! членство узла и распределитель разделов журнала → BLOB, шина → сертификаты TLS → сервисы ролей (вход, хранилище
//! писем, поиск, очередь SMTP; `services`) → монитор нагрузки узла → роли (`protocols`, `roles`) → служебный HTTP
//! (`/healthz/ready` — 200, когда готовы все роли узла). Остановка: роли проходят drain, узел покидает членство,
//! служебный HTTP и фоновые задачи останавливаются последними.
//!
//! Проверка учётной записи и ограничения ролей (seccomp, Landlock) — до рантайма tokio (`startup`, из `main`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tp_cluster::{PartitionAssignerConfig, RendezvousAssigner};
use tp_contracts::events::{EventLog, PartitionAssigner};
use tp_contracts::lease::LeaseStore;
use tp_contracts::membership::{Membership, NodeId};
use tp_contracts::role::{CancellationToken, Role, RoleService, fixed_config};
use tp_contracts::secrets::SecretStore;
use tp_contracts::{Error, Result};
use tp_meta::events::EventConsumer;
use tp_meta::{MetaConfig, PgMeta};
use tp_platform::roles::RssGauge;
use tp_platform::service_http;
use tp_platform::{
    Config, Health, LoadConfig, Metrics, NodeLoad, Reloader, RoleSpec, Roles, RolesReport,
    RuntimeConfig, ServiceHttpConfig, Telemetry, TelemetryConfig, TlsCerts, TlsConfig,
    section_watch,
};
use tp_search::SearchConfig;
use tp_smtp::dkim_sign::DkimSign;
use tp_smtp::{Mode, SmtpConfig, SmtpOutService};
use tp_sys::{NodeResources, ResourcesConfig};

use crate::infra;
use crate::modules;
use crate::protocols::{Bound, HttpRole, ImapRole, InProcessRole, IndexerRole, SmtpRole, Sockets};
use crate::roles::{Maintenance, PendingRole};
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
    /// Адрес служебного HTTP.
    pub service_addr: SocketAddr,
    /// Фактические адреса слушателей ролей.
    pub listeners: Bound,
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
    let registry = Arc::new(sections::registry()?);
    let path = opts.config.clone();
    let reloader = tokio::task::spawn_blocking(move || Reloader::start(registry, path))
        .await
        .map_err(|_| Error::internal("чтение конфигурации прервано"))??;
    let cfg = reloader.current();
    let store = secrets::store(&cfg, opts.secrets).await?;
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

    let pool = infra::connect_node_db(&cfg, store.clone()).await?;
    let meta_cfg = (*cfg.section::<MetaConfig>(MetaConfig::SECTION)?).clone();
    let migrations = infra::migration_pool(&cfg, store.as_ref(), &pool).await?;
    if meta_cfg.migrate_on_start {
        tp_meta::schema::migrate(&migrations).await?;
    }
    let events_cfg = meta_cfg.events.clone();
    let blob_cfg = cfg.section::<BlobSection>(BLOB)?;
    let meta = PgMeta::new(pool, meta_cfg);

    let ext = modules::activate(&cfg, &migrations).await?;
    let store = ext.secret_store().unwrap_or(store);

    // Членство: запись узла до чтения состава, иначе узлу не достанется ни одного раздела.
    let node = infra::node_id(&cfg)?;
    let cluster = cfg.section::<ClusterSection>(CLUSTER)?;
    let mut role_names: Vec<String> = cfg
        .roles()
        .roles
        .iter()
        .map(|r| r.as_str().to_owned())
        .collect();
    role_names.extend(ext.roles().iter().map(|r| r.name().to_owned()));
    meta.heartbeat_roles(&node, &role_names, events_cfg.node_ttl)
        .await?;
    let assigner = RendezvousAssigner::start(
        Arc::new(meta.clone()) as Arc<dyn Membership>,
        &PartitionAssignerConfig {
            poll_interval: cluster.assign_poll_interval,
        },
    )
    .await?;

    let blob = match ext.blob_store() {
        Some(blob) => blob,
        None => infra::open_blob(&cfg, store.as_ref()).await?,
    };
    let bus = infra::open_bus(&cfg, store.as_ref()).await?;
    let counters = bus.counters.clone();
    metrics.register_bus(move || infra::bus_sample(counters.snapshot()));
    let bus = bus.bus;

    // Фоновые задачи узла живут дольше ролей: служебный HTTP отвечает 503 во время drain.
    let aux = CancellationToken::new();
    let service = (*cfg.section::<ServiceHttpConfig>("service_http")?).clone();
    let tls = cfg.section::<TlsConfig>("tls")?;
    // Сертификаты нужны служебному HTTP (вне dev) и протоколам (STARTTLS, неявный TLS, HTTPS jmap и admin).
    let certs = if !service.allow_plaintext || tls.cert_file.is_file() {
        let certs = Arc::new(TlsCerts::load(&tls).await.map_err(|e| {
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

    let services = Services::build(Base {
        cfg: &cfg,
        meta: meta.clone(),
        blob: blob.clone(),
        bus: bus.clone(),
        secrets: store.clone(),
        ext: &ext,
        metrics: &metrics,
    })
    .await?;
    // Ротация ключей DKIM без перезапуска (SMTP-08).
    tokio::spawn(apply_dkim(
        reloader.subscribe(),
        services.dkim.clone(),
        aux.clone(),
    ));

    // Уровень нагрузки узла по ресурсам cgroups (PLT-15): роли читают его из `RoleCtx::load`.
    let load = NodeLoad::start(
        (*cfg.section::<LoadConfig>("load")?).clone(),
        tp_sys::watch_resources(&*cfg.section::<ResourcesConfig>("resources")?).await,
        Some(reloader.subscribe()),
        &metrics,
        aux.clone(),
    );

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
    .with_assigner(
        Arc::new(assigner) as Arc<dyn PartitionAssigner>,
        node.clone(),
    );
    let maintenance = Maintenance {
        index: Arc::new(meta.clone()),
        store: blob.clone(),
        leases,
        holder,
        events: events.clone(),
        sinks: ext.event_sinks().to_vec(),
        events_poll: cluster.events_poll_interval,
        gc: blob_cfg.gc.clone(),
        orphans: blob_cfg.orphans.clone(),
    };
    let bound = Bound::default();
    let mut setup = RoleSetup {
        cfg: &cfg,
        services: &services,
        ext: &ext,
        certs: certs.clone(),
        sockets: Sockets::new(bound.clone()),
        bound: bound.clone(),
        updates: reloader.subscribe(),
        events,
        events_poll: cluster.events_poll_interval,
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
    let listener = service_http::bind(&service)?;
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
        node.clone(),
        role_names,
        events_cfg.node_ttl,
        events_cfg.heartbeat_interval(),
        aux.clone(),
    ));
    let rss = RssGauge::register(&metrics);
    let rss_shutdown = aux.clone();
    tokio::spawn(async move { rss.run(&runtime, rss_shutdown).await });
    let telemetry = telemetry.map(Arc::new);
    let telemetry_task = telemetry
        .clone()
        .map(|t| tokio::spawn(apply_telemetry(reloader.subscribe(), t, aux.clone())));
    tokio::spawn(reloader.run(aux.clone()));

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
        service_addr,
        listeners: bound,
        task,
    })
}

/// Сборка сервисов включённых ролей ядра.
struct RoleSetup<'a> {
    cfg: &'a Config,
    services: &'a Services,
    ext: &'a tp_contracts::ext::Registry,
    certs: Option<Arc<TlsCerts>>,
    sockets: Sockets,
    bound: Bound,
    updates: tokio::sync::watch::Receiver<Arc<Config>>,
    events: EventConsumer,
    events_poll: Duration,
    maintenance: Option<Maintenance>,
}

impl RoleSetup<'_> {
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
                    Arc::new(SmtpRole::new(mode, deps, self.sockets.clone())),
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
                Arc::new(ImapRole::new(
                    self.services.imap(),
                    self.certs.clone(),
                    self.sockets.clone(),
                )),
                section(sections::IMAP)?,
            ),
            Role::Jmap => {
                let cfg = (*self.cfg.section::<tp_jmap::JmapConfig>(sections::JMAP)?).clone();
                let jmap = Arc::new(self.services.jmap(self.ext, cfg)?);
                let stop = jmap.clone();
                let service = HttpRole::new(
                    role,
                    jmap.router(),
                    http.jmap.clone(),
                    profile,
                    self.certs.clone(),
                    self.bound.clone(),
                )
                .on_stop(Arc::new(move || stop.shutdown()));
                (Arc::new(service), fixed_config(()))
            }
            Role::Admin => {
                let cfg = (*self.cfg.section::<tp_admin::AdminConfig>(sections::ADMIN)?).clone();
                let admin = Arc::new(self.services.admin(self.ext, cfg)?);
                let service = HttpRole::new(
                    role,
                    admin.router(),
                    http.admin.clone(),
                    profile,
                    self.certs.clone(),
                    self.bound.clone(),
                );
                (Arc::new(service), fixed_config(()))
            }
            Role::Search => (Arc::new(InProcessRole(role)), fixed_config(())),
            Role::Indexer => {
                let search = self.cfg.section::<SearchConfig>(sections::SEARCH)?;
                let service = IndexerRole::new(
                    self.events.clone(),
                    self.services.indexer(&search),
                    self.services.rebuilder(&search),
                    self.services.meta.clone(),
                    self.events_poll,
                    self.cfg.section::<ClusterSection>(CLUSTER)?.rebuild_scan,
                );
                (Arc::new(service), fixed_config(()))
            }
            Role::Maintenance => match self.maintenance.take() {
                Some(m) => (Arc::new(m), fixed_config(())),
                None => (Arc::new(PendingRole(role)), fixed_config(())),
            },
            _ => (Arc::new(PendingRole(role)), fixed_config(())),
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

/// Продление записи узла с периодом `period` до остановки.
async fn heartbeat(
    meta: PgMeta,
    node: NodeId,
    roles: Vec<String>,
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
        if let Err(e) = meta.heartbeat_roles(&node, &roles, ttl).await {
            tracing::warn!(error = %e, "запись узла в членстве не продлена");
        }
    }
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

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
        secrets.set(&db, password.unwrap_or_else(|| "unused".to_owned()));
        let valkey: SecretRef = "thepost/valkey#password".parse().expect("ref");
        let node = format!("app-test-{}", std::process::id());
        let blob = dir.path().join("blob");
        let path = write_config(
            dir.path(),
            &format!(
                "domain: example.org\nprofile: dev\nroles: [all]\n\
                 service_http:\n  listen: 127.0.0.1:0\n  allow_plaintext: true\n\
                 database:\n  url: \"{url}\"\n  password: {{ secret: \"{db}\" }}\n  tls: {{ allow_plaintext: true }}\n\
                 bus:\n  valkey:\n    password: {{ secret: \"{valkey}\" }}\n\
                 meta:\n  events:\n    node_ttl: 3s\n\
                 cluster:\n  node_id: {node}\n\
                 runtime:\n  drain_deadline: 5s\n\
                 blob:\n  url: file://{}\n",
                blob.display()
            ),
        );
        // Секреты конфигурации читаются все при запуске: пароля Valkey (шиной в процессе он не используется)
        // нет в хранилище — узел не стартует.
        let (o, _) = opts(path.clone(), Some(secrets.clone()));
        let e = start(o).await.expect_err("no valkey secret").to_string();
        assert!(e.contains("thepost/valkey#password"), "{e}");
        secrets.set(&valkey, "pw");
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
        let metrics = running.metrics.encode().expect("metrics");
        assert!(metrics.contains("tp_bus_connected"), "{metrics}");
        assert!(metrics.contains("tp_roles_running"), "{metrics}");

        shutdown.cancel();
        let report = running.wait().await.expect("wait");
        assert!(report.failed.is_empty(), "{report:?}");
        assert!(report.cancelled.is_empty(), "{report:?}");
    }
}
