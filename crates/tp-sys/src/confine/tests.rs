//! Ограничения применяются к отдельному потоку теста (`apply_scoped(false)`): процесс тестов не затрагивается.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use landlock::RulesetStatus;
use nix::libc;
use tp_contracts::role::{RoleEntry, RolePreset};

use super::*;

fn set(entries: &[RoleEntry]) -> RoleSet {
    RoleSet::expand(entries)
}

fn edge() -> Profile {
    Profile::for_roles(&set(&[RoleEntry::Preset(RolePreset::Edge)]))
}

fn indexer() -> Profile {
    Profile::for_roles(&set(&[RoleEntry::Role(Role::Indexer)]))
}

/// Конфигурация без служебных путей: тесту доступно только то, что он разрешил явно.
fn bare(seccomp: SeccompMode, landlock: bool) -> ConfineConfig {
    ConfineConfig {
        seccomp,
        landlock,
        system_read: Vec::new(),
        system_rw: Vec::new(),
        spawn_exec: Vec::new(),
    }
}

/// Выполняет `f` в новом потоке после применения ограничений к этому потоку.
fn confined<T: Send + 'static>(
    c: Confinement,
    f: impl FnOnce(ConfineReport) -> T + Send + 'static,
) -> T {
    std::thread::spawn(move || {
        let report = c.apply_scoped(false).expect("apply");
        f(report)
    })
    .join()
    .expect("поток теста")
}

/// Временный каталог теста.
struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("tp-confine-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        Self(dir)
    }

    fn sub(&self, name: &str, file: &str) -> PathBuf {
        let dir = self.0.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(file), name).expect("write");
        dir
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn denied(r: io::Result<impl Sized>) -> bool {
    matches!(r, Err(e) if e.kind() == io::ErrorKind::PermissionDenied)
}

fn errno(r: io::Result<impl Sized>) -> Option<i32> {
    r.err().and_then(|e| e.raw_os_error())
}

#[test]
fn profile_is_union_of_roles() {
    let e = edge();
    assert!(!e.writes_data(), "edge: без записи данных");
    assert!(!e.spawns(), "edge: без подпроцессов");

    let access = Profile::for_roles(&set(&[RoleEntry::Preset(RolePreset::Access)]));
    assert!(access.writes_data());
    assert!(!access.spawns());

    let idx = indexer();
    assert!(idx.writes_data() && idx.spawns());

    let mixed = Profile::for_roles(&set(&[
        RoleEntry::Preset(RolePreset::Edge),
        RoleEntry::Role(Role::Indexer),
    ]));
    assert_eq!(mixed, idx, "объединение: edge + indexer = indexer");

    let smtp_in = Profile::for_roles(&set(&[RoleEntry::Role(Role::SmtpIn)]));
    assert!(smtp_in.writes_data(), "smtp-in не в режиме edge пишет BLOB");
}

#[test]
fn syscall_groups_by_profile() {
    let list = |p: Profile| {
        seccomp::allowed(p.needs)
            .into_iter()
            .collect::<BTreeSet<_>>()
    };
    let e = list(edge());
    let full = list(indexer());
    for nr in [
        libc::SYS_read,
        libc::SYS_openat,
        libc::SYS_accept4,
        libc::SYS_futex,
    ] {
        assert!(e.contains(&nr) && full.contains(&nr), "{nr}");
    }
    for nr in [
        libc::SYS_unlinkat,
        libc::SYS_mkdirat,
        libc::SYS_execve,
        libc::SYS_wait4,
    ] {
        assert!(!e.contains(&nr), "edge: {nr}");
        assert!(full.contains(&nr), "indexer: {nr}");
    }
    let access = list(Profile::for_roles(&set(&[RoleEntry::Preset(
        RolePreset::Access,
    )])));
    assert!(access.contains(&libc::SYS_unlinkat) && !access.contains(&libc::SYS_execve));
    for mode in [SeccompMode::Enforce, SeccompMode::Log] {
        assert_eq!(seccomp::filters(edge().needs, mode).expect("bpf").len(), 2);
    }
    assert!(
        seccomp::filters(edge().needs, SeccompMode::Off)
            .expect("bpf")
            .is_empty()
    );
}

#[test]
fn config_defaults_and_modes() {
    let cfg: ConfineConfig = serde_yaml_ng::from_str("{}").expect("yaml");
    assert_eq!(cfg, ConfineConfig::default());
    assert_eq!(cfg.seccomp, SeccompMode::Enforce);
    assert!(cfg.landlock);
    assert!(cfg.system_read.contains(&PathBuf::from("/sys/fs/cgroup")));
    let log: ConfineConfig =
        serde_yaml_ng::from_str("seccomp: log\nlandlock: false").expect("yaml");
    assert_eq!((log.seccomp, log.landlock), (SeccompMode::Log, false));
    assert!(serde_yaml_ng::from_str::<ConfineConfig>("seccomp: strict").is_err());
    assert!(serde_yaml_ng::from_str::<ConfineConfig>("paths: []").is_err());
}

/// Запрещённые профилю edge вызовы отклоняются, разрешённые (в т.ч. потоки) работают.
#[test]
fn seccomp_rejects_forbidden_calls() {
    let dir = Dir::new("seccomp");
    let file = dir.sub("conf", "a.txt").join("a.txt");
    let target = dir.0.join("new");
    let c = Confinement::new(
        bare(SeccompMode::Enforce, false),
        edge(),
        RolePaths::default(),
    );
    confined(c, move |report| {
        assert_eq!(report.seccomp, SeccompMode::Enforce);
        assert_eq!(report.landlock, LandlockOutcome::Disabled);
        assert_eq!(std::fs::read(&file).expect("чтение разрешено"), b"conf");
        assert_eq!(
            errno(std::fs::create_dir(&target)),
            Some(libc::EPERM),
            "mkdir"
        );
        assert_eq!(
            errno(std::fs::remove_file(&file)),
            Some(libc::EPERM),
            "unlink"
        );
        assert!(
            Command::new("/bin/true").status().is_err(),
            "запуск процесса запрещён"
        );
        let n = std::thread::spawn(|| 7).join().expect("потоки разрешены");
        assert_eq!(n, 7);
    });
    assert!(!dir.0.join("new").exists());
}

/// Профиль с подпроцессами разрешает запуск программы (загрузчик, библиотеки, `execve`, `wait4`).
#[test]
fn seccomp_allows_spawn_for_indexer() {
    let c = Confinement::new(
        bare(SeccompMode::Enforce, false),
        indexer(),
        RolePaths::default(),
    );
    confined(c, |_| {
        let status = Command::new("/bin/true").status().expect("spawn");
        assert!(status.success());
    });
}

/// Режим журнала не блокирует вызовы вне профиля.
#[test]
fn seccomp_log_mode_does_not_block() {
    let dir = Dir::new("seccomp-log");
    let target = dir.0.join("new");
    let c = Confinement::new(bare(SeccompMode::Log, false), edge(), RolePaths::default());
    let made = target.clone();
    confined(c, move |_| {
        std::fs::create_dir(&made).expect("mkdir в режиме журнала")
    });
    assert!(target.is_dir());
}

/// Доступ вне разрешённых каталогов отклоняется; без Landlock в ядре — предупреждение, не отказ.
#[test]
fn landlock_limits_paths() {
    let dir = Dir::new("landlock");
    let conf = dir.sub("conf", "a.txt");
    let blob = dir.sub("blob", "b.txt");
    let other = dir.sub("other", "c.txt");
    let paths = RolePaths {
        config: vec![conf.clone()],
        blob: vec![blob.clone()],
        ..RolePaths::default()
    };

    // edge: конфигурация — только чтение, каталоги данных недоступны.
    let c = Confinement::new(bare(SeccompMode::Off, true), edge(), paths.clone());
    let (conf2, blob2, other2) = (conf.clone(), blob.clone(), other.clone());
    confined(c, move |report| {
        if report.landlock == LandlockOutcome::Unsupported {
            assert!(!report.warnings().is_empty());
            return;
        }
        assert_eq!(std::fs::read(conf2.join("a.txt")).expect("conf"), b"conf");
        assert!(
            denied(std::fs::write(conf2.join("new"), b"x")),
            "запись в conf"
        );
        assert!(denied(std::fs::read(blob2.join("b.txt"))), "edge: blob");
        assert!(denied(std::fs::read(other2.join("c.txt"))), "вне ролей");
        assert!(denied(std::fs::read_dir(Path::new("/etc"))), "/etc");
    });

    // роли с данными: BLOB — чтение и запись, прочее по-прежнему закрыто.
    let access = Profile::for_roles(&set(&[RoleEntry::Preset(RolePreset::Access)]));
    let c = Confinement::new(bare(SeccompMode::Off, true), access, paths);
    let (blob2, other2) = (blob.clone(), other.clone());
    confined(c, move |report| {
        if report.landlock == LandlockOutcome::Unsupported {
            return;
        }
        std::fs::write(blob2.join("new"), b"x").expect("запись в blob");
        assert_eq!(std::fs::read(blob2.join("b.txt")).expect("blob"), b"blob");
        assert!(denied(std::fs::read(other2.join("c.txt"))), "вне ролей");
    });
    assert!(!conf.join("new").exists());
}

#[test]
fn landlock_unsupported_is_warning() {
    assert_eq!(
        fs::outcome(&RulesetStatus::NotEnforced, false),
        LandlockOutcome::Unsupported
    );
    assert_eq!(
        fs::outcome(&RulesetStatus::FullyEnforced, true),
        LandlockOutcome::Enforced { all_threads: true }
    );
    let report = ConfineReport {
        seccomp: SeccompMode::Enforce,
        landlock: LandlockOutcome::Unsupported,
    };
    assert_eq!(report.warnings().len(), 1);
    let full = ConfineReport {
        seccomp: SeccompMode::Enforce,
        landlock: LandlockOutcome::Enforced { all_threads: true },
    };
    assert!(full.warnings().is_empty());
    let thread_only = ConfineReport {
        landlock: LandlockOutcome::Enforced { all_threads: false },
        ..full
    };
    assert_eq!(thread_only.warnings().len(), 1);
}

/// Проба ABI не паникует и согласована с применением: ядро с Landlock — `Some(>=1)`,
/// без него — `None` (SYS-11).
#[test]
fn landlock_abi_matches_enforcement() {
    let abi = crate::landlock_abi();
    assert!(abi.is_none_or(|v| v >= 1), "ABI {abi:?}");
    let c = Confinement::new(bare(SeccompMode::Off, true), edge(), RolePaths::default());
    let outcome = confined(c, |report| report.landlock);
    assert_eq!(
        abi.is_some(),
        outcome != LandlockOutcome::Unsupported,
        "ABI {abi:?}, итог {outcome:?}"
    );
    assert_eq!(crate::landlock_abi(), abi, "повторная проба");
}

/// Landlock проверяет цель ссылки: ссылка из разрешённого каталога на файл другого каталога
/// читается, только если разрешён и он (SYS-08).
#[test]
fn landlock_follows_symlink_target() {
    let dir = Dir::new("symlink");
    let certs = dir.sub("certs", "local.pem");
    let share = dir.sub("share", "ca.crt");
    let link = certs.join("ca.pem");
    std::os::unix::fs::symlink(share.join("ca.crt"), &link).expect("symlink");
    let cfg = |system_read: Vec<PathBuf>| ConfineConfig {
        system_read,
        ..bare(SeccompMode::Off, true)
    };

    let c = Confinement::new(cfg(vec![certs.clone()]), edge(), RolePaths::default());
    let l = link.clone();
    confined(c, move |report| {
        if report.landlock == LandlockOutcome::Unsupported {
            return;
        }
        assert!(denied(std::fs::read(&l)), "цель ссылки вне списка");
    });

    let c = Confinement::new(
        cfg(vec![certs.clone(), share.clone()]),
        edge(),
        RolePaths::default(),
    );
    confined(c, move |report| {
        if report.landlock == LandlockOutcome::Unsupported {
            return;
        }
        assert_eq!(std::fs::read(&link).expect("ссылка"), b"share");
        assert!(std::fs::read_dir(&certs).expect("каталог ссылок").count() >= 2);
    });
}

/// С настройками по умолчанию корневой сертификат читается по ссылке `/etc/ssl/certs/*.pem`
/// (Debian/Ubuntu); без таких ссылок в системе — только состав списка.
#[test]
fn default_system_read_covers_ca_links() {
    let cfg = ConfineConfig {
        seccomp: SeccompMode::Off,
        ..ConfineConfig::default()
    };
    for p in [
        "/etc/ssl",
        "/usr/share/ca-certificates",
        "/usr/local/share/ca-certificates",
        "/etc/pki",
    ] {
        assert!(cfg.system_read.contains(&PathBuf::from(p)), "{p}");
    }
    let allowed = cfg.system_read.clone();
    let link = std::fs::read_dir("/etc/ssl/certs")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .find(|p| {
            p.extension().is_some_and(|e| e == "pem")
                && p.is_symlink()
                && std::fs::canonicalize(p).is_ok_and(|t| {
                    !t.starts_with("/etc/ssl") && allowed.iter().any(|a| t.starts_with(a))
                })
        });
    let Some(link) = link else {
        return;
    };
    let c = Confinement::new(cfg, edge(), RolePaths::default());
    confined(c, move |report| {
        if report.landlock == LandlockOutcome::Unsupported {
            return;
        }
        let pem = std::fs::read(&link).expect("корневой сертификат по ссылке");
        assert!(!pem.is_empty(), "{}", link.display());
    });
}

/// Ссылка `/etc/resolv.conf` → `/run/systemd/resolve/stub-resolv.conf` (Ubuntu 24.04, SYS-12):
/// systemd-resolved заменяет файл переименованием (новый inode) при смене поисковых доменов.
/// Правило на ссылку разрешает только прежний inode; с каталогом цели в списке новый файл читается.
#[test]
fn landlock_resolv_conf_replaced_by_resolved() {
    let cfg = ConfineConfig::default();
    for p in ["/etc/resolv.conf", "/run/systemd/resolve", "/etc/hostname"] {
        assert!(cfg.system_read.contains(&PathBuf::from(p)), "{p}");
    }

    let dir = Dir::new("resolved");
    let run = dir.sub("resolve", "stub-resolv.conf");
    let etc = dir.sub("etc", "hosts");
    let link = etc.join("resolv.conf");
    std::os::unix::fs::symlink(run.join("stub-resolv.conf"), &link).expect("symlink");

    // Ограничения на отдельном потоке; файл заменяет основной (неограниченный) поток.
    let read_after_replace = |system_read: Vec<PathBuf>, content: &str| {
        let c = Confinement::new(
            ConfineConfig {
                system_read,
                ..bare(SeccompMode::Off, true)
            },
            edge(),
            RolePaths::default(),
        );
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        let l = link.clone();
        let thread = std::thread::spawn(move || {
            let report = c.apply_scoped(false).expect("apply");
            let before = std::fs::read(&l);
            ready_tx.send(()).expect("ready");
            go_rx.recv().expect("go");
            (report, before, std::fs::read(&l))
        });
        ready_rx.recv().expect("ready");
        let tmp = run.join(".stub-resolv.tmp");
        std::fs::write(&tmp, content).expect("write");
        std::fs::rename(&tmp, run.join("stub-resolv.conf")).expect("rename");
        go_tx.send(()).expect("go");
        thread.join().expect("поток теста")
    };

    let (report, before, after) = read_after_replace(vec![link.clone()], "search a.test");
    if report.landlock == LandlockOutcome::Unsupported {
        return;
    }
    assert_eq!(before.expect("до замены"), b"resolve");
    assert!(denied(after), "новый inode вне правила на ссылку");

    let (_, before, after) = read_after_replace(vec![link.clone(), run.clone()], "search b.test");
    assert_eq!(before.expect("до замены"), b"search a.test");
    assert_eq!(after.expect("после замены"), b"search b.test");
}

/// SYS-16: при входе GSSAPI (`krb5_rcache` задан) правила Landlock содержат каталог кэша повторов Kerberos на запись
/// для любых ролей, в т.ч. edge; без него — нет, и `/var/tmp` не разрешается ни в каком случае.
#[test]
fn landlock_krb5_rcache_only_with_gssapi() {
    let rcache = PathBuf::from(KRB5_RCACHE_DIR);
    let has = |rules: &[(Grant, Vec<PathBuf>)], path: &Path| {
        rules
            .iter()
            .any(|(g, paths)| *g == Grant::Write && paths.iter().any(|p| p == path))
    };
    let any = |rules: &[(Grant, Vec<PathBuf>)], path: &Path| {
        rules
            .iter()
            .any(|(_, paths)| paths.iter().any(|p| p == path))
    };
    let access = Profile::for_roles(&set(&[RoleEntry::Preset(RolePreset::Access)]));
    for profile in [edge(), access] {
        let with = Confinement::new(
            ConfineConfig::default(),
            profile,
            RolePaths {
                krb5_rcache: Some(rcache.clone()),
                ..RolePaths::default()
            },
        )
        .rules();
        assert!(has(&with, &rcache), "{profile:?}: каталог кэша с GSSAPI");
        assert!(!any(&with, Path::new("/var/tmp")), "{profile:?}: /var/tmp");
        let without =
            Confinement::new(ConfineConfig::default(), profile, RolePaths::default()).rules();
        assert!(
            !any(&without, &rcache),
            "{profile:?}: каталог кэша без GSSAPI"
        );
    }

    // Под Landlock узел edge пишет в каталог кэша, только если он задан.
    let dir = Dir::new("krb5rcache");
    let cache = dir.sub("rcache", "krb5_0.rcache2");
    let write = |krb5_rcache: Option<PathBuf>| {
        let paths = RolePaths {
            krb5_rcache,
            ..RolePaths::default()
        };
        let c = Confinement::new(bare(SeccompMode::Off, true), edge(), paths);
        let file = cache.join("krb5_1.rcache2");
        confined(c, move |report| {
            (report.landlock, std::fs::write(file, b"rc"))
        })
    };
    let (outcome, allowed) = write(Some(cache.clone()));
    if outcome == LandlockOutcome::Unsupported {
        return;
    }
    allowed.expect("запись в каталог кэша повторов");
    let (_, other) = write(None);
    assert!(denied(other), "без GSSAPI каталог кэша закрыт");
}
