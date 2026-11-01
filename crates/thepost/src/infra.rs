//! Выбор реализаций по конфигурации: хранилище BLOB (локальный каталог / S3), шина уведомлений
//! (в процессе / Valkey), пул PostgreSQL, имя узла (APP-01).

use std::sync::Arc;

use sqlx::PgPool;
use tp_blob::{LocalStore, S3Store};
use tp_cluster::{BusCounters, BusMetrics, LocalBus, ValkeyBus};
use tp_contracts::blob::BlobStore;
use tp_contracts::bus::EventBus;
use tp_contracts::membership::NodeId;
use tp_contracts::secrets::SecretStore;
use tp_contracts::{Error, Result};
use tp_meta::pool::{self, PoolConfig};
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

/// Пул PostgreSQL узла (`serve`): с `database.dynamic_credentials` аренда учётных данных продлевается, при
/// отказе — новые учётные данные для новых соединений (META-18).
pub async fn connect_node_db(cfg: &Config, store: Arc<dyn SecretStore>) -> Result<PgPool> {
    let db = pool_config(cfg)?;
    pool::connect_rotating(&db, store)
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

/// Шина уведомлений узла и её счётчики для метрик.
pub struct Bus {
    /// Шина.
    pub bus: Arc<dyn EventBus>,
    /// Счётчики (метрики `tp_bus_*`).
    pub counters: Arc<BusCounters>,
}

impl std::fmt::Debug for Bus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bus")
            .field("metrics", &self.counters.snapshot())
            .finish_non_exhaustive()
    }
}

/// Шина по секции `bus`. Вызывать внутри runtime tokio.
pub async fn open_bus(cfg: &Config, store: &dyn SecretStore) -> Result<Bus> {
    let section = cfg.section::<BusSection>(BUS)?;
    match section.kind {
        BusKind::Local => {
            let bus = LocalBus::new(&section.local());
            let counters = bus.counters();
            Ok(Bus {
                bus: Arc::new(bus),
                counters,
            })
        }
        BusKind::Valkey => {
            let bus = ValkeyBus::connect(section.valkey.clone(), cfg.profile(), store)
                .await
                .map_err(|e| context("bus.valkey", e))?;
            let counters = bus.counters();
            Ok(Bus {
                bus: Arc::new(bus),
                counters,
            })
        }
    }
}

/// Снимок счётчиков шины для реестра метрик платформы (PLT-10, CLU-05).
pub fn bus_sample(m: BusMetrics) -> BusSample {
    BusSample {
        connected: m.connected,
        published: m.published,
        publish_errors: m.publish_errors,
        received: m.received,
        delivered: m.delivered,
        decode_errors: m.decode_errors,
        reconnects: m.reconnects,
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
        let bus = open_bus(&cfg, &none).await.expect("bus");
        assert!(bus.counters.snapshot().connected);
        assert!(bus_sample(bus.counters.snapshot()).connected);
        assert_eq!(node_id(&cfg).expect("node").as_str(), "node-a");
    }

    #[tokio::test]
    async fn valkey_password_from_secret_store() {
        let cfg = config(
            "domain: example.org\nprofile: dev\ndatabase:\n  url: postgres://x@y/z?sslmode=disable\n  tls: { allow_plaintext: true }\n\
             blob:\n  url: file:///tmp/b\nbus:\n  kind: valkey\n  valkey:\n    url: redis://127.0.0.1:1\n    password: { secret: \"thepost/valkey#password\" }\n    tls: { allow_plaintext: true }\n",
        );
        let mem = tp_testkit::MemSecretStore::default();
        let e = open_bus(&cfg, &mem).await.expect_err("no secret");
        assert!(e.is_retryable(), "{e}");
        mem.set(&"thepost/valkey#password".parse().expect("ref"), "pw");
        // Шина стартует (подключение — в фоне, адрес недоступен — переподключения).
        let bus = open_bus(&cfg, &mem).await.expect("bus");
        assert!(!bus.counters.snapshot().connected || bus.counters.snapshot().published == 0);
    }
}
