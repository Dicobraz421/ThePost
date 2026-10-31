//! Хранилище секретов узла: ссылки `{ secret: "<путь>#<поле>" }` в config.yaml разрешаются через [`SecretStore`]
//! (CTR-15; решение 2026-10-30 «Секреты — только через хранилище секретов»).
//!
//! Хранилище — клиент OpenBao/Vault [`VaultStore`] по секции `secrets` (PLT-12); вызывающий может подставить своё
//! (тесты — `MemSecretStore`). Без `secrets.url` конфигурация со ссылками на секреты — явная ошибка запуска
//! «хранилище секретов не настроено», без ссылок узел работает.

use std::sync::Arc;

use async_trait::async_trait;
use tp_contracts::secrets::{Secret, SecretRef, SecretStore};
use tp_contracts::{Error, Result};
use tp_platform::secrets::DoctorReport;
use tp_platform::{Config, SecretsConfig, VaultStore};

/// Имя секции хранилища секретов (регистрирует платформа).
pub const SECRETS: &str = "secrets";

/// Ссылки на секреты во всех секциях конфигурации, без повторов.
pub fn references(cfg: &Config) -> Vec<String> {
    cfg.secret_refs().map(ToString::to_string).collect()
}

/// Сообщение о ненастроенном хранилище секретов с перечнем ссылок.
fn not_configured(refs: &[String]) -> String {
    format!(
        "хранилище секретов не настроено (задайте секцию secrets: url и способ входа), \
         а в конфигурации есть ссылки на секреты: {}",
        refs.join(", ")
    )
}

/// Хранилище узла: заданное вызывающим; иначе клиент OpenBao/Vault по секции `secrets`; без `secrets.url` —
/// «не настроено» (ссылки на секреты в конфигурации — `Invalid`).
pub async fn store(
    cfg: &Config,
    store: Option<Arc<dyn SecretStore>>,
) -> Result<Arc<dyn SecretStore>> {
    if let Some(store) = store {
        return Ok(store);
    }
    let section = cfg.section::<SecretsConfig>(SECRETS)?;
    if section.url.is_some() {
        let vault = VaultStore::new(&section, cfg.profile())
            .await
            .map_err(context)?;
        return Ok(Arc::new(vault));
    }
    let refs = references(cfg);
    if !refs.is_empty() {
        return Err(Error::Invalid(not_configured(&refs)));
    }
    Ok(Arc::new(NoSecretStore))
}

/// Проверки `thepost doctor` для хранилища секретов (PLT-12): секция, доступность, распечатывание, вход узла,
/// срок токена узла.
pub async fn doctor(cfg: &Config) -> DoctorReport {
    let section = cfg
        .section::<SecretsConfig>(SECRETS)
        .map(|s| (*s).clone())
        .unwrap_or_default();
    tp_platform::secrets::doctor(&section, cfg.profile()).await
}

fn context(e: Error) -> Error {
    match e {
        Error::Invalid(m) if !m.starts_with(SECRETS) => Error::Invalid(format!("{SECRETS}: {m}")),
        other => other,
    }
}

/// Хранилище не настроено: любое чтение — `Invalid` с пояснением.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSecretStore;

#[async_trait]
impl SecretStore for NoSecretStore {
    async fn get(&self, secret: &SecretRef) -> Result<Secret> {
        Err(Error::Invalid(not_configured(&[secret.to_string()])))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sections::registry;
    use tp_platform::secrets::DoctorStatus;

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

    fn parse(yaml: &str) -> Config {
        registry()
            .expect("registry")
            .parse(yaml, Vec::new())
            .expect("config")
    }

    #[tokio::test]
    async fn missing_store_with_references_is_explicit_error() {
        let cfg = parse(DEV);
        assert_eq!(references(&cfg), vec!["thepost/db#password".to_owned()]);
        let e = store(&cfg, None).await.err().expect("error").to_string();
        assert!(e.contains("хранилище секретов не настроено"), "{e}");
        assert!(e.contains("thepost/db#password"), "{e}");

        let plain = DEV.replace("  password: { secret: \"thepost/db#password\" }\n", "");
        assert!(store(&parse(&plain), None).await.is_ok());
    }

    #[tokio::test]
    async fn vault_store_from_section() {
        let cfg = parse(&format!(
            "{DEV}secrets:\n  url: http://127.0.0.1:1\n  tls: {{ allow_plaintext: true }}\n  auth:\n    token:\n      token_file: /nonexistent/token\n"
        ));
        // Клиент создаётся без обращения к хранилищу; чтение секрета — ошибка хранилища, а не «не настроено».
        let vault = store(&cfg, None).await.expect("vault");
        let r: SecretRef = "thepost/db#password".parse().expect("ref");
        let e = vault.get(&r).await.expect_err("unreachable").to_string();
        assert!(!e.contains("не настроено"), "{e}");

        let report = doctor(&cfg).await;
        assert_eq!(report.status(), DoctorStatus::Fail, "{report:?}");
        assert_eq!(
            report.get("secrets_store").map(|c| c.status),
            Some(DoctorStatus::Fail),
            "{report:?}"
        );
        assert_eq!(
            doctor(&parse(DEV))
                .await
                .get("secrets_config")
                .map(|c| c.status),
            Some(DoctorStatus::Skipped)
        );
    }
}
