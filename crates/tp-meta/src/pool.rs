//! Пул соединений PostgreSQL из конфигурации и транзакции в контексте организации (SRS 3.1, 3.8).
//!
//! Режим PgBouncer transaction ([`PoolConfig::pgbouncer`]): соединение сервера меняется между транзакциями,
//! поэтому состояние сеанса не используется — контекст организации задаёт `SET LOCAL` ([`with_tenant`]),
//! кэш подготовленных выражений отключён (безымянные выражения работают с любой версией PgBouncer).
//! Миграции держат рекомендательную блокировку сеанса — их пул подключается в обход PgBouncer
//! ([`PoolConfig::direct_url`], [`connect_direct`]).
//!
//! Канал до PostgreSQL — только TLS `sslmode=verify-full` с корнями и клиентским сертификатом из
//! [`PoolConfig::tls`] (CTR-12, решение 2026-10-30); адрес без `sslmode` подключается так же. Открытый канал
//! (`sslmode=disable|allow|prefer`) — только профиль dev с `tls.allow_plaintext`, с предупреждением в лог
//! при каждом создании пула; в остальных случаях — ошибка конфигурации ([`PoolConfig::check`]).
//!
//! Пароль — только ссылка на секрет в хранилище ([`PoolConfig::password`], CTR-15, решение 2026-10-30): адрес
//! без пароля, значение читается из [`SecretStore`] при создании пула. Пароль в адресе — только профиль dev.

use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::str::FromStr;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::{PgConnection, PgPool};
use tp_contracts::config::duration;
use tp_contracts::secrets::{Secret, SecretRef, SecretStore};
use tp_contracts::tls::{InternalChannel, InternalTls, Profile, Transport};
use tp_contracts::{Error, Result, TenantId};

use crate::schema::{begin, begin_system, db_err};

/// Секция конфигурации пула соединений `database` (SRS 5.5). Все параметры требуют перезапуска.
/// Все поля, кроме `url`, необязательны; неизвестное поле — ошибка разбора.
///
/// ```yaml
/// database:
///   url: postgres://thepost@db.example.ru:5432/thepost
///   password: { secret: "thepost/db#password" }
///   acquire_timeout: 5s
/// ```
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PoolConfig {
    /// Адрес PostgreSQL или PgBouncer без пароля (`postgres://user@host:port/db`); обязателен.
    /// Пароль в адресе — только профиль dev.
    pub url: String,
    /// Пароль роли — ссылка на секрет в хранилище (`{ secret: "<путь>#<поле>" }`), общий для `url` и
    /// `direct_url`. Не задан — вход без пароля (сертификат клиента; `trust` в dev).
    pub password: Option<SecretRef>,
    /// Адрес PostgreSQL в обход PgBouncer для миграций (без пароля); по умолчанию — `url`.
    pub direct_url: Option<String>,
    /// Соединение через PgBouncer в режиме transaction.
    pub pgbouncer: bool,
    /// Наибольшее число соединений пула узла.
    pub max_connections: u32,
    /// Число соединений, поддерживаемых открытыми.
    pub min_connections: u32,
    /// Ожидание свободного соединения; дольше — `Unavailable`.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub acquire_timeout: Duration,
    /// Простаивающее соединение закрывается через этот срок; `null` — не закрывается.
    #[serde(with = "duration::option")]
    #[schemars(with = "Option<duration::HumanDuration>")]
    pub idle_timeout: Option<Duration>,
    /// Соединение пересоздаётся через этот срок (переключение реплик, балансировка); `null` — не пересоздаётся.
    #[serde(with = "duration::option")]
    #[schemars(with = "Option<duration::HumanDuration>")]
    pub max_lifetime: Option<Duration>,
    /// Подготовленных выражений в кэше соединения (без PgBouncer).
    pub statement_cache: usize,
    /// `application_name` соединений (`pg_stat_activity`).
    pub application_name: String,
    /// TLS канала `database.tls`: корни для проверки сервера, клиентский сертификат, `allow_plaintext` (dev).
    pub tls: InternalTls,
    /// Профиль запуска: параметр `profile` верхнего уровня config.yaml, задаёт приложение (в секции не пишется).
    #[serde(skip)]
    #[schemars(skip)]
    pub profile: Profile,
}

impl Default for PoolConfig {
    fn default() -> Self {
        Self {
            url: String::new(),
            password: None,
            direct_url: None,
            pgbouncer: false,
            max_connections: 32,
            min_connections: 0,
            acquire_timeout: Duration::from_secs(5),
            idle_timeout: Some(Duration::from_secs(600)),
            max_lifetime: Some(Duration::from_secs(1800)),
            statement_cache: 256,
            application_name: "thepost".into(),
            tls: InternalTls::default(),
            profile: Profile::default(),
        }
    }
}

impl fmt::Debug for PoolConfig {
    // Адрес в dev может содержать пароль — в журнал не попадает; ссылка на секрет — не секрет.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PoolConfig")
            .field("password", &self.password)
            .field("pgbouncer", &self.pgbouncer)
            .field("max_connections", &self.max_connections)
            .field("min_connections", &self.min_connections)
            .field("acquire_timeout", &self.acquire_timeout)
            .field("idle_timeout", &self.idle_timeout)
            .field("max_lifetime", &self.max_lifetime)
            .field("statement_cache", &self.statement_cache)
            .field("application_name", &self.application_name)
            .field("tls", &self.tls)
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

impl PoolConfig {
    /// Имя секции в config.yaml.
    pub const SECTION: &'static str = "database";

    /// Пароль роли из хранилища секретов; `None` — пароль не задан.
    /// Недоступный, отозванный или запрещённый политикой секрет — `Unavailable` (причина — в журнале).
    pub async fn password(&self, secrets: &dyn SecretStore) -> Result<Option<Secret>> {
        match &self.password {
            None => Ok(None),
            Some(r) => fetch_secret(secrets, r, "database.password")
                .await
                .map(Some),
        }
    }

    /// Параметры соединения с `url` без пароля; `pgbouncer` отключает кэш подготовленных выражений.
    pub fn connect_options(&self, url: &str, pgbouncer: bool) -> Result<PgConnectOptions> {
        self.options(url, pgbouncer).map(|(opts, _)| opts)
    }

    /// Проверяет секцию (`thepost config check` и запуск): адреса `url` и `direct_url`, TLS канала,
    /// пароль в адресах (только профиль dev).
    /// `Transport::Plaintext` — хотя бы один адрес открыт (только профиль dev с `tls.allow_plaintext`).
    pub fn check(&self) -> Result<Transport> {
        let (_, mut transport) = self.options(&self.url, self.pgbouncer)?;
        if let Some(direct) = &self.direct_url
            && self.options(direct, false)?.1 == Transport::Plaintext
        {
            transport = Transport::Plaintext;
        }
        Ok(transport)
    }

    fn options(&self, url: &str, pgbouncer: bool) -> Result<(PgConnectOptions, Transport)> {
        if url.is_empty() {
            return Err(Error::invalid("database.url не задан"));
        }
        // Тексты ошибок проверки и разбора не содержат адреса: в нём может быть пароль.
        let transport = self
            .tls
            .check_url(InternalChannel::Postgres, url, self.profile)?;
        if self.profile != Profile::Dev && url_has_password(url) {
            return Err(Error::invalid(
                "database: пароль в адресе PostgreSQL допустим только в профиле dev; \
                 задайте database.password: { secret: \"<путь>#<поле>\" }",
            ));
        }
        if self.tls.server_name.is_some() {
            return Err(Error::invalid(
                "database.tls.server_name не поддерживается: сертификат проверяется по хосту из database.url",
            ));
        }
        let mut opts = PgConnectOptions::from_str(url)
            .map_err(|_| Error::invalid("database.url: неверный адрес"))?
            .application_name(&self.application_name);
        if transport == Transport::Tls {
            // Режим задаётся явно: адрес без `sslmode` и переменная PGSSLMODE не ослабляют проверку.
            opts = opts.ssl_mode(PgSslMode::VerifyFull);
            if let Some(ca) = &self.tls.ca_file {
                opts = opts.ssl_root_cert(ca);
            }
            if let (Some(cert), Some(key)) = (&self.tls.cert_file, &self.tls.key_file) {
                opts = opts.ssl_client_cert(cert).ssl_client_key(key);
            }
        }
        let opts = if pgbouncer {
            opts.statement_cache_capacity(0)
        } else {
            opts.statement_cache_capacity(self.statement_cache)
        };
        Ok((opts, transport))
    }

    /// Параметры нового пула с паролем из хранилища; открытый канал — предупреждение в лог.
    fn open_options(
        &self,
        url: &str,
        pgbouncer: bool,
        password: Option<&Secret>,
    ) -> Result<PgConnectOptions> {
        let (opts, transport) = self.options(url, pgbouncer)?;
        if transport == Transport::Plaintext {
            tracing::warn!("{}", InternalChannel::Postgres.plaintext_warning());
        }
        Ok(match password {
            Some(p) => opts.password(p.expose()),
            None => opts,
        })
    }

    fn pool_options(&self) -> PgPoolOptions {
        PgPoolOptions::new()
            .max_connections(self.max_connections)
            .min_connections(self.min_connections)
            .acquire_timeout(self.acquire_timeout)
            .idle_timeout(self.idle_timeout)
            .max_lifetime(self.max_lifetime)
    }
}

/// Адрес PostgreSQL содержит пароль: `scheme://user:password@host…` или параметр `password=`.
pub fn url_has_password(url: &str) -> bool {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    if rest[..authority_end]
        .rsplit_once('@')
        .is_some_and(|(userinfo, _)| userinfo.contains(':'))
    {
        return true;
    }
    rest[authority_end..]
        .split_once('?')
        .is_some_and(|(_, query)| {
            query.split(['&', '#']).any(|kv| {
                kv.split_once('=')
                    .map_or(kv, |(k, _)| k)
                    .eq_ignore_ascii_case("password")
            })
        })
}

/// Значение секрета для подключения. Отказ хранилища (нет секрета, отозван, запрещён, недоступно) —
/// `Unavailable`: пароль мог смениться, повтор после исправления поможет; ссылка (не значение) и причина — в журнал.
pub(crate) async fn fetch_secret(
    secrets: &dyn SecretStore,
    secret: &SecretRef,
    what: &str,
) -> Result<Secret> {
    secrets.get(secret).await.map_err(|e| {
        tracing::warn!(%secret, error = %e, "{what}: секрет недоступен");
        match e {
            Error::Unavailable { retry_after } => Error::Unavailable { retry_after },
            _ => Error::UNAVAILABLE,
        }
    })
}

/// Пул по конфигурации; пароль — из хранилища секретов. Первое соединение открывается сразу
/// (ошибка адреса, секрета или доступа — при старте).
pub async fn connect(cfg: &PoolConfig, secrets: &dyn SecretStore) -> Result<PgPool> {
    let password = cfg.password(secrets).await?;
    let opts = cfg.open_options(&cfg.url, cfg.pgbouncer, password.as_ref())?;
    cfg.pool_options().connect_with(opts).await.map_err(db_err)
}

/// Пул к адресу `url` с паролем `password` и параметрами `cfg`, соединения — по требованию
/// (роли загружаются лениво, SRS 2.7; пулы шардов).
pub fn connect_lazy(cfg: &PoolConfig, url: &str, password: Option<&Secret>) -> Result<PgPool> {
    let opts = cfg.open_options(url, cfg.pgbouncer, password)?;
    Ok(cfg.pool_options().connect_lazy_with(opts))
}

/// Пул из одного соединения в обход PgBouncer для миграций ([`crate::schema::migrate`], модули).
pub async fn connect_direct(cfg: &PoolConfig, secrets: &dyn SecretStore) -> Result<PgPool> {
    let (url, pgbouncer) = match &cfg.direct_url {
        Some(url) => (url.as_str(), false),
        None => (cfg.url.as_str(), cfg.pgbouncer),
    };
    let password = cfg.password(secrets).await?;
    let opts = cfg.open_options(url, pgbouncer, password.as_ref())?;
    PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(cfg.acquire_timeout)
        .connect_with(opts)
        .await
        .map_err(db_err)
}

/// Будущее тела транзакции, заимствующее соединение.
pub type TxFuture<'c, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'c>>;

/// Выполняет `f` в транзакции организации (`SET LOCAL app.tenant_id`, RLS): `Ok` — фиксация,
/// ошибка — откат. Внутри — только СУБД: к BLOB и сети в открытой транзакции не обращаться.
///
/// ```no_run
/// # async fn demo(pool: &sqlx::PgPool, tenant: tp_contracts::TenantId) -> tp_contracts::Result<()> {
/// let n: i64 = tp_meta::pool::with_tenant(pool, tenant, |tx| {
///     Box::pin(async move {
///         sqlx::query_scalar("SELECT count(*) FROM accounts")
///             .fetch_one(&mut *tx)
///             .await
///             .map_err(tp_meta::schema::db_err)
///     })
/// })
/// .await?;
/// # Ok(()) }
/// ```
pub async fn with_tenant<T, F>(pool: &PgPool, tenant: TenantId, f: F) -> Result<T>
where
    F: for<'c> FnOnce(&'c mut PgConnection) -> TxFuture<'c, T>,
{
    let mut tx = begin(pool, tenant).await?;
    let out = f(&mut tx).await?;
    tx.commit().await.map_err(db_err)?;
    Ok(out)
}

/// Как [`with_tenant`], но по всем организациям (`SET LOCAL app.all_tenants`): GC, журнал событий.
pub async fn with_system<T, F>(pool: &PgPool, f: F) -> Result<T>
where
    F: for<'c> FnOnce(&'c mut PgConnection) -> TxFuture<'c, T>,
{
    let mut tx = begin_system(pool).await?;
    let out = f(&mut tx).await?;
    tx.commit().await.map_err(db_err)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(url: &str) -> PoolConfig {
        PoolConfig {
            url: url.into(),
            ..PoolConfig::default()
        }
    }

    fn dev(url: &str) -> PoolConfig {
        PoolConfig {
            tls: InternalTls {
                allow_plaintext: true,
                ..InternalTls::default()
            },
            profile: Profile::Dev,
            ..cfg(url)
        }
    }

    #[test]
    fn verify_full_by_default() {
        for url in [
            "postgres://u@db.internal/thepost",
            "postgres://u@db.internal/thepost?sslmode=verify-full",
        ] {
            let c = cfg(url);
            assert_eq!(c.check(), Ok(Transport::Tls), "{url}");
            let opts = c.connect_options(url, false).expect("options");
            assert!(
                matches!(opts.get_ssl_mode(), PgSslMode::VerifyFull),
                "{url}"
            );
        }
        // В dev адрес с TLS остаётся verify-full.
        let url = "postgres://db.internal/thepost";
        let opts = dev(url).connect_options(url, true).expect("dev tls");
        assert!(matches!(opts.get_ssl_mode(), PgSslMode::VerifyFull));
    }

    #[test]
    fn plaintext_only_in_dev() {
        let url = "postgres://u:secret@db/thepost?sslmode=disable";
        // Вне dev — ошибка конфигурации, даже с allow_plaintext.
        let e = cfg(url).check().expect_err("prod disable");
        assert!(
            matches!(&e, Error::Invalid(m) if !m.contains("secret")),
            "{e:?}"
        );
        let allowed = PoolConfig {
            profile: Profile::Prod,
            ..dev(url)
        };
        assert!(allowed.check().is_err());
        // dev без allow_plaintext — тоже ошибка.
        let no_allow = PoolConfig {
            profile: Profile::Dev,
            ..cfg(url)
        };
        assert!(no_allow.check().is_err());

        let d = dev(url);
        assert_eq!(d.check(), Ok(Transport::Plaintext));
        let opts = d.connect_options(url, false).expect("dev plaintext");
        assert!(matches!(opts.get_ssl_mode(), PgSslMode::Disable));
    }

    #[test]
    fn weak_modes_and_direct_url() {
        for mode in ["require", "verify-ca", "prefer", "allow"] {
            let url = format!("postgres://db/thepost?sslmode={mode}");
            assert!(cfg(&url).check().is_err(), "{mode}");
        }
        let direct = Some("postgres://db:5432/thepost?sslmode=disable".to_owned());
        let c = PoolConfig {
            direct_url: direct.clone(),
            ..cfg("postgres://pgbouncer:6432/thepost")
        };
        assert!(c.check().is_err());
        let d = PoolConfig {
            direct_url: direct,
            ..dev("postgres://pgbouncer:6432/thepost")
        };
        assert_eq!(d.check(), Ok(Transport::Plaintext));
    }

    #[test]
    fn password_in_url() {
        for url in [
            "postgres://u:p@db/thepost",
            "postgres://u:p@h1:5432,h2:5432/thepost",
            "postgresql://u:@db",
            "postgres://u@db/thepost?sslmode=verify-full&password=p",
            "postgres://db/thepost?PASSWORD=p",
        ] {
            assert!(url_has_password(url), "{url}");
            assert!(cfg(url).check().is_err(), "{url}");
        }
        for url in [
            "postgres://u@db:5432/thepost",
            "postgres://db/thepost?sslmode=verify-full",
            "postgres://u@db/thepost?application_name=a:b@c",
            "postgres://db/p:q@r",
        ] {
            assert!(!url_has_password(url), "{url}");
        }
        // dev: пароль в адресе допустим.
        let url = "postgres://u:p@db/thepost?sslmode=disable";
        assert_eq!(dev(url).check(), Ok(Transport::Plaintext));
    }

    #[test]
    fn tls_section() {
        let url = "postgres://db.internal/thepost";
        let with_tls = |tls| PoolConfig { tls, ..cfg(url) };
        let full = with_tls(InternalTls {
            ca_file: Some("/etc/thepost/ca.pem".into()),
            cert_file: Some("/etc/thepost/node.pem".into()),
            key_file: Some("/etc/thepost/node.key".into()),
            ..InternalTls::default()
        });
        assert_eq!(full.check(), Ok(Transport::Tls));
        assert!(format!("{full:?}").contains("ca.pem"));
        let half = with_tls(InternalTls {
            cert_file: Some("/etc/thepost/node.pem".into()),
            ..InternalTls::default()
        });
        assert!(half.check().is_err());
        let named = with_tls(InternalTls {
            server_name: Some("db.internal".into()),
            ..InternalTls::default()
        });
        assert!(named.check().is_err());
    }
}
