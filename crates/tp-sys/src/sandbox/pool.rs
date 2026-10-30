//! Сторона родителя: пул подпроцессов, таймауты, классификация сбоев, cgroup v2.

use std::ffi::OsString;
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{ExitStatus, Stdio};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use nix::libc;
use schemars::JsonSchema;
use tokio::io::BufReader;
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{OnceCell, Semaphore};
use tokio::time::timeout;

use super::protocol;
use super::worker::{ENV_CPU, ENV_MAX_INPUT, ENV_MEMORY};

/// Конфигурация пула изолированных подпроцессов (SRS 2.7).
#[derive(Debug, Clone, PartialEq, Eq, JsonSchema)]
pub struct SandboxConfig {
    /// Максимум одновременно работающих подпроцессов; создаются по первому заданию.
    pub workers: usize,
    /// Предел адресного пространства подпроцесса (RLIMIT_AS), байт; 0 — без предела.
    pub memory_limit_bytes: u64,
    /// Предел CPU на задание (RLIMIT_CPU), с; 0 — без предела.
    pub cpu_limit_secs: u64,
    /// Предел времени задания по часам, мс; по истечении подпроцесс уничтожается.
    pub timeout_ms: u64,
    /// Предел запуска подпроцесса до готовности, мс.
    pub start_timeout_ms: u64,
    /// Максимальный размер входных данных задания, байт.
    pub max_input_bytes: u64,
    /// Максимальный размер результата задания, байт.
    pub max_output_bytes: u64,
    /// Делегированный каталог cgroup v2 для подпроцессов (подготавливает развёртывание, например
    /// systemd `Delegate=yes`). Пул пишет в него `memory.max` = `memory_limit_bytes` × `workers`
    /// и `cpu.max` = `workers` CPU и помещает туда подпроцессы. `None` — cgroup не используется.
    pub cgroup_dir: Option<PathBuf>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            workers: 2,
            memory_limit_bytes: 1 << 30,
            cpu_limit_secs: 30,
            timeout_ms: 60_000,
            start_timeout_ms: 5_000,
            max_input_bytes: 64 << 20,
            max_output_bytes: 16 << 20,
            cgroup_dir: None,
        }
    }
}

/// Команда запуска подпроцесса: подкоманда того же бинарного файла.
#[derive(Debug, Clone)]
pub struct WorkerCommand {
    program: PathBuf,
    args: Vec<OsString>,
    envs: Vec<(OsString, OsString)>,
}

impl WorkerCommand {
    /// Текущий исполняемый файл с аргументами подкоманды, например `["sandbox-worker"]`.
    pub fn current_exe<I, S>(args: I) -> io::Result<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        Ok(Self {
            program: std::env::current_exe()?,
            args: args.into_iter().map(Into::into).collect(),
            envs: Vec::new(),
        })
    }

    /// Переменная окружения подпроцесса (остальное окружение не наследуется).
    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.envs.push((key.into(), value.into()));
        self
    }
}

/// Ошибка задания. Основной процесс не затрагивается; сбойный подпроцесс заменяется новым.
#[derive(Debug, thiserror::Error)]
pub enum JobError {
    #[error("входные данные задания больше предела: {0} байт")]
    InputTooLarge(usize),
    #[error("обработчик вернул ошибку: {0}")]
    Failed(String),
    #[error("превышено время задания")]
    Timeout,
    #[error("превышен предел CPU")]
    CpuLimit,
    #[error("подпроцесс завершился аварийно: {0}")]
    Crashed(String),
    #[error("нарушение протокола подпроцесса: {0}")]
    Protocol(String),
    #[error("не удалось запустить подпроцесс: {0}")]
    Spawn(#[source] io::Error),
}

#[derive(Debug)]
struct Worker {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Worker {
    async fn call(&mut self, input: &[u8], max_output: u64) -> io::Result<protocol::Response> {
        protocol::write_request(&mut self.stdin, input).await?;
        protocol::read_response(&mut self.stdout, max_output).await
    }

    /// Уничтожает подпроцесс и возвращает статус завершения. Если он уже завершился,
    /// возвращается его собственный статус.
    async fn kill(mut self) -> Option<ExitStatus> {
        let _ = self.child.start_kill();
        self.child.wait().await.ok()
    }
}

/// Пул изолированных подпроцессов.
#[derive(Debug)]
pub struct SandboxPool {
    cfg: SandboxConfig,
    cmd: WorkerCommand,
    idle: Mutex<Vec<Worker>>,
    permits: Semaphore,
    cgroup: OnceCell<()>,
}

impl SandboxPool {
    /// Пул без подпроцессов: они запускаются по первому заданию (SRS 2.7).
    pub fn new(cfg: SandboxConfig, cmd: WorkerCommand) -> Self {
        let permits = Semaphore::new(cfg.workers.max(1));
        Self {
            cfg,
            cmd,
            idle: Mutex::new(Vec::new()),
            permits,
            cgroup: OnceCell::new(),
        }
    }

    /// Выполняет задание в подпроцессе.
    pub async fn run(&self, input: &[u8]) -> Result<Vec<u8>, JobError> {
        if input.len() as u64 > self.cfg.max_input_bytes {
            return Err(JobError::InputTooLarge(input.len()));
        }
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|e| JobError::Spawn(io::Error::other(e)))?;
        let idle = self
            .idle
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        let mut worker = match idle {
            Some(w) => w,
            None => self.spawn().await?,
        };

        let deadline = Duration::from_millis(self.cfg.timeout_ms);
        match timeout(deadline, worker.call(input, self.cfg.max_output_bytes)).await {
            Ok(Ok(resp)) => {
                if !resp.retire {
                    self.idle
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(worker);
                }
                resp.result.map_err(JobError::Failed)
            }
            Ok(Err(e)) if e.kind() == io::ErrorKind::InvalidData => {
                worker.kill().await;
                Err(JobError::Protocol(e.to_string()))
            }
            Ok(Err(_)) => Err(classify(worker.kill().await)),
            Err(_) => {
                worker.kill().await;
                Err(JobError::Timeout)
            }
        }
    }

    async fn spawn(&self) -> Result<Worker, JobError> {
        let mut cmd = Command::new(&self.cmd.program);
        cmd.args(&self.cmd.args)
            .env_clear()
            .envs(self.cmd.envs.iter().map(|(k, v)| (k, v)))
            .env(ENV_MEMORY, self.cfg.memory_limit_bytes.to_string())
            .env(ENV_CPU, self.cfg.cpu_limit_secs.to_string())
            .env(ENV_MAX_INPUT, self.cfg.max_input_bytes.to_string())
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().map_err(JobError::Spawn)?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            return Err(JobError::Spawn(io::Error::other("no stdio pipes")));
        };
        let mut worker = Worker {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };

        if let Some(dir) = &self.cfg.cgroup_dir {
            let pid = worker.child.id();
            let res = match pid {
                Some(pid) => self.join_cgroup(dir, pid).await,
                None => Err(io::Error::other("worker exited before cgroup setup")),
            };
            if let Err(e) = res {
                worker.kill().await;
                return Err(JobError::Spawn(e));
            }
        }

        let start = Duration::from_millis(self.cfg.start_timeout_ms);
        match timeout(start, protocol::read_handshake(&mut worker.stdout)).await {
            Ok(Ok(())) => Ok(worker),
            Ok(Err(e)) => {
                worker.kill().await;
                Err(JobError::Spawn(e))
            }
            Err(_) => {
                worker.kill().await;
                Err(JobError::Spawn(io::ErrorKind::TimedOut.into()))
            }
        }
    }

    /// Пишет лимиты каталога cgroup (один раз) и помещает в него подпроцесс.
    async fn join_cgroup(&self, dir: &Path, pid: u32) -> io::Result<()> {
        self.cgroup
            .get_or_try_init(|| {
                let dir = dir.to_path_buf();
                let workers = self.cfg.workers.max(1) as u64;
                let memory = self.cfg.memory_limit_bytes.saturating_mul(workers);
                blocking(move || {
                    if memory > 0 {
                        std::fs::write(dir.join("memory.max"), memory.to_string())?;
                    }
                    std::fs::write(dir.join("cpu.max"), format!("{} 100000", workers * 100_000))
                })
            })
            .await?;
        let procs = dir.join("cgroup.procs");
        blocking(move || std::fs::write(procs, pid.to_string())).await
    }
}

async fn blocking<F>(f: F) -> io::Result<()>
where
    F: FnOnce() -> io::Result<()> + Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(io::Error::other)?
}

fn classify(status: Option<ExitStatus>) -> JobError {
    match status {
        Some(s) if s.signal() == Some(libc::SIGXCPU) => JobError::CpuLimit,
        Some(s) => JobError::Crashed(s.to_string()),
        None => JobError::Crashed("статус неизвестен".to_owned()),
    }
}
