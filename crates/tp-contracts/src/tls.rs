//! TLS внутренних каналов (CONTRACTS.md#Internal TLS; SRS 6.2, решение 2026-10-30).
//!
//! Узел ↔ PostgreSQL, Valkey, S3 и узел ↔ узел — только TLS. Секция [`InternalTls`] одна для всех
//! каналов: крейт канала вкладывает её полем `tls` своей секции и при загрузке конфигурации и запуске
//! вызывает [`InternalTls::check_url`]. Открытый канал — только в профиле [`Profile::Dev`] с
//! `allow_plaintext: true`; тогда крейт пишет [`InternalChannel::plaintext_warning`] в лог на каждом запуске.
//!
//! ```
//! use tp_contracts::tls::{InternalChannel, InternalTls, Profile, Transport};
//!
//! let tls = InternalTls::default();
//! let url = "postgres://thepost@db.internal/thepost?sslmode=verify-full";
//! assert_eq!(tls.check_url(InternalChannel::Postgres, url, Profile::Prod)?, Transport::Tls);
//! assert!(tls.check_url(InternalChannel::Valkey, "redis://cache:6379", Profile::Prod).is_err());
//!
//! let dev = InternalTls { allow_plaintext: true, ..InternalTls::default() };
//! assert_eq!(
//!     dev.check_url(InternalChannel::Valkey, "redis://cache:6379", Profile::Dev)?,
//!     Transport::Plaintext
//! );
//! assert!(dev.check_url(InternalChannel::Valkey, "redis://cache:6379", Profile::Prod).is_err());
//! # Ok::<(), tp_contracts::Error>(())
//! ```

use std::fmt;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Профиль запуска узла: параметр `profile` верхнего уровня config.yaml (читает приложение).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Рабочий: все внутренние каналы только с TLS.
    #[default]
    Prod,
    /// Разработка: открытый канал допускается по явной настройке канала.
    Dev,
}

/// Внутренний канал узла.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum InternalChannel {
    /// PostgreSQL: `postgres://`/`postgresql://`, TLS — `sslmode=verify-full` (или без `sslmode`).
    Postgres,
    /// Valkey: `rediss://`/`valkeys://`; открытый — `redis://`/`valkey://`.
    Valkey,
    /// S3: `https://`; открытый — `http://`.
    S3,
    /// Узел ↔ узел (SMTP edge → mailbox, mTLS по SRS 2.6): `https://`/`smtps://`; открытый — `http://`/`smtp://`.
    Node,
}

impl InternalChannel {
    /// Название для сообщений и логов.
    pub const fn as_str(self) -> &'static str {
        match self {
            InternalChannel::Postgres => "PostgreSQL",
            InternalChannel::Valkey => "Valkey",
            InternalChannel::S3 => "S3",
            InternalChannel::Node => "node-to-node",
        }
    }

    /// Адрес канала с TLS для подсказки в ошибке.
    const fn tls_hint(self) -> &'static str {
        match self {
            InternalChannel::Postgres => "sslmode=verify-full",
            InternalChannel::Valkey => "rediss://",
            InternalChannel::S3 => "https://",
            InternalChannel::Node => "https:// or smtps://",
        }
    }

    /// Предупреждение для лога при открытом канале (без адреса: в нём может быть пароль).
    pub fn plaintext_warning(self) -> String {
        format!(
            "internal channel {self} is NOT encrypted (tls.allow_plaintext, profile dev); never use in production"
        )
    }

    /// Транспорт по схеме адреса.
    fn transport_of(self, url: &str) -> Result<Transport> {
        let (scheme, rest) = url
            .split_once("://")
            .ok_or_else(|| Error::Invalid(format!("{self}: address without scheme")))?;
        let scheme = scheme.to_ascii_lowercase();
        let transport = match (self, scheme.as_str()) {
            (InternalChannel::Postgres, "postgres" | "postgresql") => {
                return postgres_transport(rest);
            }
            (InternalChannel::Valkey, "rediss" | "valkeys")
            | (InternalChannel::S3, "https")
            | (InternalChannel::Node, "https" | "smtps") => Transport::Tls,
            (InternalChannel::Valkey, "redis" | "valkey")
            | (InternalChannel::S3, "http")
            | (InternalChannel::Node, "http" | "smtp") => Transport::Plaintext,
            _ => {
                return Err(Error::Invalid(format!(
                    "{self}: unsupported address scheme {scheme}://"
                )));
            }
        };
        Ok(transport)
    }
}

impl fmt::Display for InternalChannel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `sslmode` из строки запроса адреса PostgreSQL (libpq).
fn postgres_transport(rest: &str) -> Result<Transport> {
    let query = rest.split_once('?').map_or("", |(_, q)| q);
    let mode = query
        .split('&')
        .filter_map(|kv| kv.split_once('='))
        .filter(|(k, _)| *k == "sslmode")
        .map(|(_, v)| v)
        .next_back();
    match mode {
        // Без `sslmode` клиент обязан подключаться с verify-full.
        None | Some("verify-full") => Ok(Transport::Tls),
        Some("disable" | "allow" | "prefer") => Ok(Transport::Plaintext),
        Some(m @ ("require" | "verify-ca")) => Err(Error::Invalid(format!(
            "PostgreSQL: sslmode={m} does not verify the server name; use sslmode=verify-full"
        ))),
        Some(m) => Err(Error::Invalid(format!("PostgreSQL: unknown sslmode={m}"))),
    }
}

/// Транспорт внутреннего канала после проверки.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transport {
    /// TLS с проверкой сертификата сервера.
    Tls,
    /// Открытый канал (только профиль dev): записать [`InternalChannel::plaintext_warning`] в лог.
    Plaintext,
}

/// Секция TLS внутреннего канала (поле `tls` секции канала). Без полей — TLS с системными корнями.
///
/// Сертификаты и ключи — PEM-файлы; источник (файлы, секреты Kubernetes) и перечитывание — как для
/// клиентского TLS (SRS 6.2). Изменение путей применяется при новом подключении.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct InternalTls {
    /// Корневые сертификаты для проверки сервера (PEM); не задан — системные корни.
    pub ca_file: Option<PathBuf>,
    /// Клиентский сертификат для mTLS (PEM); задаётся вместе с `key_file`.
    pub cert_file: Option<PathBuf>,
    /// Ключ клиентского сертификата (PEM); задаётся вместе с `cert_file`.
    pub key_file: Option<PathBuf>,
    /// Имя сервера для SNI и проверки сертификата; не задано — хост из адреса.
    pub server_name: Option<String>,
    /// Разрешить открытый канал. Только в профиле dev; иначе — ошибка конфигурации.
    pub allow_plaintext: bool,
}

impl InternalTls {
    /// Проверяет секцию: `allow_plaintext` только в профиле dev, сертификат и ключ клиента — парой.
    pub fn validate(&self, channel: InternalChannel, profile: Profile) -> Result<()> {
        if self.allow_plaintext && profile != Profile::Dev {
            return Err(Error::Invalid(format!(
                "{channel}: tls.allow_plaintext is allowed only with profile: dev"
            )));
        }
        if self.cert_file.is_some() != self.key_file.is_some() {
            return Err(Error::Invalid(format!(
                "{channel}: tls.cert_file and tls.key_file must be set together"
            )));
        }
        if self.server_name.as_deref().is_some_and(str::is_empty) {
            return Err(Error::Invalid(format!(
                "{channel}: tls.server_name is empty"
            )));
        }
        Ok(())
    }

    /// Проверяет секцию и адрес канала (`thepost config check` и запуск). Адрес без TLS допустим, только если
    /// `allow_plaintext` в профиле dev; `Transport::Plaintext` — вызывающий пишет предупреждение в лог.
    pub fn check_url(
        &self,
        channel: InternalChannel,
        url: &str,
        profile: Profile,
    ) -> Result<Transport> {
        self.validate(channel, profile)?;
        let transport = channel.transport_of(url)?;
        if transport == Transport::Plaintext && !self.allow_plaintext {
            return Err(Error::Invalid(format!(
                "{channel}: address without TLS; use {} (plaintext only with profile: dev and tls.allow_plaintext)",
                channel.tls_hint()
            )));
        }
        Ok(transport)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use InternalChannel::*;

    fn dev() -> InternalTls {
        InternalTls {
            allow_plaintext: true,
            ..InternalTls::default()
        }
    }

    #[test]
    fn plaintext_outside_dev_rejected() {
        let e = dev()
            .validate(Postgres, Profile::Prod)
            .expect_err("prod plaintext");
        assert_eq!(
            e,
            Error::Invalid(
                "PostgreSQL: tls.allow_plaintext is allowed only with profile: dev".into()
            )
        );
        // Даже при адресе с TLS: настройка не должна попасть в рабочую конфигурацию.
        assert!(
            dev()
                .check_url(S3, "https://s3.internal", Profile::Prod)
                .is_err()
        );
        assert_eq!(dev().validate(Postgres, Profile::Dev), Ok(()));
    }

    #[test]
    fn address_without_tls() {
        let tls = InternalTls::default();
        for profile in [Profile::Prod, Profile::Dev] {
            let e = tls
                .check_url(Valkey, "redis://cache:6379", profile)
                .expect_err("plaintext without allow");
            assert!(
                matches!(&e, Error::Invalid(m) if m.contains("rediss://")),
                "{e:?}"
            );
        }
        assert_eq!(
            dev().check_url(Valkey, "redis://cache:6379", Profile::Dev),
            Ok(Transport::Plaintext)
        );
        assert_eq!(
            dev().check_url(S3, "http://minio:9000", Profile::Dev),
            Ok(Transport::Plaintext)
        );
        assert!(Valkey.plaintext_warning().contains("NOT encrypted"));
    }

    #[test]
    fn schemes_per_channel() {
        let tls = InternalTls::default();
        let ok = |c, url| tls.check_url(c, url, Profile::Prod);
        assert_eq!(ok(Valkey, "rediss://cache:6380/0"), Ok(Transport::Tls));
        assert_eq!(ok(Valkey, "VALKEYS://cache"), Ok(Transport::Tls));
        assert_eq!(ok(S3, "https://s3.example.org/bucket"), Ok(Transport::Tls));
        assert_eq!(ok(Node, "smtps://mailbox-1:465"), Ok(Transport::Tls));
        assert_eq!(ok(Node, "https://mailbox-1:8443"), Ok(Transport::Tls));
        assert!(ok(S3, "file:///var/lib/thepost").is_err());
        assert!(ok(Valkey, "cache:6379").is_err());
        assert!(ok(Postgres, "https://db").is_err());
    }

    #[test]
    fn postgres_sslmode() {
        let tls = InternalTls::default();
        let pg = |url| tls.check_url(Postgres, url, Profile::Prod);
        assert_eq!(pg("postgres://u:p@db/thepost"), Ok(Transport::Tls));
        assert_eq!(
            pg("postgresql://db/thepost?application_name=tp&sslmode=verify-full"),
            Ok(Transport::Tls)
        );
        assert!(pg("postgres://db/thepost?sslmode=disable").is_err());
        let weak = pg("postgres://db/thepost?sslmode=require").expect_err("require");
        assert!(
            matches!(&weak, Error::Invalid(m) if m.contains("verify-full")),
            "{weak:?}"
        );
        assert!(pg("postgres://db/thepost?sslmode=verify-ca").is_err());
        assert!(pg("postgres://db/thepost?sslmode=bogus").is_err());
        assert_eq!(
            dev().check_url(Postgres, "postgres://db/x?sslmode=disable", Profile::Dev),
            Ok(Transport::Plaintext)
        );
    }

    #[test]
    fn section_fields() {
        let tls: InternalTls = serde_json::from_str(
            r#"{"ca_file": "/etc/thepost/ca.pem", "cert_file": "/etc/thepost/node.pem",
                "key_file": "/etc/thepost/node.key", "server_name": "db.internal"}"#,
        )
        .expect("parse");
        assert_eq!(tls.validate(Node, Profile::Prod), Ok(()));
        assert_eq!(tls.server_name.as_deref(), Some("db.internal"));
        assert!(serde_json::from_str::<InternalTls>(r#"{"bogus": 1}"#).is_err());

        let half = InternalTls {
            cert_file: Some("/etc/thepost/node.pem".into()),
            ..InternalTls::default()
        };
        assert!(half.validate(Node, Profile::Prod).is_err());

        assert_eq!(Profile::default(), Profile::Prod);
        assert_eq!(
            serde_json::from_str::<Profile>(r#""dev""#).expect("dev"),
            Profile::Dev
        );
        assert!(serde_json::from_str::<Profile>(r#""staging""#).is_err());
    }
}
