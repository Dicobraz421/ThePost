//! Подготовка процесса до запуска рантайма tokio (PLT-16, SYS-07): проверка учётной записи, ограничения ролей
//! (seccomp и Landlock) по набору ролей узла. Ограничения необратимы и действуют на все потоки, поэтому
//! применяются строго до создания потоков рантайма. Каталоги ролей, разрешённые на запись, создаются до
//! наложения Landlock (CMP-02): правила Landlock молча пропускают несуществующие пути, и каталог, созданный
//! позже, оказался бы вне разрешённых.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use tp_contracts::Result;
use tp_contracts::tls::InternalTls;
use tp_meta::pool::PoolConfig;
use tp_platform::Config;
use tp_platform::privileges;
use tp_sys::{ConfineConfig, ConfineReport, Confinement, RolePaths, SeccompMode};

use crate::sections::{
    self, AUTH, AuthSection, BLOB, BlobLocation, BlobSection, CLUSTER, ClusterSection, SEARCH,
};

/// Учётная запись процесса и ограничения ролей для `thepost serve` по config.yaml `path`.
pub fn prepare_serve(path: &Path) -> Result<ConfineReport> {
    let cfg = sections::registry()?.load(path)?;
    privileges::check_user(&cfg)?;
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let paths = role_paths(&cfg, path);
    create_role_dirs(&paths, &prepared_dirs(&cfg))?;
    // Локальный сокет `admin init` — до Landlock (APP-19).
    crate::local_admin::bind(&cfg)?;
    confine(&cfg, paths, has_section(&text, "confine"))
}

/// Создаёт недостающие каталоги ролей, разрешённые на запись (BLOB, кэш, состояние, кэш повторов Kerberos), с правами
/// 0700 учётной записи процесса; `skip` — каталоги, которые готовит развёртывание (делегированная cgroup). Файлы
/// (TLS, секреты) не создаются, права существующих каталогов не меняются. Возвращает созданные каталоги; ошибка
/// создания (путь и причина) прерывает запуск.
pub fn create_role_dirs(paths: &RolePaths, skip: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut created = Vec::new();
    for dir in paths
        .blob
        .iter()
        .chain(&paths.cache)
        .chain(&paths.state)
        .chain(&paths.krb5_rcache)
    {
        if skip.contains(dir) || dir.as_os_str().is_empty() || dir.is_dir() {
            continue;
        }
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(|e| {
                tp_contracts::Error::Internal(format!(
                    "cannot create role directory {}: {e}",
                    dir.display()
                ))
            })?;
        created.push(dir.clone());
    }
    Ok(created)
}

/// Каталоги на запись, которые готовит развёртывание, а не процесс: делегированная cgroup подпроцессов
/// (`search.extract.sandbox.cgroup_dir`, systemd `Delegate=yes`).
fn prepared_dirs(cfg: &Config) -> Vec<PathBuf> {
    cfg.section::<tp_search::SearchConfig>(SEARCH)
        .ok()
        .and_then(|s| s.extract.sandbox.cgroup_dir.clone())
        .into_iter()
        .collect()
}

/// Ограничения ролей узла. Секции `confine` в файле нет — seccomp в режиме `log` (вызовы вне профиля
/// выполняются и пишутся в журнал аудита ядра), пока профили не проверены на стенде; Landlock — по умолчанию.
pub fn confine(cfg: &Config, paths: RolePaths, explicit: bool) -> Result<ConfineReport> {
    let mut section: ConfineConfig = (*cfg.section::<ConfineConfig>("confine")?).clone();
    if !explicit {
        section.seccomp = SeccompMode::Log;
    }
    let profile = tp_sys::Profile::for_roles(cfg.roles());
    Confinement::new(section, profile, paths)
        .apply()
        .map_err(|e| tp_contracts::Error::Internal(format!("confine: {e}")))
}

/// В тексте config.yaml есть ключ верхнего уровня `name`.
pub fn has_section(text: &str, name: &str) -> bool {
    text.lines().any(|l| {
        l.strip_prefix(name)
            .is_some_and(|rest| rest.trim_start().starts_with(':'))
    })
}

/// Каталоги ролей: секции платформы (`privileges::role_paths`), каталог BLOB (`blob.url` `file://`), индексы
/// поиска, делегированная cgroup подпроцессов, файлы TLS внутренних каналов (CA, сертификат и ключ клиента), при
/// входе GSSAPI (`auth.gssapi`) — keytab и каталог кэша повторов Kerberos (`KRB5RCACHEDIR` задаёт развёртывание).
pub fn role_paths(cfg: &Config, config_file: &Path) -> RolePaths {
    let mut paths = privileges::role_paths(cfg, config_file);
    if let Ok(blob) = cfg.section::<BlobSection>(BLOB) {
        if let Ok(BlobLocation::Local(root)) = blob.location() {
            paths.blob.push(root);
        }
        tls_files(&blob.s3.tls, &mut paths.config);
    }
    if let Ok(search) = cfg.section::<tp_search::SearchConfig>(SEARCH) {
        paths.cache.push(search.index.dir.clone());
        if let Some(dir) = &search.extract.sandbox.cgroup_dir {
            paths.state.push(dir.clone());
        }
    }
    if let Ok(db) = cfg.section::<PoolConfig>(PoolConfig::SECTION) {
        tls_files(&db.tls, &mut paths.config);
    }
    // Межузловой канал (`cluster.rpc.tls`): файлы читаются при запуске и перечитываются под Landlock.
    if let Ok(cluster) = cfg.section::<ClusterSection>(CLUSTER) {
        tls_files(&cluster.rpc.tls, &mut paths.config);
    }
    if let Ok(auth) = cfg.section::<AuthSection>(AUTH) {
        if let Some(ldap) = &auth.ldap {
            tls_files(&ldap.tls, &mut paths.config);
        }
        if let Some(keytab) = auth.gssapi.as_ref().and_then(|g| g.keytab.clone()) {
            paths.secrets.push(keytab);
        }
        if auth.gssapi.is_some() {
            paths.krb5_rcache = Some(tp_sys::KRB5_RCACHE_DIR.into());
        }
    }
    paths
}

/// Файлы секции TLS внутреннего канала. Ключ по ссылке на секрет (`key`, CTR-52) читается из хранилища секретов:
/// файла ключа на диске нет и в пути доступа роли он не входит (APP-36).
fn tls_files(tls: &InternalTls, out: &mut Vec<PathBuf>) {
    let key_file = if tls.key.is_some() {
        &None
    } else {
        &tls.key_file
    };
    for file in [&tls.ca_file, &tls.cert_file, key_file]
        .into_iter()
        .flatten()
    {
        if !out.contains(file) {
            out.push(file.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_key_detection() {
        assert!(has_section(
            "domain: x\nconfine:\n  seccomp: enforce\n",
            "confine"
        ));
        assert!(has_section("confine : { seccomp: off }\n", "confine"));
        assert!(!has_section(
            "domain: x\n  confine: 1\n# confine: no\n",
            "confine"
        ));
        assert!(!has_section("confined: 1\n", "confine"));
    }

    #[test]
    fn missing_index_dir_is_created_and_allowed() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().expect("tempdir");
        let index = tmp.path().join("search/index");
        let cgroup = tmp.path().join("cgroup");
        let blob = tmp.path().join("blob");
        let cfg = sections::registry()
            .expect("registry")
            .parse(
                &format!(
                    "domain: example.org\nprofile: dev\n\
                     database:\n  url: postgres://x@y/z\n\
                     blob:\n  url: file://{}\n\
                     search:\n  index: {{ dir: {} }}\n  extract: {{ sandbox: {{ cgroup_dir: {} }} }}\n",
                    blob.display(),
                    index.display(),
                    cgroup.display()
                ),
                Vec::new(),
            )
            .expect("config");
        let paths = role_paths(&cfg, Path::new("/etc/thepost/config.yaml"));
        assert!(!index.exists());
        let created = create_role_dirs(&paths, &prepared_dirs(&cfg)).expect("dirs");
        assert_eq!(created, vec![blob.clone(), index.clone()]);
        for dir in [&blob, &index] {
            assert!(dir.is_dir());
            let mode = std::fs::metadata(dir).expect("meta").permissions().mode();
            assert_eq!(mode & 0o077, 0, "{}: mode {mode:o}", dir.display());
        }
        assert!(paths.cache.contains(&index));
        assert!(paths.blob.contains(&blob));
        assert!(!cgroup.exists(), "cgroup is prepared by deployment");
        // Повторный запуск: каталог есть — ничего не создаётся.
        assert!(
            create_role_dirs(&paths, &prepared_dirs(&cfg))
                .expect("dirs")
                .is_empty()
        );
    }

    /// APP-68: при входе GSSAPI (`auth.gssapi`) каталог кэша повторов Kerberos — в путях роли и создаётся до Landlock;
    /// без секции — его нет.
    #[test]
    fn krb5_rcache_dir_with_gssapi() {
        use std::os::unix::fs::PermissionsExt;

        let parse = |auth: &str| {
            let cfg = sections::registry()
                .expect("registry")
                .parse(
                    &format!(
                        "domain: example.org\nprofile: dev\n\
                         database:\n  url: postgres://x@y/z\n\
                         blob:\n  url: file:///var/lib/thepost/blob\n{auth}"
                    ),
                    Vec::new(),
                )
                .expect("config");
            role_paths(&cfg, Path::new("/etc/thepost/config.yaml"))
        };
        let with = parse("auth:\n  gssapi: { hostname: mail.example.org }\n");
        assert_eq!(
            with.krb5_rcache,
            Some(PathBuf::from(tp_sys::KRB5_RCACHE_DIR))
        );
        assert_eq!(parse("").krb5_rcache, None);

        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("krb5rcache");
        let paths = RolePaths {
            krb5_rcache: Some(dir.clone()),
            ..RolePaths::default()
        };
        let created = create_role_dirs(&paths, &[]).expect("dirs");
        assert_eq!(created, vec![dir.clone()]);
        let mode = std::fs::metadata(&dir).expect("meta").permissions().mode();
        assert_eq!(mode & 0o077, 0, "mode {mode:o}");
    }

    #[test]
    fn uncreatable_dir_fails_with_path() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file = tmp.path().join("file");
        std::fs::write(&file, b"x").expect("write");
        let dir = file.join("index");
        let paths = RolePaths {
            cache: vec![dir.clone()],
            ..RolePaths::default()
        };
        let err = create_role_dirs(&paths, &[]).expect_err("must fail");
        assert!(
            err.to_string().contains(&dir.display().to_string()),
            "{err}"
        );
    }

    #[test]
    fn paths_include_blob_cache_and_channel_files() {
        let cfg = sections::registry()
            .expect("registry")
            .parse(
                "domain: example.org\nprofile: dev\n\
                 database:\n  url: postgres://x@y/z\n  tls: { ca_file: /etc/thepost/pg-ca.pem }\n\
                 blob:\n  url: file:///var/lib/thepost/blob\n\
                 search:\n  index: { dir: /var/cache/thepost/index }\n",
                Vec::new(),
            )
            .expect("config");
        let paths = role_paths(&cfg, Path::new("/etc/thepost/config.yaml"));
        assert!(paths.blob.contains(&PathBuf::from("/var/lib/thepost/blob")));
        assert!(
            paths
                .cache
                .contains(&PathBuf::from("/var/cache/thepost/index"))
        );
        assert!(
            paths
                .config
                .contains(&PathBuf::from("/etc/thepost/pg-ca.pem"))
        );
        assert!(
            paths
                .config
                .contains(&PathBuf::from("/etc/thepost/config.yaml"))
        );
    }

    /// APP-36: файлы межузлового канала и СУБД входят в пути роли; при ключе из хранилища секретов (`key`)
    /// файла ключа среди них нет.
    #[test]
    fn channel_key_from_secret_store_has_no_file_path() {
        let parse = |db_key: &str, rpc_key: &str| {
            let cfg = sections::registry()
                .expect("registry")
                .parse(
                    &format!(
                        "domain: example.org\nprofile: dev\n\
                         database:\n  url: postgres://x@y/z\n  tls:\n    ca_file: /etc/thepost/pg-ca.pem\n    \
                         cert_file: /etc/thepost/pg.pem\n    {db_key}\n\
                         blob:\n  url: file:///var/lib/thepost/blob\n\
                         cluster:\n  rpc:\n    tls:\n      ca_file: /etc/thepost/ca.pem\n      \
                         cert_file: /etc/thepost/node.pem\n      {rpc_key}\n"
                    ),
                    Vec::new(),
                )
                .expect("config");
            role_paths(&cfg, Path::new("/etc/thepost/config.yaml")).config
        };
        let files = parse(
            "key_file: /etc/thepost/pg-key.pem",
            "key_file: /etc/thepost/node-key.pem",
        );
        for file in [
            "pg-ca.pem",
            "pg.pem",
            "pg-key.pem",
            "ca.pem",
            "node.pem",
            "node-key.pem",
        ] {
            assert!(
                files.contains(&Path::new("/etc/thepost").join(file)),
                "{file}: {files:?}"
            );
        }
        let files = parse(
            "key: { secret: \"thepost/db#key\" }",
            "key: { secret: \"thepost/node#key\" }",
        );
        assert!(files.contains(&PathBuf::from("/etc/thepost/node.pem")));
        assert!(files.contains(&PathBuf::from("/etc/thepost/pg.pem")));
        assert!(
            !files
                .iter()
                .any(|f| f.ends_with("pg-key.pem") || f.ends_with("node-key.pem")),
            "{files:?}"
        );
    }
}
