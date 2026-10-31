//! Пароли (SRS 5.1): пароль каталога (привязка LDAP) и пароли приложений — для клиентов без Kerberos и
//! OAUTHBEARER; выпуск и отзыв — через Admin API.
//!
//! Пароль приложения — случайная строка, выдаётся один раз; хранится только хэш argon2id (PHC-строка).
//! Проверка — в `spawn_blocking`: argon2 намеренно дорог. Защита от перебора — `tp-platform` (PLT-07):
//! протокол спрашивает ограничитель до [`PasswordAuthenticator`] и сообщает ему о неудаче.

use std::fmt;
use std::sync::Arc;
use std::time::SystemTime;

use argon2::{Argon2, Params, PasswordHasher, PasswordVerifier};
use async_trait::async_trait;
use rand::RngExt;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tp_contracts::auth::{Authenticator, Credentials, Principal};
use tp_contracts::secrets::Secret;
use tp_contracts::{AccountId, Error, Result, TenantId};

use crate::Accounts;
use crate::ldap::LdapDirectory;

/// Алфавит паролей приложений: строчные латинские буквы (удобно набирать на телефоне).
const ALPHABET: &[u8; 26] = b"abcdefghijklmnopqrstuvwxyz";

/// Параметры argon2id (RFC 9106); по умолчанию — рекомендация OWASP.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct Argon2Config {
    /// Память, КиБ.
    pub memory_kib: u32,
    /// Число проходов.
    pub iterations: u32,
    /// Степень параллельности.
    pub parallelism: u32,
}

impl Default for Argon2Config {
    fn default() -> Self {
        Argon2Config {
            memory_kib: Params::DEFAULT_M_COST,
            iterations: Params::DEFAULT_T_COST,
            parallelism: Params::DEFAULT_P_COST,
        }
    }
}

/// Секция паролей (`auth.passwords`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PasswordConfig {
    /// Длина пароля приложения, символов (не меньше 12).
    pub app_password_length: usize,
    /// Наибольшее число действующих паролей приложений учётной записи.
    pub max_app_passwords: usize,
    /// Параметры хэширования новых паролей; выпущенные проверяются по параметрам из своего хэша.
    pub argon2: Argon2Config,
}

impl Default for PasswordConfig {
    fn default() -> Self {
        PasswordConfig {
            app_password_length: 16,
            max_app_passwords: 10,
            argon2: Argon2Config::default(),
        }
    }
}

impl PasswordConfig {
    /// Проверяет секцию.
    pub fn check(&self) -> Result<()> {
        if self.app_password_length < 12 {
            return Err(Error::invalid(
                "passwords: app_password_length must be at least 12",
            ));
        }
        self.params().map(|_| ())
    }

    fn params(&self) -> Result<Params> {
        let a = &self.argon2;
        Params::new(a.memory_kib, a.iterations, a.parallelism, None)
            .map_err(|e| Error::Invalid(format!("passwords.argon2: {e}")))
    }
}

/// Пароль приложения (без значения: оно выдаётся один раз при выпуске).
#[derive(Clone, PartialEq, Eq)]
pub struct AppPassword {
    /// Идентификатор (для отзыва).
    pub id: String,
    /// Подпись владельца («Телефон», «Thunderbird дома»).
    pub label: String,
    /// Хэш argon2id, PHC-строка.
    pub hash: String,
    /// Выпущен.
    pub created_at: SystemTime,
    /// Отозван; отозванный пароль не принимается.
    pub revoked_at: Option<SystemTime>,
}

impl fmt::Debug for AppPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppPassword")
            .field("id", &self.id)
            .field("label", &self.label)
            .field("created_at", &self.created_at)
            .field("revoked_at", &self.revoked_at)
            .finish_non_exhaustive()
    }
}

/// Хранилище паролей приложений учётной записи (метаданные).
#[async_trait]
pub trait AppPasswordStore: Send + Sync + 'static {
    /// Пароли учётной записи, включая отозванные.
    async fn list(&self, tenant: TenantId, account: AccountId) -> Result<Vec<AppPassword>>;

    /// Добавляет пароль.
    async fn insert(
        &self,
        tenant: TenantId,
        account: AccountId,
        password: AppPassword,
    ) -> Result<()>;

    /// Отмечает пароль отозванным; нет пароля — `NotFound`, повторный отзыв — не ошибка.
    async fn revoke(
        &self,
        tenant: TenantId,
        account: AccountId,
        id: &str,
        at: SystemTime,
    ) -> Result<()>;
}

/// Выпуск, отзыв и проверка паролей приложений.
#[derive(Clone)]
pub struct AppPasswords {
    store: Arc<dyn AppPasswordStore>,
    cfg: PasswordConfig,
}

impl fmt::Debug for AppPasswords {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppPasswords")
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

impl AppPasswords {
    /// Пароли приложений в `store` с параметрами `cfg`.
    pub fn new(store: Arc<dyn AppPasswordStore>, cfg: PasswordConfig) -> Result<Self> {
        cfg.check()?;
        Ok(AppPasswords { store, cfg })
    }

    /// Выпускает пароль с подписью `label`; значение возвращается один раз. Предел действующих паролей —
    /// `Conflict`.
    pub async fn issue(
        &self,
        tenant: TenantId,
        account: AccountId,
        label: &str,
    ) -> Result<(AppPassword, Secret)> {
        let active = self
            .store
            .list(tenant, account)
            .await?
            .iter()
            .filter(|p| p.revoked_at.is_none())
            .count();
        if active >= self.cfg.max_app_passwords {
            return Err(Error::Conflict);
        }
        let (id, value) = {
            let mut rng = rand::rng();
            let id = format!("{:016x}", rng.random::<u64>());
            let value: String = (0..self.cfg.app_password_length)
                .map(|_| char::from(ALPHABET[rng.random_range(0..ALPHABET.len())]))
                .collect();
            (id, Secret::new(value))
        };
        let params = self.cfg.params()?;
        let v = value.clone();
        let hash = tokio::task::spawn_blocking(move || {
            Argon2::from(params)
                .hash_password(v.expose().as_bytes())
                .map(|h| h.to_string())
        })
        .await
        .map_err(|e| Error::Internal(format!("argon2: {e}")))?
        .map_err(|e| Error::Internal(format!("argon2: {e}")))?;
        let password = AppPassword {
            id,
            label: label.to_owned(),
            hash,
            created_at: SystemTime::now(),
            revoked_at: None,
        };
        self.store.insert(tenant, account, password.clone()).await?;
        Ok((password, value))
    }

    /// Отзывает пароль `id`.
    pub async fn revoke(&self, tenant: TenantId, account: AccountId, id: &str) -> Result<()> {
        self.store
            .revoke(tenant, account, id, SystemTime::now())
            .await
    }

    /// Пароли учётной записи (без значений), включая отозванные.
    pub async fn list(&self, tenant: TenantId, account: AccountId) -> Result<Vec<AppPassword>> {
        self.store.list(tenant, account).await
    }

    /// Совпадает ли `secret` с действующим паролем приложения учётной записи.
    pub async fn verify(&self, tenant: TenantId, account: AccountId, secret: &str) -> Result<bool> {
        let hashes: Vec<String> = self
            .store
            .list(tenant, account)
            .await?
            .into_iter()
            .filter(|p| p.revoked_at.is_none())
            .map(|p| p.hash)
            .collect();
        if hashes.is_empty() || secret.is_empty() {
            return Ok(false);
        }
        let secret = Secret::new(secret);
        tokio::task::spawn_blocking(move || {
            let argon = Argon2::default();
            hashes.iter().any(|h| {
                argon
                    .verify_password(secret.expose().as_bytes(), h.as_str())
                    .is_ok()
            })
        })
        .await
        .map_err(|e| Error::Internal(format!("argon2: {e}")))
    }
}

/// Проверка пароля учётной записи во внешнем каталоге. Реализация — [`LdapDirectory`] (привязка).
#[async_trait]
pub trait PasswordCheck: Send + Sync + 'static {
    /// Пароль верен; нет записи или неверный пароль — `false`, каталог недоступен — `Unavailable`.
    async fn check_password(&self, login: &str, password: &str) -> Result<bool>;
}

#[async_trait]
impl PasswordCheck for LdapDirectory {
    async fn check_password(&self, login: &str, password: &str) -> Result<bool> {
        LdapDirectory::check_password(self, login, password).await
    }
}

/// [`Authenticator`] для [`Credentials::Password`]: учётная запись включена, пароль — действующий пароль
/// приложения или пароль каталога. Иные учётные данные, неверный пароль — `Forbidden`.
#[derive(Clone)]
pub struct PasswordAuthenticator {
    accounts: Accounts,
    apps: Option<AppPasswords>,
    directory: Option<Arc<dyn PasswordCheck>>,
}

impl fmt::Debug for PasswordAuthenticator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PasswordAuthenticator")
            .field("accounts", &self.accounts)
            .field("apps", &self.apps)
            .field("directory", &self.directory.is_some())
            .finish()
    }
}

impl PasswordAuthenticator {
    /// Проверка паролей учётных записей `accounts`; без источников пароля любой вход — `Forbidden`.
    pub fn new(accounts: Accounts) -> Self {
        PasswordAuthenticator {
            accounts,
            apps: None,
            directory: None,
        }
    }

    /// Принимать пароли приложений.
    pub fn with_app_passwords(mut self, apps: AppPasswords) -> Self {
        self.apps = Some(apps);
        self
    }

    /// Принимать пароль каталога.
    pub fn with_directory(mut self, directory: Arc<dyn PasswordCheck>) -> Self {
        self.directory = Some(directory);
        self
    }
}

#[async_trait]
impl Authenticator for PasswordAuthenticator {
    async fn verify(&self, credentials: Credentials) -> Result<Principal> {
        let Credentials::Password { login, secret } = credentials else {
            return Err(Error::Forbidden);
        };
        let secret = Secret::new(secret);
        if secret.expose().is_empty() {
            return Err(Error::Forbidden);
        }
        let account = self.accounts.enabled(&login).await?;
        let mut ok = false;
        if let Some(apps) = &self.apps {
            ok = apps
                .verify(account.tenant, account.id, secret.expose())
                .await?;
        }
        if !ok && let Some(dir) = &self.directory {
            ok = dir.check_password(&account.login, secret.expose()).await?;
        }
        if ok {
            self.accounts.principal(account, None).await
        } else {
            Err(Error::Forbidden)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::sync::Mutex;

    use super::*;
    use crate::testutil::accounts;

    /// Пароли приложений в памяти (временно: хранилища в метаданных и контракта для него пока нет).
    #[derive(Default)]
    struct MemAppPasswords(Mutex<HashMap<(TenantId, AccountId), Vec<AppPassword>>>);

    #[async_trait]
    impl AppPasswordStore for MemAppPasswords {
        async fn list(&self, tenant: TenantId, account: AccountId) -> Result<Vec<AppPassword>> {
            Ok(self
                .0
                .lock()
                .expect("lock")
                .get(&(tenant, account))
                .cloned()
                .unwrap_or_default())
        }

        async fn insert(
            &self,
            tenant: TenantId,
            account: AccountId,
            password: AppPassword,
        ) -> Result<()> {
            self.0
                .lock()
                .expect("lock")
                .entry((tenant, account))
                .or_default()
                .push(password);
            Ok(())
        }

        async fn revoke(
            &self,
            tenant: TenantId,
            account: AccountId,
            id: &str,
            at: SystemTime,
        ) -> Result<()> {
            let mut map = self.0.lock().expect("lock");
            let p = map
                .get_mut(&(tenant, account))
                .and_then(|v| v.iter_mut().find(|p| p.id == id))
                .ok_or(Error::NotFound)?;
            p.revoked_at.get_or_insert(at);
            Ok(())
        }
    }

    /// Каталог: имя входа → пароль.
    struct Dir(HashMap<&'static str, &'static str>);

    #[async_trait]
    impl PasswordCheck for Dir {
        async fn check_password(&self, login: &str, password: &str) -> Result<bool> {
            Ok(self.0.get(login) == Some(&password))
        }
    }

    fn cheap() -> PasswordConfig {
        PasswordConfig {
            argon2: Argon2Config {
                memory_kib: 64,
                iterations: 1,
                parallelism: 1,
            },
            max_app_passwords: 2,
            ..PasswordConfig::default()
        }
    }

    fn pw(login: &str, secret: &str) -> Credentials {
        Credentials::Password {
            login: login.into(),
            secret: secret.into(),
        }
    }

    #[tokio::test]
    async fn directory_password() {
        let (accounts, ids) =
            accounts(&[("alice@example.org", true), ("bob@example.org", false)]).await;
        let auth =
            PasswordAuthenticator::new(accounts).with_directory(Arc::new(Dir(HashMap::from([
                ("alice@example.org", "wonderland"),
                ("bob@example.org", "builder"),
            ]))));
        let p = auth
            .verify(pw("Alice@Example.org", "wonderland"))
            .await
            .expect("ok");
        assert_eq!(p.account, ids[0]);
        assert_eq!(p.login, "alice@example.org");
        assert_eq!(p.groups, BTreeSet::from(["staff".to_owned()]));
        assert_eq!(p.scopes, None);
        for bad in [
            pw("alice@example.org", "wrong"),
            pw("alice@example.org", ""),
            pw("nobody@example.org", "wonderland"),
            pw("bob@example.org", "builder"),
            Credentials::Bearer("token".into()),
        ] {
            assert_eq!(auth.verify(bad).await, Err(Error::Forbidden));
        }
    }

    #[tokio::test]
    async fn app_password_issue_and_revoke() {
        let (accounts, ids) = accounts(&[("alice@example.org", true)]).await;
        let store = Arc::new(MemAppPasswords::default());
        let apps = AppPasswords::new(store.clone(), cheap()).expect("cfg");
        let auth = PasswordAuthenticator::new(accounts)
            .with_app_passwords(apps.clone())
            .with_directory(Arc::new(Dir(HashMap::from([(
                "alice@example.org",
                "wonderland",
            )]))));
        let t = TenantId::DEFAULT;

        let (phone, secret) = apps.issue(t, ids[0], "Телефон").await.expect("issue");
        assert_eq!(secret.expose().len(), 16);
        assert!(secret.expose().bytes().all(|b| b.is_ascii_lowercase()));
        assert!(phone.hash.starts_with("$argon2id$"));
        assert!(!format!("{phone:?}").contains(&phone.hash));
        let (_, other) = apps.issue(t, ids[0], "Ноутбук").await.expect("issue 2");
        assert_ne!(secret.expose(), other.expose());
        assert_eq!(
            apps.issue(t, ids[0], "лишний").await.map(|_| ()),
            Err(Error::Conflict)
        );

        assert!(
            auth.verify(pw("alice@example.org", secret.expose()))
                .await
                .is_ok()
        );
        assert!(
            auth.verify(pw("alice@example.org", other.expose()))
                .await
                .is_ok()
        );
        assert_eq!(
            auth.verify(pw("alice@example.org", "abcdefghijklmnop"))
                .await,
            Err(Error::Forbidden)
        );

        // Отозванный пароль не принимается; другой пароль приложения и пароль каталога — по-прежнему.
        apps.revoke(t, ids[0], &phone.id).await.expect("revoke");
        apps.revoke(t, ids[0], &phone.id)
            .await
            .expect("revoke again");
        assert_eq!(
            auth.verify(pw("alice@example.org", secret.expose())).await,
            Err(Error::Forbidden)
        );
        assert!(
            auth.verify(pw("alice@example.org", other.expose()))
                .await
                .is_ok()
        );
        assert!(
            auth.verify(pw("alice@example.org", "wonderland"))
                .await
                .is_ok()
        );
        assert_eq!(apps.revoke(t, ids[0], "nope").await, Err(Error::NotFound));
        // После отзыва можно выпустить новый.
        assert!(apps.issue(t, ids[0], "Телефон 2").await.is_ok());
        assert_eq!(apps.list(t, ids[0]).await.expect("list").len(), 3);
    }

    #[test]
    fn config() {
        assert_eq!(PasswordConfig::default().check(), Ok(()));
        let short = PasswordConfig {
            app_password_length: 8,
            ..PasswordConfig::default()
        };
        assert!(short.check().is_err());
        let bad = PasswordConfig {
            argon2: Argon2Config {
                memory_kib: 1,
                ..Argon2Config::default()
            },
            ..PasswordConfig::default()
        };
        assert!(bad.check().is_err());
        let parsed: PasswordConfig =
            serde_json::from_str(r#"{"max_app_passwords": 3}"#).expect("parse");
        assert_eq!(parsed.max_app_passwords, 3);
        assert_eq!(parsed.app_password_length, 16);
    }
}
