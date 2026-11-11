//! Выбор реализаций по конфигурации: хранилище BLOB (локальный каталог / S3), шина сигналов (в процессе или
//! PostgreSQL LISTEN/NOTIFY, APP-08), пул PostgreSQL, имя узла (APP-01).

use std::sync::Arc;

use sqlx::PgPool;
use tp_blob::{LocalStore, S3Store};
use tp_cluster::{BusMetrics, LocalBus};
use tp_contracts::blob::BlobStore;
use tp_contracts::bus::EventBus;
use tp_contracts::membership::NodeId;
use tp_contracts::secrets::SecretStore;
use tp_contracts::{Error, Result};
use tp_meta::MetaConfig;
use tp_meta::failover::{self, Failover};
use tp_meta::pool::{self, PoolConfig};
use tp_meta::signals::{PgSignalBus, SignalMetrics};
use tp_platform::{BusSample, Config};

use crate::sections::{
    self, BLOB, BUS, BlobLocation, BlobSection, BusKind, BusSection, CLUSTER, ClusterSection,
};

/// Секция `database` с профилем узла.
pub fn pool_config(cfg: &Config) -> Result<PoolConfig> {
    Ok(sections::pool_config(
        &*cfg.section::<PoolConfig>(PoolConfig::SECTION)?,
        cfg.profile(),
    ))
}

/// Пул PostgreSQL для коротких команд (`doctor`, `verify`): пароль или динамические учётные данные — из
/// хранилища секретов, один раз.
pub async fn connect_db(cfg: &Config, store: &dyn SecretStore) -> Result<PgPool> {
    let db = pool_config(cfg)?;
    pool::connect(&db, store)
        .await
        .map_err(|e| context("database", e))
}

/// Пул PostgreSQL узла (`serve`) с поиском primary по `database.url` + `database.urls` и повтором при
/// переключении СУБД (META-25, `database.failover`); с `database.dynamic_credentials` аренда учётных данных
/// продлевается, при отказе — новые учётные данные для новых соединений (META-18). Primary не найден за
/// `database.failover.retry_for` — `Unavailable`. Метрики переключения — [`crate::metrics::register_failover`].
pub async fn connect_node_db(cfg: &Config, store: Arc<dyn SecretStore>) -> Result<Failover> {
    let db = pool_config(cfg)?;
    failover::connect_failover(&db, store)
        .await
        .map_err(|e| context("database", e))
}

/// Пул для миграций: от владельца схемы (`database.owner`, META-23) и/или в обход PgBouncer; иначе — основной пул.
/// Узлы с ролью PostgreSQL `thepost_app` без `database.owner` схему не меняют: им — `meta.migrate_on_start: false`.
pub async fn migration_pool(
    cfg: &Config,
    store: &dyn SecretStore,
    main: &PgPool,
) -> Result<PgPool> {
    let db = pool_config(cfg)?;
    if db.pgbouncer || db.direct_url.is_some() || db.owner.is_some() {
        let section = if db.owner.is_some() {
            "database.owner"
        } else {
            "database.direct_url"
        };
        return pool::connect_direct(&db, store)
            .await
            .map_err(|e| context(section, e));
    }
    Ok(main.clone())
}

/// Хранилище BLOB по `blob.url`; ключи S3 — из хранилища секретов (`blob.s3.credentials`).
pub async fn open_blob(cfg: &Config, store: &dyn SecretStore) -> Result<Arc<dyn BlobStore>> {
    let blob = cfg.section::<BlobSection>(BLOB)?;
    match blob.location()? {
        BlobLocation::Local(root) => {
            let store = LocalStore::open(blob.local_config(root))
                .await
                .map_err(|e| context("blob", e))?;
            Ok(Arc::new(store))
        }
        BlobLocation::S3 { bucket, prefix } => {
            let store = S3Store::connect(blob.s3_config(bucket, prefix), cfg.profile(), store)
                .await
                .map_err(|e| context("blob.s3", e))?;
            Ok(Arc::new(store))
        }
    }
}

/// Снимок метрик шины для реестра метрик платформы.
pub type BusSampler = Arc<dyn Fn() -> BusSample + Send + Sync>;

/// Шина сигналов узла и снимок её метрик (`tp_bus_*`).
pub struct Bus {
    /// Шина.
    pub bus: Arc<dyn EventBus>,
    /// Снимок метрик.
    pub sample: BusSampler,
}

impl std::fmt::Debug for Bus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bus")
            .field("metrics", &(self.sample)())
            .finish_non_exhaustive()
    }
}

/// Шина по секции `bus`: `local` — в процессе; `postgres` — LISTEN/NOTIFY: публикатор на пуле узла `db`
/// (обязателен), слушатель — прямое соединение с primary (`tp_meta::signals::connect_listener`), параметры —
/// `meta.signals`. Вызывать внутри runtime tokio.
pub async fn open_bus(
    cfg: &Config,
    db: Option<&Failover>,
    store: Arc<dyn SecretStore>,
) -> Result<Bus> {
    let section = cfg.section::<BusSection>(BUS)?;
    match section.kind {
        BusKind::Local => {
            let bus = LocalBus::new(&section.local());
            let counters = bus.counters();
            Ok(Bus {
                bus: Arc::new(bus),
                sample: Arc::new(move || bus_sample(counters.snapshot())),
            })
        }
        BusKind::Postgres => {
            let db = db.ok_or_else(|| {
                Error::invalid("bus: kind: postgres — шине нужен пул узла (только для serve)")
            })?;
            let signals = cfg
                .section::<MetaConfig>(MetaConfig::SECTION)?
                .signals
                .clone();
            let listen = tp_meta::signals::connect_listener(&pool_config(cfg)?, store)
                .await
                .map_err(|e| context("bus", e))?;
            let bus = PgSignalBus::start(db.clone(), listen, signals)
                .map_err(|e| context("meta.signals", e))?;
            let probe = bus.clone();
            Ok(Bus {
                bus: Arc::new(bus),
                sample: Arc::new(move || pg_bus_sample(probe.connected(), &probe.metrics())),
            })
        }
    }
}

/// Снимок метрик шины LISTEN/NOTIFY: отброшенные сигналы — ошибки публикации, `Resync` из-за переполнения —
/// отставания подписчиков.
pub fn pg_bus_sample(connected: bool, m: &SignalMetrics) -> BusSample {
    BusSample {
        connected,
        published: m.published,
        publish_errors: m.publish_errors + m.dropped,
        received: m.received,
        delivered: m.received,
        decode_errors: 0,
        reconnects: m.reconnects,
        lagged: m.overflows,
    }
}

/// Снимок счётчиков шины в процессе для реестра метрик платформы (PLT-10, CLU-05).
pub fn bus_sample(m: BusMetrics) -> BusSample {
    BusSample {
        connected: m.connected,
        published: m.published,
        // Шины Valkey нет (CLU-09): в процессе ошибок публикации, разбора и переподключений не бывает.
        publish_errors: 0,
        received: m.received,
        delivered: m.delivered,
        decode_errors: 0,
        reconnects: 0,
        lagged: m.lagged,
    }
}

/// Имя узла: `cluster.node_id` или имя хоста (`HOSTNAME`, `/etc/hostname`).
pub fn node_id(cfg: &Config) -> Result<NodeId> {
    let cluster = cfg.section::<ClusterSection>(CLUSTER)?;
    if let Some(name) = &cluster.node_id {
        return NodeId::new(name.clone());
    }
    let host = std::env::var("HOSTNAME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|h| h.trim().to_owned())
                .filter(|h| !h.is_empty())
        })
        .ok_or_else(|| Error::invalid("cluster.node_id не задан и имя хоста не определено"))?;
    NodeId::new(host.trim().to_owned())
}

/// Ошибка с именем секции; `Invalid` получает префикс, остальные — как есть.
pub fn context(section: &str, e: Error) -> Error {
    match e {
        Error::Invalid(m) if !m.starts_with(section) => Error::Invalid(format!("{section}: {m}")),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sections::registry;

    fn config(yaml: &str) -> Config {
        registry()
            .expect("registry")
            .parse(yaml, Vec::new())
            .expect("config")
    }

    #[tokio::test]
    async fn local_blob_and_bus() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cfg = config(&format!(
            "domain: example.org\nprofile: dev\ndatabase:\n  url: postgres://x@y/z?sslmode=disable\n  tls: {{ allow_plaintext: true }}\n\
             blob:\n  url: file://{}\ncluster:\n  node_id: node-a\n",
            dir.path().display().to_string().replace('\\', "/")
        ));
        let none = crate::secrets::NoSecretStore;
        let store = open_blob(&cfg, &none).await.expect("blob");
        store
            .put_raw("doctor/probe", bytes::Bytes::from_static(b"x"))
            .await
            .expect("put");
        store.delete_raw("doctor/probe").await.expect("delete");
        let bus = open_bus(&cfg, None, Arc::new(crate::secrets::NoSecretStore))
            .await
            .expect("bus");
        assert!((bus.sample)().connected);
        // Шина PostgreSQL без пула узла (короткие команды) — ошибка конфигурации, а не паника.
        let pg = config(&format!(
            "domain: example.org\nprofile: dev\ndatabase:\n  url: postgres://x@y/z?sslmode=disable\n  tls: {{ allow_plaintext: true }}\n\
             blob:\n  url: file://{}\nbus:\n  kind: postgres\n",
            dir.path().display().to_string().replace('\\', "/")
        ));
        let e = open_bus(&pg, None, Arc::new(crate::secrets::NoSecretStore))
            .await
            .expect_err("без пула");
        assert!(e.to_string().contains("postgres"), "{e}");
        let s = pg_bus_sample(
            true,
            &SignalMetrics {
                published: 3,
                dropped: 1,
                overflows: 2,
                ..SignalMetrics::default()
            },
        );
        assert_eq!((s.published, s.publish_errors, s.lagged), (3, 1, 2));
        assert_eq!(node_id(&cfg).expect("node").as_str(), "node-a");
    }

    /// Пул узла — `connect_failover`: ни один адрес `url` + `urls` не принимает соединений — `Unavailable` за
    /// `database.failover.retry_for`, без зависания.
    #[tokio::test]
    async fn node_db_without_primary_is_unavailable() {
        let cfg = config(
            "domain: example.org\nprofile: dev\ndatabase:\n  url: postgres://x@127.0.0.1:1/z?sslmode=disable\n  \
             urls: [postgres://x@127.0.0.1:2/z]\n  tls: { allow_plaintext: true }\n  \
             failover: { retry_initial: 10ms, retry_max: 20ms, retry_for: 100ms, probe_timeout: 200ms }\n\
             blob:\n  url: file:///tmp/b\n",
        );
        let none: Arc<dyn SecretStore> = Arc::new(crate::secrets::NoSecretStore);
        let started = std::time::Instant::now();
        let e = connect_node_db(&cfg, none).await.expect_err("no primary");
        assert!(matches!(e, Error::Unavailable { .. }), "{e}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
    }

    /// Пул узла на `connect_failover` против PostgreSQL: primary выбран (`url`), метрики в `/metrics` — без
    /// переключений.
    #[tokio::test]
    async fn node_db_failover_pool() {
        let Some(url) = tp_testkit::pg_url() else {
            eprintln!("TEST_DATABASE_URL не задан — тест пропущен");
            return;
        };
        let (url, password) = crate::testutil::split_password(&crate::testutil::dev_url(&url));
        let mem = tp_testkit::MemSecretStore::default();
        mem.set(
            &"thepost/db#password".parse().expect("ref"),
            password.unwrap_or_else(|| "unused".to_owned()),
        );
        let cfg = config(&format!(
            "domain: example.org\nprofile: dev\ndatabase:\n  url: \"{url}\"\n  urls: [\"{url}\"]\n  \
             password: {{ secret: \"thepost/db#password\" }}\n  tls: {{ allow_plaintext: true }}\n\
             blob:\n  url: file:///tmp/b\n"
        ));
        let db = connect_node_db(&cfg, Arc::new(mem)).await.expect("pool");
        let one: i32 = sqlx::query_scalar("SELECT 1")
            .fetch_one(db.pool())
            .await
            .expect("select");
        assert_eq!(one, 1);
        let m = db.metrics();
        assert_eq!((m.switches, m.primary, m.switching), (0, 0, false));
        let metrics = tp_platform::Metrics::default();
        crate::metrics::register_failover(&metrics, db.clone());
        let text = metrics.encode().expect("encode");
        for line in [
            "tp_db_failover_switches_total 0",
            "tp_db_failover_switching 0",
            "tp_db_failover_primary 0",
            "tp_db_failover_last_switch_seconds 0",
        ] {
            assert!(text.contains(line), "{line}: {text}");
        }
        db.pool().close().await;
    }
}
