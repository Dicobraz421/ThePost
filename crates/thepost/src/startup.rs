//! Подготовка процесса до запуска рантайма tokio (PLT-16, SYS-07): проверка учётной записи, ограничения ролей
//! (seccomp и Landlock) по набору ролей узла. Ограничения необратимы и действуют на все потоки, поэтому
//! применяются строго до создания потоков рантайма.

use std::path::{Path, PathBuf};

use tp_contracts::Result;
use tp_contracts::tls::InternalTls;
use tp_meta::pool::PoolConfig;
use tp_platform::Config;
use tp_platform::privileges;
use tp_sys::{ConfineConfig, ConfineReport, Confinement, RolePaths, SeccompMode};

use crate::sections::{
    self, AUTH, AuthSection, BLOB, BUS, BlobLocation, BlobSection, BusSection, SEARCH,
};

/// Учётная запись процесса и ограничения ролей для `thepost serve` по config.yaml `path`.
pub fn prepare_serve(path: &Path) -> Result<ConfineReport> {
    let cfg = sections::registry()?.load(path)?;
    privileges::check_user(&cfg)?;
    let text = std::fs::read_to_string(path).unwrap_or_default();
    confine(&cfg, role_paths(&cfg, path), has_section(&text, "confine"))
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
/// поиска, делегированная cgroup подпроцессов, файлы TLS внутренних каналов (CA, сертификат и ключ клиента).
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
    if let Ok(bus) = cfg.section::<BusSection>(BUS) {
        tls_files(&bus.valkey.tls, &mut paths.config);
    }
    if let Ok(auth) = cfg.section::<AuthSection>(AUTH) {
        if let Some(ldap) = &auth.ldap {
            tls_files(&ldap.tls, &mut paths.config);
        }
        if let Some(keytab) = auth.gssapi.as_ref().and_then(|g| g.keytab.clone()) {
            paths.secrets.push(keytab);
        }
    }
    paths
}

fn tls_files(tls: &InternalTls, out: &mut Vec<PathBuf>) {
    for file in [&tls.ca_file, &tls.cert_file, &tls.key_file]
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
}
