//! Минимальные привилегии при запуске (SRS 2.7, 6.8; решение 2026-11-01-borrowings-security; PLT-16).
//!
//! Порядок запуска в приложении:
//! 1. [`check_user`] — отказ запуска от root без `privileges.allow_root: true`;
//! 2. [`role_paths`] — каталоги ролей из секций платформы (конфигурация, TLS, секреты, ACME); каталоги BLOB, кэша
//!    и подпроцессов добавляет приложение по секциям своих крейтов;
//! 3. [`confine`] — профили seccomp и Landlock набора ролей (SYS-07). На ядрах без Landlock ABI v8 правила действуют
//!    только на вызывающий поток и его будущих потомков, поэтому вызывать до запуска рантайма tokio (набор ролей и
//!    каталоги известны из конфигурации); на ABI v8 — можно и после инициализации ролей до приёма соединений;
//! 4. [`inherited_sockets`] и [`crate::Listener::open`] — сокеты, переданные systemd (`LISTEN_FDS`), вместо bind портов
//!    ниже 1024: процессу не нужен `CAP_NET_BIND_SERVICE`.
//!
//! [`doctor`] — проверки `thepost doctor`: учётная запись, capabilities, `NoNewPrivs`, поддержка seccomp и Landlock ядром
//! и режимы секции `confine`.

use std::io;
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Deserialize;
use tp_contracts::Error;
use tp_sys::{ConfineConfig, ConfineReport, Confinement, RolePaths, SeccompMode};

use crate::acme::AcmeConfig;
use crate::config::Config;
use crate::secrets::{DoctorReport, DoctorStatus, SecretsConfig};
use crate::tls::TlsConfig;

/// Имя секции.
pub const PRIVILEGES: &str = "privileges";
/// Имя секции ограничений ролей (SYS-07, [`ConfineConfig`]).
pub const CONFINE: &str = "confine";

/// Секция `privileges`: запуск от непривилегированной учётной записи. Требует перезапуска.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PrivilegesConfig {
    /// Разрешить запуск от root (uid 0). По умолчанию запуск от root отклоняется: порты ниже 1024 — через
    /// активацию сокетов systemd или `CAP_NET_BIND_SERVICE`.
    pub allow_root: bool,
    /// Принимать слушающие сокеты от systemd (`LISTEN_FDS`, `LISTEN_PID`, `LISTEN_FDNAMES`).
    pub socket_activation: bool,
}

impl Default for PrivilegesConfig {
    fn default() -> Self {
        Self {
            allow_root: false,
            socket_activation: true,
        }
    }
}

/// `CAP_NET_BIND_SERVICE`: единственная допустимая capability (привязка портов ниже 1024).
pub const CAP_NET_BIND_SERVICE: u32 = 10;

/// Имена capabilities Linux по номеру бита.
const CAP_NAMES: [&str; 41] = [
    "chown",
    "dac_override",
    "dac_read_search",
    "fowner",
    "fsetid",
    "kill",
    "setgid",
    "setuid",
    "setpcap",
    "linux_immutable",
    "net_bind_service",
    "net_broadcast",
    "net_admin",
    "net_raw",
    "ipc_lock",
    "ipc_owner",
    "sys_module",
    "sys_rawio",
    "sys_chroot",
    "sys_ptrace",
    "sys_pacct",
    "sys_admin",
    "sys_boot",
    "sys_nice",
    "sys_resource",
    "sys_time",
    "sys_tty_config",
    "mknod",
    "lease",
    "audit_write",
    "audit_control",
    "setfcap",
    "mac_override",
    "mac_admin",
    "syslog",
    "wake_alarm",
    "block_suspend",
    "audit_read",
    "perfmon",
    "bpf",
    "checkpoint_restore",
];

/// Имена capabilities маски `mask` (`cap_<имя>`; неизвестные — `cap_<номер>`).
pub fn cap_names(mask: u64) -> Vec<String> {
    (0..64u32)
        .filter(|bit| mask & (1u64 << bit) != 0)
        .map(|bit| match CAP_NAMES.get(bit as usize) {
            Some(name) => format!("cap_{name}"),
            None => format!("cap_{bit}"),
        })
        .collect()
}

/// Учётная запись и привилегии процесса из `/proc/self/status`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ProcessStatus {
    /// Реальный uid.
    pub uid: u32,
    /// Действующий uid.
    pub euid: u32,
    /// Действующий gid.
    pub egid: u32,
    /// Действующие capabilities (`CapEff`).
    pub cap_eff: u64,
    /// Разрешённые capabilities (`CapPrm`).
    pub cap_prm: u64,
    /// Ограничивающий набор (`CapBnd`).
    pub cap_bnd: u64,
    /// Наследуемые через `execve` (`CapAmb`).
    pub cap_amb: u64,
    /// `NoNewPrivs`: новые привилегии через `execve` недоступны.
    pub no_new_privs: bool,
    /// Режим seccomp процесса (0 — нет, 1 — strict, 2 — фильтр); `None` — ядро без seccomp.
    pub seccomp: Option<u8>,
}

impl ProcessStatus {
    /// Разбор текста `/proc/<pid>/status`; `None` — нет строк `Uid:` или `CapEff:`.
    pub fn parse(status: &str) -> Option<Self> {
        let field = |name: &str| {
            status
                .lines()
                .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
                .map(str::trim)
        };
        let ids = |name: &str| -> Option<Vec<u32>> {
            field(name)?
                .split_whitespace()
                .map(|v| v.parse().ok())
                .collect()
        };
        let caps = |name: &str| field(name).and_then(|v| u64::from_str_radix(v, 16).ok());
        let uids = ids("Uid")?;
        let gids = ids("Gid").unwrap_or_default();
        Some(Self {
            uid: *uids.first()?,
            euid: *uids.get(1)?,
            egid: gids.get(1).copied().unwrap_or_default(),
            cap_eff: caps("CapEff")?,
            cap_prm: caps("CapPrm").unwrap_or_default(),
            cap_bnd: caps("CapBnd").unwrap_or_default(),
            cap_amb: caps("CapAmb").unwrap_or_default(),
            no_new_privs: field("NoNewPrivs") == Some("1"),
            seccomp: field("Seccomp").and_then(|v| v.parse().ok()),
        })
    }

    /// Текущий процесс; `None` — нет procfs (не Linux). Блокирующее чтение: при старте или в `spawn_blocking`.
    pub fn current() -> Option<Self> {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| Self::parse(&s))
    }

    /// Процесс работает от root.
    pub fn is_root(&self) -> bool {
        self.euid == 0 || self.uid == 0
    }

    /// Capabilities сверх `CAP_NET_BIND_SERVICE` в маске `mask`.
    pub fn extra_caps(mask: u64) -> u64 {
        mask & !(1u64 << CAP_NET_BIND_SERVICE)
    }
}

/// Проверка учётной записи перед запуском: от root — ошибка, если не задано `privileges.allow_root`.
pub fn check_user_status(
    cfg: &PrivilegesConfig,
    status: &ProcessStatus,
) -> tp_contracts::Result<()> {
    if !status.is_root() {
        let extra = ProcessStatus::extra_caps(status.cap_eff);
        if extra != 0 {
            tracing::warn!(
                capabilities = ?cap_names(extra),
                "процессу выданы лишние capabilities: допустима только cap_net_bind_service"
            );
        }
        return Ok(());
    }
    if cfg.allow_root {
        tracing::warn!("запуск от root разрешён privileges.allow_root: ограничения ролей слабее");
        return Ok(());
    }
    Err(Error::Invalid(format!(
        "running as root (uid {}, euid {}) is not allowed: run thepost under an unprivileged account \
         (ports below 1024 — systemd socket activation or CAP_NET_BIND_SERVICE) or set {PRIVILEGES}.allow_root: true",
        status.uid, status.euid
    )))
}

/// Проверка учётной записи текущего процесса по секции `privileges`; без procfs (не Linux) — пропуск.
/// Блокирующее чтение `/proc/self/status`: вызывать при старте до рантайма или в `spawn_blocking`.
pub fn check_user(cfg: &Config) -> tp_contracts::Result<()> {
    let section = cfg.section::<PrivilegesConfig>(PRIVILEGES)?;
    match ProcessStatus::current() {
        Some(status) => check_user_status(&section, &status),
        None => Ok(()),
    }
}

/// Каталоги ролей из секций платформы: файл и каталог конфигурации, файлы TLS (чтение; при встроенном ACME —
/// запись в их каталоги и в `acme.state_dir`), файлы входа в хранилище секретов и CA хранилища (чтение).
/// Каталоги BLOB, кэша и делегированной cgroup подпроцессов добавляет приложение.
pub fn role_paths(cfg: &Config, config_file: &Path) -> RolePaths {
    let mut paths = RolePaths::default();
    paths.config.push(config_file.to_owned());
    if let Some(dir) = config_file.parent().filter(|d| !d.as_os_str().is_empty()) {
        // Каталог целиком: замена ConfigMap через символическую ссылку (hot reload).
        paths.config.push(dir.to_owned());
    }
    let tls = cfg
        .section::<TlsConfig>("tls")
        .map(|t| (*t).clone())
        .unwrap_or_default();
    let acme = cfg
        .section::<AcmeConfig>("acme")
        .map(|a| (*a).clone())
        .unwrap_or_default();
    if acme.enabled {
        paths.state.push(acme.state_dir.clone());
        for file in [&tls.cert_file, &tls.key_file] {
            if let Some(dir) = file.parent() {
                push_unique(&mut paths.state, dir.to_owned());
            }
        }
    } else {
        paths.config.push(tls.cert_file.clone());
        paths.config.push(tls.key_file.clone());
    }
    if let Ok(secrets) = cfg.section::<SecretsConfig>("secrets")
        && secrets.url.is_some()
    {
        paths.secrets.push(secrets.tls.ca_file.clone());
        let auth = &secrets.auth;
        if let Some(k) = &auth.kubernetes {
            paths.secrets.push(k.jwt_file.clone());
        }
        if let Some(c) = &auth.cert {
            paths.secrets.push(c.cert_file.clone());
            paths.secrets.push(c.key_file.clone());
        }
        if let Some(a) = &auth.approle {
            paths.secrets.extend(a.role_id_file.clone());
            paths.secrets.push(a.secret_id_file.clone());
        }
        if let Some(t) = &auth.token {
            paths.secrets.push(t.token_file.clone());
        }
    }
    paths
}

fn push_unique(list: &mut Vec<PathBuf>, path: PathBuf) {
    if !list.contains(&path) {
        list.push(path);
    }
}

/// Применяет профили seccomp и Landlock набора ролей узла (SYS-07) по секции `confine`. Необратимо. Ошибка — запуск
/// прервать. Ослабления (ядро без Landlock, режим `log`) — в [`ConfineReport::warnings`] и в журнале.
pub fn confine(cfg: &Config, paths: RolePaths) -> tp_contracts::Result<ConfineReport> {
    let section = cfg.section::<ConfineConfig>(CONFINE)?;
    let profile = tp_sys::Profile::for_roles(cfg.roles());
    Confinement::new((*section).clone(), profile, paths)
        .apply()
        .map_err(|e| Error::Internal(format!("{CONFINE}: {e}")))
}

// ---------- активация сокетов systemd ----------

/// Первый дескриптор, переданный systemd (`SD_LISTEN_FDS_START`).
pub const LISTEN_FDS_START: i32 = 3;

/// Дескриптор слушающего сокета, переданный systemd.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InheritedFd {
    /// Номер дескриптора.
    pub fd: i32,
    /// Имя из `LISTEN_FDNAMES` (`FileDescriptorName=` юнита сокета).
    pub name: Option<String>,
}

/// Дескрипторы из переменных `LISTEN_PID`, `LISTEN_FDS`, `LISTEN_FDNAMES` для процесса `pid` (sd_listen_fds(3)):
/// переменные не заданы или `LISTEN_PID` другого процесса (наследник) — пусто; неверные значения — ошибка.
pub fn listen_fds_from(
    pid: u32,
    var: impl Fn(&str) -> Option<String>,
) -> tp_contracts::Result<Vec<InheritedFd>> {
    let invalid = |m: String| Error::Invalid(format!("systemd socket activation: {m}"));
    let (Some(listen_pid), Some(count)) = (var("LISTEN_PID"), var("LISTEN_FDS")) else {
        return Ok(Vec::new());
    };
    let listen_pid: u32 = listen_pid
        .trim()
        .parse()
        .map_err(|_| invalid(format!("LISTEN_PID={listen_pid}")))?;
    if listen_pid != pid {
        return Ok(Vec::new());
    }
    let count: i32 = count
        .trim()
        .parse()
        .ok()
        .filter(|n| (0..=4096).contains(n))
        .ok_or_else(|| invalid(format!("LISTEN_FDS={count}")))?;
    let names: Vec<String> = var("LISTEN_FDNAMES")
        .map(|n| n.split(':').map(str::to_owned).collect())
        .unwrap_or_default();
    Ok((0..count)
        .map(|i| InheritedFd {
            fd: LISTEN_FDS_START + i,
            name: names
                .get(i as usize)
                .filter(|n| !n.is_empty() && n.as_str() != "unknown")
                .cloned(),
        })
        .collect())
}

/// Дескрипторы, переданные systemd текущему процессу (переменные окружения процесса).
pub fn listen_fds() -> tp_contracts::Result<Vec<InheritedFd>> {
    listen_fds_from(std::process::id(), |name| std::env::var(name).ok())
}

/// Слушающие сокеты, переданные systemd, по локальным адресам.
#[derive(Debug, Default)]
pub struct InheritedSockets {
    sockets: Vec<(SocketAddr, Option<String>, TcpListener)>,
}

impl InheritedSockets {
    /// Принимает дескрипторы `fds`: `adopt` превращает номер дескриптора в слушающий TCP-сокет (владение
    /// дескриптором переходит сокету). Ошибка — запуск прервать: сокет юнита не TCP или уже закрыт.
    pub fn adopt(
        fds: Vec<InheritedFd>,
        mut adopt: impl FnMut(i32) -> io::Result<TcpListener>,
    ) -> tp_contracts::Result<Self> {
        let mut sockets = Vec::with_capacity(fds.len());
        for InheritedFd { fd, name } in fds {
            let invalid =
                |e: io::Error| Error::Invalid(format!("systemd socket activation: fd {fd}: {e}"));
            let tcp = adopt(fd).map_err(invalid)?;
            let addr = tcp.local_addr().map_err(invalid)?;
            tracing::info!(fd, %addr, name = name.as_deref().unwrap_or(""), "сокет от systemd");
            sockets.push((addr, name, tcp));
        }
        Ok(Self { sockets })
    }

    /// Сокет для адреса `addr`: точное совпадение; для `0.0.0.0` или `[::]` — любой сокет на всех адресах с тем же
    /// портом (systemd часто слушает `[::]` для обоих семейств).
    pub fn take(&mut self, addr: SocketAddr) -> Option<TcpListener> {
        let pos = self
            .sockets
            .iter()
            .position(|(a, _, _)| *a == addr)
            .or_else(|| {
                addr.ip().is_unspecified().then_some(())?;
                self.sockets
                    .iter()
                    .position(|(a, _, _)| a.ip().is_unspecified() && a.port() == addr.port())
            })?;
        Some(self.sockets.remove(pos).2)
    }

    /// Адреса сокетов, не взятых слушателями (лишние в юните сокета).
    pub fn unused(&self) -> Vec<SocketAddr> {
        self.sockets.iter().map(|(a, _, _)| *a).collect()
    }

    /// Сокетов нет.
    pub fn is_empty(&self) -> bool {
        self.sockets.is_empty()
    }
}

/// Сокеты systemd текущего процесса по секции `privileges` (`socket_activation: false` — пусто). `adopt` — как в
/// [`InheritedSockets::adopt`]; вызывать один раз при старте, до открытия слушателей.
pub fn inherited_sockets(
    cfg: &Config,
    adopt: impl FnMut(i32) -> io::Result<TcpListener>,
) -> tp_contracts::Result<InheritedSockets> {
    let section = cfg.section::<PrivilegesConfig>(PRIVILEGES)?;
    if !section.socket_activation {
        return Ok(InheritedSockets::default());
    }
    InheritedSockets::adopt(listen_fds()?, adopt)
}

// ---------- doctor ----------

/// Проверки `thepost doctor` по процессу проверки: `privileges_user`, `privileges_capabilities`,
/// `privileges_no_new_privs`, `privileges_seccomp`, `privileges_landlock`. Не завершается ошибкой.
pub async fn doctor(cfg: &Config) -> DoctorReport {
    let status = tokio::fs::read_to_string("/proc/self/status").await.ok();
    let lsm = tokio::fs::read_to_string("/sys/kernel/security/lsm")
        .await
        .ok();
    doctor_with(cfg, status.as_deref(), lsm.as_deref())
}

/// Проверки по тексту `/proc/self/status` и `/sys/kernel/security/lsm` (`None` — файл недоступен).
pub fn doctor_with(cfg: &Config, status: Option<&str>, lsm: Option<&str>) -> DoctorReport {
    let mut r = DoctorReport::default();
    let privileges = cfg
        .section::<PrivilegesConfig>(PRIVILEGES)
        .map(|s| (*s).clone())
        .unwrap_or_default();
    let confine = cfg
        .section::<ConfineConfig>(CONFINE)
        .map(|s| (*s).clone())
        .unwrap_or_default();
    let Some(st) = status.and_then(ProcessStatus::parse) else {
        for id in [
            "privileges_user",
            "privileges_capabilities",
            "privileges_no_new_privs",
            "privileges_seccomp",
            "privileges_landlock",
        ] {
            r.push(
                id,
                DoctorStatus::Skipped,
                "",
                "нет /proc/self/status: проверка только на Linux",
            );
        }
        return r;
    };

    let user = format!("uid={} euid={} egid={}", st.uid, st.euid, st.egid);
    match (st.is_root(), privileges.allow_root) {
        (false, _) => r.push(
            "privileges_user",
            DoctorStatus::Ok,
            user,
            "непривилегированная учётная запись",
        ),
        (true, true) => r.push(
            "privileges_user",
            DoctorStatus::Warn,
            user,
            "запуск от root разрешён privileges.allow_root: используйте отдельную учётную запись",
        ),
        (true, false) => r.push(
            "privileges_user",
            DoctorStatus::Fail,
            user,
            "запуск от root: сервер откажется стартовать; используйте отдельную учётную запись \
             (порты ниже 1024 — активация сокетов systemd или CAP_NET_BIND_SERVICE)",
        ),
    }

    let extra_eff = ProcessStatus::extra_caps(st.cap_eff | st.cap_prm | st.cap_amb);
    let extra_bnd = ProcessStatus::extra_caps(st.cap_bnd);
    let caps = format!(
        "eff={} bnd={}",
        cap_list(st.cap_eff),
        if extra_bnd == 0 {
            cap_list(st.cap_bnd)
        } else {
            format!("{} capabilities", st.cap_bnd.count_ones())
        }
    );
    if extra_eff != 0 {
        r.push(
            "privileges_capabilities",
            DoctorStatus::Warn,
            caps,
            format!(
                "лишние capabilities: {}; допустима только cap_net_bind_service",
                cap_names(extra_eff).join(", ")
            ),
        );
    } else if extra_bnd != 0 {
        r.push(
            "privileges_capabilities",
            DoctorStatus::Warn,
            caps,
            "ограничивающий набор не пуст: CapabilityBoundingSet=CAP_NET_BIND_SERVICE в службе systemd, \
             drop: [ALL] в Kubernetes",
        );
    } else {
        r.push(
            "privileges_capabilities",
            DoctorStatus::Ok,
            caps,
            "capabilities не выданы (кроме привязки портов)",
        );
    }

    if st.no_new_privs {
        r.push(
            "privileges_no_new_privs",
            DoctorStatus::Ok,
            "1",
            "новые привилегии недоступны",
        );
    } else {
        r.push(
            "privileges_no_new_privs",
            DoctorStatus::Warn,
            "0",
            "NoNewPrivileges=yes в службе systemd, allowPrivilegeEscalation: false в Kubernetes",
        );
    }

    let mode = format!("{:?}", confine.seccomp).to_lowercase();
    match (st.seccomp.is_some(), confine.seccomp) {
        (_, SeccompMode::Off) => r.push(
            "privileges_seccomp",
            DoctorStatus::Warn,
            mode,
            "seccomp выключен в confine.seccomp",
        ),
        (false, _) => r.push(
            "privileges_seccomp",
            DoctorStatus::Fail,
            mode,
            "ядро без seccomp: запуск с confine.seccomp не выполнится",
        ),
        (true, SeccompMode::Log) => r.push(
            "privileges_seccomp",
            DoctorStatus::Warn,
            mode,
            "режим журнала: вызовы вне профиля не блокируются (для первого запуска и отладки профиля)",
        ),
        (true, SeccompMode::Enforce) => r.push(
            "privileges_seccomp",
            DoctorStatus::Ok,
            mode,
            "профиль seccomp ролей применяется при запуске",
        ),
    }

    let landlock = lsm.map(|l| l.trim().split(',').any(|m| m == "landlock"));
    match (confine.landlock, landlock) {
        (false, _) => r.push(
            "privileges_landlock",
            DoctorStatus::Warn,
            "off",
            "Landlock выключен в confine.landlock",
        ),
        (true, Some(true)) => r.push(
            "privileges_landlock",
            DoctorStatus::Ok,
            "on",
            "ограничения файловой системы ролей применяются при запуске",
        ),
        (true, Some(false)) => r.push(
            "privileges_landlock",
            DoctorStatus::Warn,
            "unsupported",
            "ядро не поддерживает Landlock (нет в /sys/kernel/security/lsm): доступ к файловой системе не ограничен",
        ),
        (true, None) => r.push(
            "privileges_landlock",
            DoctorStatus::Warn,
            "unknown",
            "не удалось прочитать /sys/kernel/security/lsm: поддержка Landlock не проверена",
        ),
    }
    r
}

fn cap_list(mask: u64) -> String {
    let names = cap_names(mask);
    if names.is_empty() {
        "-".to_owned()
    } else {
        names.join(",")
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::net::Ipv4Addr;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;
    use crate::config::SectionRegistry;
    use crate::listen::{EndpointConfig, Listener};

    const BASE: &str = "database: { url: pg }\nblob: { url: file:///b }\ndomain: d\n";

    fn config(extra: &str) -> Config {
        SectionRegistry::new()
            .parse(&format!("{BASE}{extra}"), Vec::new())
            .expect("parse")
    }

    fn status(uid: u32, eff: &str, bnd: &str, nnp: u8) -> String {
        format!(
            "Name:\tthepost\nUid:\t{uid}\t{uid}\t{uid}\t{uid}\nGid:\t998\t998\t998\t998\nCapInh:\t0000000000000000\n\
             CapPrm:\t{eff}\nCapEff:\t{eff}\nCapBnd:\t{bnd}\nCapAmb:\t0000000000000000\nNoNewPrivs:\t{nnp}\n\
             Seccomp:\t0\nSeccomp_filters:\t0\n"
        )
    }

    const NONE: &str = "0000000000000000";
    const BIND: &str = "0000000000000400";
    const ALL: &str = "000001ffffffffff";

    #[test]
    fn status_parses() {
        let st = ProcessStatus::parse(&status(998, BIND, ALL, 1)).expect("status");
        assert_eq!((st.uid, st.euid, st.egid), (998, 998, 998));
        assert_eq!(st.cap_eff, 1 << CAP_NET_BIND_SERVICE);
        assert_eq!(st.cap_bnd.count_ones(), 41);
        assert!(st.no_new_privs && !st.is_root());
        assert_eq!(st.seccomp, Some(0));
        assert_eq!(cap_names(st.cap_eff), ["cap_net_bind_service"]);
        assert_eq!(
            cap_names((1 << 21) | (1 << 50)),
            ["cap_sys_admin", "cap_50"]
        );
        assert_eq!(ProcessStatus::parse("Name: x\n"), None);
    }

    #[test]
    fn root_is_refused_without_explicit_setting() {
        let root = ProcessStatus::parse(&status(0, ALL, ALL, 0)).expect("status");
        let e = check_user_status(&PrivilegesConfig::default(), &root).expect_err("root");
        assert!(e.to_string().contains("allow_root"), "{e}");
        let allowed = PrivilegesConfig {
            allow_root: true,
            ..PrivilegesConfig::default()
        };
        assert!(check_user_status(&allowed, &root).is_ok());
        let user = ProcessStatus::parse(&status(998, NONE, NONE, 1)).expect("status");
        assert!(check_user_status(&PrivilegesConfig::default(), &user).is_ok());

        let cfg = config("privileges: { allow_root: true }\n");
        let section = cfg
            .section::<PrivilegesConfig>(PRIVILEGES)
            .expect("section");
        assert!(section.allow_root && section.socket_activation);
        assert!(
            SectionRegistry::new()
                .parse_strict(&format!("{BASE}privileges: {{ bogus: 1 }}\n"), Vec::new())
                .is_err()
        );
    }

    fn checks(r: &DoctorReport) -> BTreeMap<&'static str, DoctorStatus> {
        r.checks.iter().map(|c| (c.id, c.status)).collect()
    }

    #[test]
    fn doctor_reports_root() {
        let cfg = config("");
        let r = doctor_with(
            &cfg,
            Some(&status(0, ALL, ALL, 0)),
            Some("lockdown,capability,yama"),
        );
        let c = checks(&r);
        assert_eq!(c["privileges_user"], DoctorStatus::Fail);
        assert_eq!(c["privileges_capabilities"], DoctorStatus::Warn);
        assert_eq!(c["privileges_no_new_privs"], DoctorStatus::Warn);
        assert_eq!(c["privileges_seccomp"], DoctorStatus::Ok);
        assert_eq!(c["privileges_landlock"], DoctorStatus::Warn);
        assert_eq!(r.status(), DoctorStatus::Fail);
        let user = r.get("privileges_user").expect("user");
        assert!(user.value.contains("euid=0"), "{user:?}");
        assert!(user.message.contains("root"), "{user:?}");
        assert!(
            r.get("privileges_capabilities")
                .expect("caps")
                .message
                .contains("cap_sys_admin")
        );

        let allowed = config("privileges: { allow_root: true }\n");
        let r = doctor_with(&allowed, Some(&status(0, ALL, ALL, 0)), None);
        assert_eq!(checks(&r)["privileges_user"], DoctorStatus::Warn);
    }

    #[test]
    fn doctor_hardened_service_is_ok() {
        let cfg = config("");
        let r = doctor_with(
            &cfg,
            Some(&status(998, NONE, BIND, 1)),
            Some("lockdown,capability,landlock,yama,bpf\n"),
        );
        assert_eq!(r.status(), DoctorStatus::Ok, "{r:?}");
        assert_eq!(r.checks.len(), 5);

        let relaxed = config("confine: { seccomp: log, landlock: false }\n");
        let r = doctor_with(
            &relaxed,
            Some(&status(998, NONE, BIND, 1)),
            Some("landlock"),
        );
        let c = checks(&r);
        assert_eq!(c["privileges_seccomp"], DoctorStatus::Warn);
        assert_eq!(c["privileges_landlock"], DoctorStatus::Warn);

        let no_seccomp = status(998, NONE, BIND, 1).replace("Seccomp:\t0\n", "");
        let r = doctor_with(&cfg, Some(&no_seccomp), Some("landlock"));
        assert_eq!(checks(&r)["privileges_seccomp"], DoctorStatus::Fail);

        let r = doctor_with(&cfg, None, None);
        assert!(r.checks.iter().all(|c| c.status == DoctorStatus::Skipped));
    }

    #[test]
    fn role_paths_from_platform_sections() {
        let cfg = config(
            "tls: { cert_file: /etc/tp/tls/crt.pem, key_file: /etc/tp/tls/key.pem }\n\
             secrets:\n  url: https://vault:8200\n  auth: { approle: { role_id: r, secret_id_file: /run/cred/sid } }\n",
        );
        let p = role_paths(&cfg, Path::new("/etc/thepost/config.yaml"));
        assert_eq!(
            p.config,
            [
                "/etc/thepost/config.yaml",
                "/etc/thepost",
                "/etc/tp/tls/crt.pem",
                "/etc/tp/tls/key.pem"
            ]
            .map(PathBuf::from)
        );
        assert!(p.secrets.contains(&PathBuf::from("/run/cred/sid")));
        assert!(p.state.is_empty() && p.blob.is_empty() && p.cache.is_empty());

        let acme = config(
            "tls: { cert_file: /var/lib/tp/tls/crt.pem, key_file: /var/lib/tp/tls/key.pem }\n\
             acme: { enabled: true, domains: [mail.example.org], state_dir: /var/lib/tp/acme }\n",
        );
        let p = role_paths(&acme, Path::new("config.yaml"));
        assert_eq!(p.config, [PathBuf::from("config.yaml")]);
        assert_eq!(
            p.state,
            ["/var/lib/tp/acme", "/var/lib/tp/tls"].map(PathBuf::from)
        );
        assert!(p.secrets.is_empty(), "хранилище секретов не настроено");
    }

    #[test]
    fn listen_fds_follow_systemd_variables() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        let fds = listen_fds_from(
            42,
            env(&[
                ("LISTEN_PID", "42"),
                ("LISTEN_FDS", "3"),
                ("LISTEN_FDNAMES", "smtp:unknown:imaps"),
            ]),
        )
        .expect("fds");
        assert_eq!(
            fds,
            [
                InheritedFd {
                    fd: 3,
                    name: Some("smtp".to_owned())
                },
                InheritedFd { fd: 4, name: None },
                InheritedFd {
                    fd: 5,
                    name: Some("imaps".to_owned())
                },
            ]
        );
        // Переменные для другого процесса (унаследованы подпроцессом) и их отсутствие — сокетов нет.
        let other = env(&[("LISTEN_PID", "41"), ("LISTEN_FDS", "1")]);
        assert!(listen_fds_from(42, other).expect("other").is_empty());
        assert!(listen_fds_from(42, env(&[])).expect("none").is_empty());
        for bad in [
            env(&[("LISTEN_PID", "x"), ("LISTEN_FDS", "1")]),
            env(&[("LISTEN_PID", "42"), ("LISTEN_FDS", "-1")]),
            env(&[("LISTEN_PID", "42"), ("LISTEN_FDS", "many")]),
        ] {
            assert!(listen_fds_from(42, bad).is_err());
        }
    }

    #[tokio::test]
    async fn systemd_sockets_replace_bind() {
        // «systemd» открыл два сокета; дескрипторы 3 и 4 — их копии.
        let smtp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind smtp");
        let imap = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind imap");
        let smtp_addr = smtp.local_addr().expect("addr");
        let imap_addr = imap.local_addr().expect("addr");
        let mut passed = BTreeMap::from([(3, smtp), (4, imap)]);
        let fds = listen_fds_from(7, |name| match name {
            "LISTEN_PID" => Some("7".to_owned()),
            "LISTEN_FDS" => Some("2".to_owned()),
            "LISTEN_FDNAMES" => Some("smtp:imap".to_owned()),
            _ => None,
        })
        .expect("fds");
        let mut inherited = InheritedSockets::adopt(fds, |fd| {
            passed
                .remove(&fd)
                .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
        })
        .expect("adopt");
        assert_eq!(inherited.unused(), [smtp_addr, imap_addr]);

        // Слушатель SMTP берёт сокет systemd, а не открывает свой.
        let listener =
            Listener::open(&EndpointConfig::new(smtp_addr), None, &mut inherited).expect("open");
        assert_eq!(listener.local_addr().expect("addr"), smtp_addr);
        assert_eq!(inherited.unused(), [imap_addr]);
        let server = tokio::spawn(async move {
            let conn = listener
                .accept()
                .await
                .expect("accept")
                .establish()
                .await
                .expect("establish");
            let mut stream = conn.stream;
            stream.write_all(b"220 ok\r\n").await.expect("write");
        });
        let mut client = tokio::net::TcpStream::connect(smtp_addr)
            .await
            .expect("connect");
        let mut buf = [0u8; 8];
        client.read_exact(&mut buf).await.expect("read");
        assert_eq!(&buf, b"220 ok\r\n");
        server.await.expect("join");

        // Адреса без сокета systemd — обычный bind; занятый systemd адрес не открывается второй раз.
        let own = Listener::open(
            &EndpointConfig::new((Ipv4Addr::LOCALHOST, 0).into()),
            None,
            &mut inherited,
        )
        .expect("bind");
        assert_ne!(own.local_addr().expect("addr").port(), 0);
        assert!(inherited.take(imap_addr).is_some());
        assert!(inherited.is_empty());

        // Сокет на всех адресах подходит слушателю `0.0.0.0:<порт>`.
        let any = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("bind any");
        let port = any.local_addr().expect("addr").port();
        let mut inherited =
            InheritedSockets::adopt(vec![InheritedFd { fd: 3, name: None }], |_| {
                Ok(any.try_clone().expect("clone"))
            })
            .expect("adopt");
        assert!(inherited.take((Ipv4Addr::LOCALHOST, port).into()).is_none());
        assert!(
            inherited
                .take((Ipv4Addr::UNSPECIFIED, port).into())
                .is_some()
        );

        // Дескриптор, который не удалось принять, — ошибка запуска.
        let e = InheritedSockets::adopt(vec![InheritedFd { fd: 9, name: None }], |_| {
            Err(io::Error::from(io::ErrorKind::InvalidInput))
        })
        .expect_err("bad fd");
        assert!(e.to_string().contains("fd 9"), "{e}");
    }
}
