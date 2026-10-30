//! Хранилище секретов и ссылки на секреты (CONTRACTS.md#Secrets; решение 2026-10-30).
//!
//! Секреты (пароли PostgreSQL и Valkey, ключи S3, DKIM, ACME, подписи токенов, LDAP/OIDC) хранятся только во внешнем
//! хранилище: OpenBao или HashiCorp Vault (API Vault). В config.yaml и в таблицах БД — только ссылки [`SecretRef`]
//! вида `{ secret: "<путь>#<поле>" }`; строка или число на месте ссылки — ошибка разбора (секрет открытым текстом).
//! Значение — [`Secret`]: скрыто в `Debug`/`Display`, обнуляется при удалении, доступ — [`Secret::expose`].
//!
//! ```
//! use serde::Deserialize;
//! use tp_contracts::secrets::SecretRef;
//!
//! #[derive(Deserialize)]
//! struct DbConfig {
//!     password: SecretRef,
//! }
//!
//! let c: DbConfig = serde_json::from_str(r#"{"password": {"secret": "thepost/db#password"}}"#)?;
//! assert_eq!(c.password.path(), "thepost/db");
//! assert_eq!(c.password.field(), "password");
//! assert!(serde_json::from_str::<DbConfig>(r#"{"password": "hunter2"}"#).is_err());
//! # Ok::<(), serde_json::Error>(())
//! ```

use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use async_trait::async_trait;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::error::{Error, Result};

/// Шаблон ссылки для JSON Schema (сегменты `.` и `..` дополнительно отвергает разбор).
pub const SECRET_REF_PATTERN: &str = "^[A-Za-z0-9_.-]+(/[A-Za-z0-9_.-]+)*#[A-Za-z0-9_.-]+$";

/// Наибольшая длина ссылки `<путь>#<поле>`, байт.
const MAX_REF_LEN: usize = 512;

/// Сообщение об ошибке на месте ссылки (без значения: это может быть сам секрет).
const PLAINTEXT: &str =
    "secret in plaintext is not allowed; use a reference { secret: \"<path>#<field>\" }";

/// Ссылка на поле секрета в хранилище: `<путь>#<поле>`, например `thepost/db#password`.
///
/// Путь — сегменты `[A-Za-z0-9_.-]+` через `/` (без пустых, `.` и `..`), поле — `[A-Za-z0-9_.-]+`.
/// В конфигурации — объект `{ secret: "<путь>#<поле>" }`, в таблицах БД — строка ([`fmt::Display`], [`FromStr`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SecretRef {
    path: String,
    field: String,
}

impl SecretRef {
    /// Ссылка из пути и поля; ошибка — `Invalid`.
    pub fn new(path: impl Into<String>, field: impl Into<String>) -> Result<Self> {
        let (path, field) = (path.into(), field.into());
        let segment = |s: &str| {
            !s.is_empty()
                && s != "."
                && s != ".."
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
        };
        if path.len() + field.len() + 1 > MAX_REF_LEN
            || !path.split('/').all(segment)
            || !segment(&field)
        {
            return Err(Error::Invalid(
                "invalid secret reference; expected <path>#<field>, e.g. thepost/db#password"
                    .into(),
            ));
        }
        Ok(SecretRef { path, field })
    }

    /// Путь секрета в хранилище (для KV v2 — без `data/`).
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Поле секрета.
    pub fn field(&self) -> &str {
        &self.field
    }
}

impl FromStr for SecretRef {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        match s.split_once('#') {
            Some((path, field)) => SecretRef::new(path, field),
            None => SecretRef::new("", ""),
        }
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}#{}", self.path, self.field)
    }
}

impl Serialize for SecretRef {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        let mut map = s.serialize_map(Some(1))?;
        map.serialize_entry("secret", &self.to_string())?;
        map.end()
    }
}

impl<'de> Deserialize<'de> for SecretRef {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            secret: String,
        }

        struct Visitor;

        impl Visitor {
            fn plaintext<T, E: de::Error>(self) -> std::result::Result<T, E> {
                Err(E::custom(PLAINTEXT))
            }
        }

        // Скалярное значение на месте ссылки — секрет открытым текстом; значение в ошибку не попадает.
        impl<'de> de::Visitor<'de> for Visitor {
            type Value = SecretRef;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a secret reference { secret: \"<path>#<field>\" }")
            }

            fn visit_map<A: de::MapAccess<'de>>(
                self,
                map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let wire = Wire::deserialize(de::value::MapAccessDeserializer::new(map))?;
                wire.secret.parse().map_err(|e| match e {
                    Error::Invalid(m) => de::Error::custom(m),
                    other => de::Error::custom(other),
                })
            }

            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Self::Value, E> {
                self.plaintext()
            }

            fn visit_bytes<E: de::Error>(self, _: &[u8]) -> std::result::Result<Self::Value, E> {
                self.plaintext()
            }

            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Self::Value, E> {
                self.plaintext()
            }

            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Self::Value, E> {
                self.plaintext()
            }

            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Self::Value, E> {
                self.plaintext()
            }

            fn visit_i128<E: de::Error>(self, _: i128) -> std::result::Result<Self::Value, E> {
                self.plaintext()
            }

            fn visit_u128<E: de::Error>(self, _: u128) -> std::result::Result<Self::Value, E> {
                self.plaintext()
            }

            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                self.plaintext()
            }
        }

        d.deserialize_any(Visitor)
    }
}

impl JsonSchema for SecretRef {
    fn schema_name() -> Cow<'static, str> {
        Cow::Borrowed("SecretRef")
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "Reference to a secret in the secret store (OpenBao/Vault): <path>#<field>.",
            "type": "object",
            "properties": {
                "secret": {
                    "type": "string",
                    "pattern": SECRET_REF_PATTERN,
                    "examples": ["thepost/db#password"],
                },
            },
            "required": ["secret"],
            "additionalProperties": false,
        })
    }
}

/// Аренда динамического секрета (lease): по истечении `ttl` секрет недействителен, если аренду не продлить.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretLease {
    /// Идентификатор аренды в хранилище.
    pub id: String,
    /// Срок от момента выдачи или продления.
    pub ttl: Duration,
    /// Аренду можно продлить ([`SecretStore::renew`]); иначе до истечения — прочитать секрет заново.
    pub renewable: bool,
}

/// Значение секрета: скрыто в `Debug` и `Display`, обнуляется при удалении (без копий, сделанных вызывающим).
#[derive(Clone)]
pub struct Secret {
    value: String,
    lease: Option<SecretLease>,
}

impl Secret {
    /// Статический секрет (без аренды).
    pub fn new(value: impl Into<String>) -> Self {
        Secret {
            value: value.into(),
            lease: None,
        }
    }

    /// Динамический секрет с арендой.
    pub fn with_lease(mut self, lease: SecretLease) -> Self {
        self.lease = Some(lease);
        self
    }

    /// Значение. Не писать в логи и сообщения об ошибках.
    pub fn expose(&self) -> &str {
        &self.value
    }

    /// Аренда динамического секрета; `None` — статический.
    pub fn lease(&self) -> Option<&SecretLease> {
        self.lease.as_ref()
    }

    /// Срок аренды динамического секрета.
    pub fn lease_ttl(&self) -> Option<Duration> {
        self.lease.as_ref().map(|l| l.ttl)
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Без `unsafe`: строку забираем байтами (без перевыделения) и затираем; `black_box` не даёт
        // компилятору выбросить запись как мёртвую.
        let mut bytes = std::mem::take(&mut self.value).into_bytes();
        bytes.fill(0);
        std::hint::black_box(&bytes);
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Secret")
            .field("value", &format_args!("***"))
            .field("lease", &self.lease)
            .finish()
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// Хранилище секретов. Встроенная реализация — клиент OpenBao/Vault (`tp-platform`); замена —
/// `Registry::set_secret_store`; in-memory — `tp-testkit`. Канал к хранилищу — только TLS.
///
/// Секреты читаются при запуске и при перечитывании конфигурации; аренды динамических секретов продлеваются.
#[async_trait]
pub trait SecretStore: Send + Sync + 'static {
    /// Значение поля секрета. Нет пути или поля — `NotFound`; запрещено политикой — `Forbidden`;
    /// хранилище недоступно или запечатано — `Unavailable`.
    async fn get(&self, secret: &SecretRef) -> Result<Secret>;

    /// Продлевает аренду; возвращает новый срок. Неизвестная, истёкшая или непродлеваемая аренда — `NotFound`
    /// (секрет нужно прочитать заново). По умолчанию аренд нет — `NotFound`.
    async fn renew(&self, lease: &SecretLease) -> Result<SecretLease> {
        let _ = lease;
        Err(Error::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn object_safe() {
        fn _store(_: std::sync::Arc<dyn SecretStore>) {}
    }

    #[test]
    fn parse_reference() {
        let r: SecretRef = "thepost/db#password".parse().expect("valid");
        assert_eq!((r.path(), r.field()), ("thepost/db", "password"));
        assert_eq!(r.to_string(), "thepost/db#password");
        assert_eq!(
            "shards/shard-01.eu#user_pass"
                .parse::<SecretRef>()
                .expect("dots"),
            SecretRef::new("shards/shard-01.eu", "user_pass").expect("new")
        );
        for bad in [
            "",
            "thepost/db",
            "#password",
            "thepost/db#",
            "/thepost/db#password",
            "thepost/db/#password",
            "thepost//db#password",
            "thepost/../db#password",
            "./db#password",
            "thepost/db#pass#word",
            "thepost/db#pass/word",
            "thepost db#password",
            "thepost/дб#password",
        ] {
            assert!(bad.parse::<SecretRef>().is_err(), "{bad:?}");
        }
        let long = format!("{}#f", "a".repeat(MAX_REF_LEN));
        assert!(long.parse::<SecretRef>().is_err());
    }

    #[test]
    fn config_form() {
        let r: SecretRef =
            serde_json::from_value(json!({"secret": "thepost/db#password"})).expect("ref");
        assert_eq!(r, "thepost/db#password".parse().expect("parse"));
        assert_eq!(
            serde_json::to_value(&r).expect("ser"),
            json!({"secret": "thepost/db#password"})
        );
        assert!(serde_json::from_value::<SecretRef>(json!({"secret": "nohash"})).is_err());
        assert!(
            serde_json::from_value::<SecretRef>(json!({"secret": "a#b", "value": "x"})).is_err()
        );
        assert!(serde_json::from_value::<SecretRef>(json!({})).is_err());
    }

    #[test]
    fn plaintext_rejected_without_value() {
        for v in [json!("hunter2"), json!(31337), json!(true), json!(1.5)] {
            let e = serde_json::from_value::<SecretRef>(v)
                .expect_err("plaintext")
                .to_string();
            assert!(e.contains("plaintext"), "{e}");
            assert!(!e.contains("hunter2") && !e.contains("31337"), "{e}");
        }
    }

    #[test]
    fn value_hidden() {
        let s = Secret::new("hunter2").with_lease(SecretLease {
            id: "database/creds/thepost/abc".into(),
            ttl: Duration::from_secs(3600),
            renewable: true,
        });
        assert_eq!(s.expose(), "hunter2");
        assert_eq!(s.lease_ttl(), Some(Duration::from_secs(3600)));
        let shown = format!("{s:?} {s} {:?}", s.clone());
        assert!(!shown.contains("hunter2"), "{shown}");
        assert!(shown.contains("***") && shown.contains("database/creds/thepost/abc"));
        assert_eq!(Secret::new("x").lease_ttl(), None);
    }

    #[test]
    fn schema_snapshot() {
        let schema = serde_json::to_value(schemars::schema_for!(SecretRef)).expect("schema");
        assert_eq!(
            schema,
            json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "title": "SecretRef",
                "description": "Reference to a secret in the secret store (OpenBao/Vault): <path>#<field>.",
                "type": "object",
                "properties": {
                    "secret": {
                        "type": "string",
                        "pattern": SECRET_REF_PATTERN,
                        "examples": ["thepost/db#password"],
                    },
                },
                "required": ["secret"],
                "additionalProperties": false,
            })
        );
    }

    struct Static;

    #[async_trait]
    impl SecretStore for Static {
        async fn get(&self, secret: &SecretRef) -> Result<Secret> {
            match secret.to_string().as_str() {
                "thepost/db#password" => Ok(Secret::new("pw")),
                "thepost/admin#token" => Err(Error::Forbidden),
                _ => Err(Error::NotFound),
            }
        }
    }

    #[test]
    fn store_errors_and_default_renew() {
        use futures::executor::block_on;
        let r = |s: &str| s.parse::<SecretRef>().expect("ref");
        assert_eq!(
            block_on(Static.get(&r("thepost/db#password")))
                .expect("pw")
                .expose(),
            "pw"
        );
        assert_eq!(
            block_on(Static.get(&r("thepost/admin#token"))).map(|_| ()),
            Err(Error::Forbidden)
        );
        assert_eq!(
            block_on(Static.get(&r("thepost/none#x"))).map(|_| ()),
            Err(Error::NotFound)
        );
        let lease = SecretLease {
            id: "l".into(),
            ttl: Duration::from_secs(1),
            renewable: true,
        };
        assert_eq!(block_on(Static.renew(&lease)), Err(Error::NotFound));
    }
}
