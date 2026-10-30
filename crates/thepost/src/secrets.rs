//! Секреты узла: ссылки `{ secret: "<путь>#<поле>" }` в config.yaml разрешаются через [`SecretStore`] (CTR-15).
//!
//! Клиент OpenBao/Vault — следующая волна (PLT-12). Пока хранилище в `serve` подключает вызывающий (тесты —
//! `MemSecretStore`, модуль — `Registry::set_secret_store`); без хранилища конфигурация со ссылками на секреты —
//! явная ошибка запуска «хранилище секретов не настроено», без ссылок узел работает.

use std::sync::Arc;

use async_trait::async_trait;
use tp_contracts::secrets::{Secret, SecretRef, SecretStore};
use tp_contracts::{Error, Result};
use tp_meta::pool::PoolConfig;
use tp_platform::Config;

use crate::sections::{BLOB, BUS, BlobSection, BusSection};

/// Ссылки на секреты в секциях конфигурации, которые читает приложение: `(параметр, ссылка)`.
pub fn references(cfg: &Config) -> Vec<(&'static str, SecretRef)> {
    let mut out = Vec::new();
    let mut push = |name: &'static str, r: &Option<SecretRef>| {
        if let Some(r) = r {
            out.push((name, r.clone()));
        }
    };
    if let Ok(db) = cfg.section::<PoolConfig>(PoolConfig::SECTION) {
        push("database.password", &db.password);
    }
    if let Ok(blob) = cfg.section::<BlobSection>(BLOB) {
        push("blob.s3.access_key_id", &blob.s3.access_key_id);
        push("blob.s3.secret_access_key", &blob.s3.secret_access_key);
    }
    if let Ok(bus) = cfg.section::<BusSection>(BUS) {
        push("bus.valkey.password", &bus.valkey.password);
    }
    out
}

/// Сообщение о ненастроенном хранилище секретов с перечнем параметров-ссылок.
fn not_configured(params: &[&str]) -> String {
    format!(
        "хранилище секретов не настроено (клиент OpenBao/Vault — PLT-12), а в конфигурации есть ссылки на секреты: {}",
        params.join(", ")
    )
}

/// Хранилище узла: заданное вызывающим или «не настроено». Без хранилища ссылки на секреты — `Invalid`.
pub fn store(cfg: &Config, store: Option<Arc<dyn SecretStore>>) -> Result<Arc<dyn SecretStore>> {
    if let Some(store) = store {
        return Ok(store);
    }
    let refs = references(cfg);
    if !refs.is_empty() {
        let params: Vec<&str> = refs.iter().map(|(p, _)| *p).collect();
        return Err(Error::Invalid(not_configured(&params)));
    }
    Ok(Arc::new(NoSecretStore))
}

/// Хранилище не настроено: любое чтение — `Invalid` с пояснением.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSecretStore;

#[async_trait]
impl SecretStore for NoSecretStore {
    async fn get(&self, secret: &SecretRef) -> Result<Secret> {
        Err(Error::Invalid(not_configured(&[&secret.to_string()])))
    }
}

/// Значение секрета по ссылке; отказ хранилища — ошибка с именем параметра (без значения).
pub async fn fetch(store: &dyn SecretStore, param: &str, r: &SecretRef) -> Result<Secret> {
    store.get(r).await.map_err(|e| {
        tracing::warn!(param, secret = %r, error = %e, "секрет недоступен");
        match e {
            Error::Invalid(m) => Error::Invalid(format!("{param}: {m}")),
            Error::NotFound => Error::Invalid(format!("{param}: секрет {r} не найден")),
            Error::Forbidden => Error::Invalid(format!("{param}: доступ к секрету {r} запрещён")),
            other => other,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sections::registry;

    const DEV: &str = "
domain: example.org
profile: dev
database:
  url: postgres://thepost@localhost/thepost?sslmode=disable
  password: { secret: \"thepost/db#password\" }
  tls: { allow_plaintext: true }
blob:
  url: file:///tmp/blob
";

    #[test]
    fn references_are_collected() {
        let cfg = registry()
            .expect("registry")
            .parse(DEV, Vec::new())
            .expect("config");
        let refs = references(&cfg);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].0, "database.password");
        assert_eq!(refs[0].1.to_string(), "thepost/db#password");
    }

    #[test]
    fn missing_store_with_references_is_explicit_error() {
        let cfg = registry()
            .expect("registry")
            .parse(DEV, Vec::new())
            .expect("config");
        let e = store(&cfg, None).err().expect("error").to_string();
        assert!(e.contains("хранилище секретов не настроено"), "{e}");
        assert!(e.contains("database.password"), "{e}");

        let plain = DEV.replace("  password: { secret: \"thepost/db#password\" }\n", "");
        let cfg = registry()
            .expect("registry")
            .parse(&plain, Vec::new())
            .expect("config");
        assert!(store(&cfg, None).is_ok());
    }

    #[tokio::test]
    async fn fetch_names_the_parameter() {
        let mem = tp_testkit::MemSecretStore::default();
        let r: SecretRef = "thepost/db#password".parse().expect("ref");
        let e = fetch(&mem, "database.password", &r)
            .await
            .expect_err("missing")
            .to_string();
        assert!(e.contains("database.password"), "{e}");
        mem.set(&r, "s3cr3t");
        let v = fetch(&mem, "database.password", &r).await.expect("value");
        assert_eq!(v.expose(), "s3cr3t");
    }
}
