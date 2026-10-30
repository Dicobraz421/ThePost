//! Общие типы секций конфигурации (CONTRACTS.md#Config, SRS 5.5).
//!
//! Длительности в config.yaml — только строки вида `5s`, `250ms`, `10m`, `24h`, `1h30m` (решение 2026-10-30, #43);
//! поля называются без единицы: `acquire_timeout: 5s`. Секреты — только ссылки [`crate::secrets::SecretRef`].

pub mod duration {
    //! Длительности в формате humantime: `serde(with = …)` для `Duration` и `Option<Duration>`, JSON Schema поля.
    //!
    //! Запись — последовательность `<целое><единица>` без пробелов, единицы по убыванию, каждая не более раза:
    //! `d`, `h`, `m`, `s`, `ms`, `us`, `ns`. Ноль — `0s`. Число без единицы, пробелы, знак, дробь и переполнение — ошибка.
    //!
    //! ```
    //! use std::time::Duration;
    //! use schemars::JsonSchema;
    //! use serde::Deserialize;
    //! use tp_contracts::config::duration;
    //!
    //! #[derive(Debug, Deserialize, JsonSchema)]
    //! struct PoolConfig {
    //!     /// Ожидание соединения из пула.
    //!     #[serde(with = "duration")]
    //!     #[schemars(with = "duration::HumanDuration")]
    //!     acquire_timeout: Duration,
    //!     /// Простой соединения до закрытия; не задано — без ограничения.
    //!     #[serde(default, with = "duration::option")]
    //!     #[schemars(with = "Option<duration::HumanDuration>")]
    //!     idle_timeout: Option<Duration>,
    //! }
    //!
    //! let c: PoolConfig = serde_json::from_str(r#"{"acquire_timeout": "1m30s"}"#)?;
    //! assert_eq!(c.acquire_timeout, Duration::from_secs(90));
    //! assert_eq!(c.idle_timeout, None);
    //! assert_eq!(duration::format(Duration::from_millis(1500)), "1s500ms");
    //! assert!(duration::parse("5").is_err());
    //! # Ok::<(), serde_json::Error>(())
    //! ```

    use std::borrow::Cow;
    use std::fmt;
    use std::str::FromStr;
    use std::time::Duration;

    use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
    use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

    use crate::error::{Error, Result};

    /// Шаблон JSON Schema: та же грамматика, что у [`parse`] (без проверки переполнения).
    pub const PATTERN: &str =
        "^(?=[0-9])([0-9]+d)?([0-9]+h)?([0-9]+m)?([0-9]+s)?([0-9]+ms)?([0-9]+us)?([0-9]+ns)?$";

    /// Единицы по убыванию: обозначение и длительность в наносекундах.
    const UNITS: [(&str, u128); 7] = [
        ("d", 86_400_000_000_000),
        ("h", 3_600_000_000_000),
        ("m", 60_000_000_000),
        ("s", 1_000_000_000),
        ("ms", 1_000_000),
        ("us", 1_000),
        ("ns", 1),
    ];

    const NANOS_PER_SEC: u128 = 1_000_000_000;

    /// Разбирает длительность (`5s`, `250ms`, `1h30m`); ошибка — `Error::Invalid`.
    pub fn parse(text: &str) -> Result<Duration> {
        let invalid = |why: &str| {
            Error::Invalid(format!(
                "duration {text:?}: {why}; expected e.g. 5s, 250ms, 10m, 24h, 1h30m"
            ))
        };
        if text.is_empty() {
            return Err(invalid("empty"));
        }
        let mut rest = text;
        let mut total: u128 = 0;
        // Индекс следующей допустимой единицы: единицы идут по убыванию, без повторов.
        let mut next_unit = 0;
        while !rest.is_empty() {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            if digits == 0 {
                return Err(invalid("number expected"));
            }
            let (number, tail) = rest.split_at(digits);
            let letters = tail.bytes().take_while(u8::is_ascii_lowercase).count();
            let (unit, tail) = tail.split_at(letters);
            if unit.is_empty() {
                return Err(invalid("unit expected (d, h, m, s, ms, us, ns)"));
            }
            let index = UNITS
                .iter()
                .position(|(u, _)| *u == unit)
                .ok_or_else(|| invalid("unknown unit"))?;
            if index < next_unit {
                return Err(invalid("units must go from largest to smallest, each once"));
            }
            next_unit = index + 1;
            let value: u128 = number.parse().map_err(|_| invalid("overflow"))?;
            total = value
                .checked_mul(UNITS[index].1)
                .and_then(|n| total.checked_add(n))
                .ok_or_else(|| invalid("overflow"))?;
            rest = tail;
        }
        let secs = u64::try_from(total / NANOS_PER_SEC).map_err(|_| invalid("overflow"))?;
        // Остаток меньше 10^9, помещается в u32.
        let nanos = (total % NANOS_PER_SEC) as u32;
        Ok(Duration::new(secs, nanos))
    }

    /// Каноническая запись: крупные единицы первыми, без нулевых, ноль — `0s`; `parse(format(d)) == d`.
    pub fn format(d: Duration) -> String {
        let mut rest = d.as_nanos();
        if rest == 0 {
            return "0s".to_owned();
        }
        let mut out = String::new();
        for (unit, nanos) in UNITS {
            let n = rest / nanos;
            if n > 0 {
                out.push_str(&n.to_string());
                out.push_str(unit);
                rest %= nanos;
            }
        }
        out
    }

    /// `serde(with = "duration")`: запись строкой [`format`].
    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&format(*d))
    }

    /// `serde(with = "duration")`: разбор строки [`parse`]; число вместо строки — ошибка.
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> std::result::Result<Duration, D::Error> {
        HumanDuration::deserialize(d).map(|h| h.0)
    }

    pub mod option {
        //! `serde(default, with = "duration::option")` для `Option<Duration>`: `null` или отсутствие поля — `None`.

        use std::time::Duration;

        use serde::{Deserialize, Deserializer, Serializer};

        use super::HumanDuration;

        /// Запись `Some` строкой, `None` — `null`.
        pub fn serialize<S: Serializer>(
            d: &Option<Duration>,
            s: S,
        ) -> std::result::Result<S::Ok, S::Error> {
            match d {
                Some(d) => super::serialize(d, s),
                None => s.serialize_none(),
            }
        }

        /// Разбор строки или `null`.
        pub fn deserialize<'de, D: Deserializer<'de>>(
            d: D,
        ) -> std::result::Result<Option<Duration>, D::Error> {
            Ok(Option::<HumanDuration>::deserialize(d)?.map(|h| h.0))
        }
    }

    /// Длительность со строковой записью: тип для `schemars(with = …)` или поле секции напрямую.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
    pub struct HumanDuration(pub Duration);

    impl From<Duration> for HumanDuration {
        fn from(d: Duration) -> Self {
            HumanDuration(d)
        }
    }

    impl From<HumanDuration> for Duration {
        fn from(h: HumanDuration) -> Self {
            h.0
        }
    }

    impl FromStr for HumanDuration {
        type Err = Error;

        fn from_str(s: &str) -> Result<Self> {
            parse(s).map(HumanDuration)
        }
    }

    impl fmt::Display for HumanDuration {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str(&format(self.0))
        }
    }

    impl Serialize for HumanDuration {
        fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
            serialize(&self.0, s)
        }
    }

    impl<'de> Deserialize<'de> for HumanDuration {
        fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
            struct Visitor;

            impl de::Visitor<'_> for Visitor {
                type Value = HumanDuration;

                fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str("a duration string such as 5s, 250ms, 10m, 24h, 1h30m")
                }

                fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                    parse(v).map(HumanDuration).map_err(|e| match e {
                        Error::Invalid(m) => E::custom(m),
                        other => E::custom(other),
                    })
                }
            }

            d.deserialize_str(Visitor)
        }
    }

    impl JsonSchema for HumanDuration {
        fn inline_schema() -> bool {
            true
        }

        fn schema_name() -> Cow<'static, str> {
            Cow::Borrowed("Duration")
        }

        fn json_schema(_: &mut SchemaGenerator) -> Schema {
            json_schema!({
                "type": "string",
                "pattern": PATTERN,
                "examples": ["5s", "250ms", "10m", "24h", "1h30m"],
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use schemars::JsonSchema;
    use serde::{Deserialize, Serialize};
    use serde_json::json;

    use super::duration::{self, HumanDuration, PATTERN, format, parse};

    #[derive(Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct Section {
        /// Таймаут.
        #[serde(with = "duration")]
        #[schemars(with = "duration::HumanDuration")]
        timeout: Duration,
        /// Необязательный период.
        #[serde(default, with = "duration::option")]
        #[schemars(with = "Option<duration::HumanDuration>")]
        period: Option<Duration>,
    }

    #[test]
    fn parse_examples() {
        let ok = |s| parse(s).expect(s);
        assert_eq!(ok("5s"), Duration::from_secs(5));
        assert_eq!(ok("250ms"), Duration::from_millis(250));
        assert_eq!(ok("10m"), Duration::from_secs(600));
        assert_eq!(ok("24h"), Duration::from_secs(86_400));
        assert_eq!(ok("1h30m"), Duration::from_secs(5_400));
        assert_eq!(ok("2d"), Duration::from_secs(172_800));
        assert_eq!(ok("1s500ms"), Duration::from_millis(1_500));
        assert_eq!(ok("0s"), Duration::ZERO);
        assert_eq!(ok("90s"), Duration::from_secs(90));
        assert_eq!(ok("1us7ns"), Duration::from_nanos(1_007));
        assert_eq!(ok("007s"), Duration::from_secs(7));
    }

    #[test]
    fn parse_rejects() {
        for bad in [
            "", "5", "5 sec", "5 s", "5sec", "-1s", "+1s", "1.5s", "s", "1h 30m", " 5s", "5s ",
            "30m1h", "1s1s", "1S", "5x", "1h30", "ms",
        ] {
            assert!(parse(bad).is_err(), "{bad:?} must be rejected");
        }
        // Переполнение: u64 секунд и само число.
        assert!(parse("18446744073709551616s").is_err());
        assert!(parse("213503982334602d").is_err());
        assert!(parse("99999999999999999999999999999999999999999d").is_err());
        assert_eq!(
            parse("18446744073709551615s").expect("max"),
            Duration::from_secs(u64::MAX)
        );
        assert!(parse("18446744073709551615s1000ms").is_err());
    }

    #[test]
    fn format_round_trip() {
        for (d, s) in [
            (Duration::ZERO, "0s"),
            (Duration::from_millis(250), "250ms"),
            (Duration::from_secs(90), "1m30s"),
            (Duration::from_secs(5_400), "1h30m"),
            (Duration::from_secs(86_400 + 1), "1d1s"),
            (Duration::new(5, 1), "5s1ns"),
            (Duration::MAX, "213503982334601d7h15s999ms999us999ns"),
        ] {
            assert_eq!(format(d), s);
            assert_eq!(parse(s).expect(s), d);
        }
        let h: HumanDuration = "1h30m".parse().expect("parse");
        assert_eq!(h.to_string(), "1h30m");
    }

    #[test]
    fn serde_round_trip() {
        let s = Section {
            timeout: Duration::from_millis(1_500),
            period: Some(Duration::from_secs(600)),
        };
        let v = serde_json::to_value(&s).expect("ser");
        assert_eq!(v, json!({"timeout": "1s500ms", "period": "10m"}));
        assert_eq!(serde_json::from_value::<Section>(v).expect("de"), s);

        let s: Section = serde_json::from_value(json!({"timeout": "5s"})).expect("default");
        assert_eq!(s.period, None);
        let s: Section =
            serde_json::from_value(json!({"timeout": "5s", "period": null})).expect("null");
        assert_eq!(s.period, None);

        for bad in [
            json!({"timeout": 5}),
            json!({"timeout": "5"}),
            json!({"timeout": "-1s"}),
        ] {
            assert!(serde_json::from_value::<Section>(bad).is_err());
        }
        let e = serde_json::from_value::<Section>(json!({"timeout": "5 sec"}))
            .expect_err("5 sec")
            .to_string();
        assert!(e.contains("expected e.g. 5s"), "{e}");
    }

    #[test]
    fn schema_snapshot() {
        let schema = serde_json::to_value(schemars::schema_for!(Section)).expect("schema");
        let examples = json!(["5s", "250ms", "10m", "24h", "1h30m"]);
        assert_eq!(
            schema,
            json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "title": "Section",
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "timeout": {
                        "description": "Таймаут.",
                        "type": "string",
                        "pattern": PATTERN,
                        "examples": examples,
                    },
                    "period": {
                        "description": "Необязательный период.",
                        "type": ["string", "null"],
                        "pattern": PATTERN,
                        "examples": examples,
                        "default": null,
                    },
                },
                "required": ["timeout"],
            })
        );
    }
}
