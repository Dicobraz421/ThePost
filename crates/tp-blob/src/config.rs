//! Разбор секций tp-blob из config.yaml (`LocalConfig`, `S3Config`, `CacheConfig`, `GcConfig`, `OrphanConfig`).
//!
//! Длительность — строка humantime без единицы в имени поля (`stale_tmp: 24h`, решение #43, CTR-13);
//! число без единицы и прежние имена с единицей (`stale_tmp_secs`) — ошибка разбора.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use serde::de::DeserializeOwned;
    use tp_contracts::secrets::SecretRef;

    use crate::{CacheConfig, GcConfig, LocalConfig, OrphanConfig, S3Config, S3Credentials};

    fn parse<T: DeserializeOwned>(yaml: &str) -> Result<T, String> {
        serde_yaml_ng::from_str(yaml).map_err(|e| e.to_string())
    }

    fn r(s: &str) -> SecretRef {
        s.parse().expect("ссылка")
    }

    #[test]
    fn sections_from_yaml() {
        let local: LocalConfig =
            parse("root: /srv/blob\nfsync: false\nstale_tmp: 1h30m\n").expect("local");
        assert_eq!(
            local,
            LocalConfig {
                root: PathBuf::from("/srv/blob"),
                fsync: false,
                stale_tmp: Duration::from_secs(5400),
                ..LocalConfig::default()
            }
        );

        let cache: CacheConfig = parse("dir: /nvme/cache\nmax_bytes: 1024\n").expect("cache");
        assert_eq!(
            (cache.dir, cache.max_bytes),
            (PathBuf::from("/nvme/cache"), 1024)
        );

        let gc: GcConfig =
            parse("window: 14d\nbatch: 10\ninterval: 10m\nlease_ttl: 90s\n").expect("gc");
        assert_eq!(
            gc,
            GcConfig {
                window: Duration::from_secs(14 * 24 * 3600),
                batch: 10,
                interval: Duration::from_secs(600),
                lease_ttl: Duration::from_secs(90),
                ..GcConfig::default()
            }
        );

        let orphans: OrphanConfig =
            parse("grace: 2h\nprefix: \"t1/\"\ninterval: 12h\nlease_ttl: 1m\n").expect("orphans");
        assert_eq!(
            orphans,
            OrphanConfig {
                grace: Duration::from_secs(7200),
                prefix: "t1/".into(),
                interval: Duration::from_secs(12 * 3600),
                lease_ttl: Duration::from_secs(60),
                ..OrphanConfig::default()
            }
        );

        let s3: S3Config = parse(
            "endpoint: https://seaweedfs:8333\n\
             bucket: mail\n\
             credentials:\n  \
               access_key_id: { secret: \"thepost/s3#access_key_id\" }\n  \
               secret_access_key: { secret: \"thepost/s3#secret_access_key\" }\n\
             retry_init_backoff: 250ms\n\
             retry_max_backoff: 10s\n\
             retry_timeout: 2m\n\
             request_timeout: 20s\n\
             connect_timeout: 1s500ms\n",
        )
        .expect("s3");
        assert_eq!(
            s3,
            S3Config {
                endpoint: Some("https://seaweedfs:8333".into()),
                bucket: "mail".into(),
                credentials: Some(S3Credentials {
                    access_key_id: r("thepost/s3#access_key_id"),
                    secret_access_key: r("thepost/s3#secret_access_key"),
                }),
                retry_init_backoff: Duration::from_millis(250),
                retry_max_backoff: Duration::from_secs(10),
                retry_timeout: Duration::from_secs(120),
                request_timeout: Duration::from_secs(20),
                connect_timeout: Duration::from_millis(1500),
                ..S3Config::default()
            }
        );
    }

    #[test]
    fn empty_sections_are_defaults() {
        assert_eq!(parse::<LocalConfig>("{}"), Ok(LocalConfig::default()));
        assert_eq!(parse::<CacheConfig>("{}"), Ok(CacheConfig::default()));
        assert_eq!(parse::<GcConfig>("{}"), Ok(GcConfig::default()));
        assert_eq!(parse::<OrphanConfig>("{}"), Ok(OrphanConfig::default()));
        assert_eq!(parse::<S3Config>("{}"), Ok(S3Config::default()));
    }

    #[test]
    fn bad_durations_rejected() {
        for (bad, why) in [
            ("stale_tmp: 3600", "число без единицы"),
            ("stale_tmp_secs: 3600", "прежнее имя с единицей"),
        ] {
            assert!(parse::<LocalConfig>(bad).is_err(), "{why}: {bad}");
        }
        for bad in ["request_timeout: 30000", "request_timeout_ms: 30000"] {
            assert!(parse::<S3Config>(bad).is_err(), "{bad}");
        }
        assert!(parse::<GcConfig>("window_secs: 3600").is_err());
        assert!(parse::<OrphanConfig>("grace: 5").is_err());
    }

    #[test]
    fn s3_keys_only_as_secret_refs() {
        let plain = "credentials:\n  access_key_id: AKIA0SECRET\n  \
                     secret_access_key: { secret: \"thepost/s3#secret_access_key\" }\n";
        let e = parse::<S3Config>(plain).expect_err("ключ открытым текстом");
        assert!(e.contains("plaintext"), "{e}");
        assert!(
            !e.contains("AKIA0SECRET"),
            "значение не попадает в ошибку: {e}"
        );

        // Полей значений ключей нет.
        let flat = "access_key_id: AKIA0SECRET\nsecret_access_key: hunter2\n";
        let e = parse::<S3Config>(flat).expect_err("поле значения");
        assert!(!e.contains("AKIA0SECRET") && !e.contains("hunter2"), "{e}");

        // Ключи — только парой.
        let half = "credentials:\n  access_key_id: { secret: \"thepost/s3#access_key_id\" }\n";
        assert!(parse::<S3Config>(half).is_err());
    }

    #[test]
    fn schema_durations_are_strings() {
        let schema = serde_json::to_value(schemars::schema_for!(S3Config)).expect("схема");
        let props = &schema["properties"];
        assert_eq!(props["request_timeout"]["type"], "string");
        assert!(props.get("request_timeout_ms").is_none());
        assert!(props.get("access_key_id").is_none());
        assert!(props.get("credentials").is_some());
        let local = serde_json::to_value(schemars::schema_for!(LocalConfig)).expect("схема");
        assert_eq!(local["properties"]["stale_tmp"]["type"], "string");
    }
}
