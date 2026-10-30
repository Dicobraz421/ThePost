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

use crate::secrets;
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

/// Пул PostgreSQL узла (пароль — из хранилища секретов).
pub async fn connect_db(cfg: &Config, store: &dyn SecretStore) -> Result<PgPool> {
    let db = pool_config(cfg)?;
    pool::connect(&db, store)
        .await
        .map_err(|e| context("database", e))
}

/// Пул для миграций: в обход PgBouncer, если он используется; иначе — основной пул.
pub async fn migration_pool(
    cfg: &Config,
    store: &dyn SecretStore,
    main: &PgPool,
) -> Result<PgPool> {
    let db = pool_config(cfg)?;
    if db.pgbouncer || db.direct_url.is_some() {
        return pool::connect_direct(&db, store)
            .await
            .map_err(|e| context("database.direct_url", e));
    }
    Ok(main.clone())
}

/// Хранилище BLOB по `blob.url`.
pub async fn open_blob(cfg: &Config, store: &dyn SecretStore) -> Result<Arc<dyn BlobStore>> {
    let blob = cfg.section::<BlobSection>(BLOB)?;
    match blob.location()? {
        BlobLocation::Local(root) => {
            let store = LocalStore::open(blob.local.config(root))
                .await
                .map_err(|e| context("blob", e))?;
            Ok(Arc::new(store))
        }
        BlobLocation::S3 { bucket, prefix } => {
            let keys = match (&blob.s3.access_key_id, &blob.s3.secret_access_key) {
                (Some(id), Some(key)) => {
                    let id = secrets::fetch(store, "blob.s3.access_key_id", id).await?;
                    let key = secrets::fetch(store, "blob.s3.secret_access_key", key).await?;
                    Some((id.expose().to_owned(), key.expose().to_owned()))
                }
                (None, None) => None,
                _ => {
                    return Err(Error::invalid(
                        "blob.s3: access_key_id и secret_access_key задаются вместе",
                    ));
                }
            };
            let s3 = blob.s3.config(bucket, prefix, keys);
            let profile = cfg.profile();
            // Чтение CA из файла — блокирующий I/O.
            let store = tokio::task::spawn_blocking(move || S3Store::new(s3, profile))
                .await
                .map_err(|_| Error::internal("blob: задача создания клиента S3 прервана"))?
                .map_err(|e| context("blob", e))?;
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
            let password = match &section.valkey.password {
                Some(r) => Some(secrets::fetch(store, "bus.valkey.password", r).await?),
                None => None,
            };
            let config = section
                .valkey
                .config(password.as_ref().map(tp_contracts::secrets::Secret::expose));
            let bus = ValkeyBus::start(config, cfg.profile()).map_err(|e| context("bus", e))?;
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
        let none = secrets::NoSecretStore;
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
        let e = open_bus(&cfg, &mem)
            .await
            .expect_err("no secret")
            .to_string();
        assert!(e.contains("bus.valkey.password"), "{e}");
        mem.set(&"thepost/valkey#password".parse().expect("ref"), "pw");
        // Шина стартует (подключение — в фоне, адрес недоступен — переподключения).
        let bus = open_bus(&cfg, &mem).await.expect("bus");
        assert!(!bus.counters.snapshot().connected || bus.counters.snapshot().published == 0);
    }
}
