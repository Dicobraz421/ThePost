//! Подпроцесс в тестах — этот же тестовый бинарник, запущенный на тест [`worker_entry`].

use std::hint::black_box;

use super::*;

const TEST_WORKER_ENV: &str = "TP_SANDBOX_TEST_WORKER";

/// Точка входа подпроцесса; при обычном прогоне тестов ничего не делает.
#[test]
fn worker_entry() {
    if std::env::var_os(TEST_WORKER_ENV).is_none() {
        return;
    }
    let code = i32::from(run_worker(handler).is_err());
    std::process::exit(code);
}

fn handler(input: &[u8]) -> Result<Vec<u8>, String> {
    match input {
        b"alloc" => {
            let v = black_box(vec![1u8; 8 << 30]);
            Ok(vec![v[v.len() - 1]])
        }
        b"spin" => loop {
            black_box(());
        },
        b"socket" => std::net::TcpListener::bind("127.0.0.1:0")
            .map(|_| b"connected".to_vec())
            .map_err(|e| e.to_string()),
        b"write" => std::fs::write("/tmp/tp-sandbox-test", b"x")
            .map(|()| b"written".to_vec())
            .map_err(|e| e.to_string()),
        b"panic" => panic!("boom"),
        other => Ok(other.to_vec()),
    }
}

fn config() -> SandboxConfig {
    SandboxConfig {
        workers: 1,
        memory_limit_bytes: 512 << 20,
        cpu_limit_secs: 30,
        timeout_ms: 10_000,
        ..SandboxConfig::default()
    }
}

fn pool(cfg: SandboxConfig) -> SandboxPool {
    let cmd = WorkerCommand::current_exe([
        "--exact",
        "sandbox::tests::worker_entry",
        "--nocapture",
        "--test-threads=1",
        "-q",
    ])
    .expect("current_exe")
    .env(TEST_WORKER_ENV, "1");
    SandboxPool::new(cfg, cmd)
}

async fn assert_recovers(pool: &SandboxPool) {
    let out = pool.run(b"after").await.expect("pool recovers");
    assert_eq!(out, b"after");
}

#[tokio::test]
async fn echo_reuses_worker() {
    let pool = pool(config());
    for i in 0..3u8 {
        let out = pool.run(&[b'x', i]).await.expect("echo");
        assert_eq!(out, [b'x', i]);
    }
}

#[tokio::test]
async fn memory_limit_fails_job_and_pool_recovers() {
    let pool = pool(config());
    let err = pool.run(b"alloc").await.expect_err("must exceed RLIMIT_AS");
    assert!(matches!(err, JobError::Crashed(_)), "{err:?}");
    assert_recovers(&pool).await;
}

#[tokio::test]
async fn cpu_limit_fails_job_and_pool_recovers() {
    let pool = pool(SandboxConfig {
        cpu_limit_secs: 1,
        ..config()
    });
    let err = pool.run(b"spin").await.expect_err("must exceed RLIMIT_CPU");
    assert!(matches!(err, JobError::CpuLimit), "{err:?}");
    assert_recovers(&pool).await;
}

#[tokio::test]
async fn timeout_kills_worker_and_pool_recovers() {
    let pool = pool(SandboxConfig {
        timeout_ms: 300,
        ..config()
    });
    let err = pool.run(b"spin").await.expect_err("must time out");
    assert!(matches!(err, JobError::Timeout), "{err:?}");
    assert_recovers(&pool).await;
}

#[tokio::test]
async fn network_is_denied() {
    let pool = pool(config());
    let err = pool.run(b"socket").await.expect_err("socket must fail");
    assert!(
        matches!(&err, JobError::Failed(m) if m.contains("os error 1")),
        "{err:?}"
    );
    assert_recovers(&pool).await;
}

#[tokio::test]
async fn disk_write_is_denied() {
    let pool = pool(config());
    let err = pool.run(b"write").await.expect_err("write must fail");
    assert!(
        matches!(&err, JobError::Failed(m) if m.contains("os error 1")),
        "{err:?}"
    );
}

#[tokio::test]
async fn panic_is_job_error() {
    let pool = pool(config());
    let err = pool.run(b"panic").await.expect_err("panic");
    assert!(matches!(err, JobError::Failed(_)), "{err:?}");
    assert_recovers(&pool).await;
}

#[tokio::test]
async fn input_limit() {
    let pool = pool(SandboxConfig {
        max_input_bytes: 4,
        ..config()
    });
    let err = pool.run(b"12345").await.expect_err("too large");
    assert!(matches!(err, JobError::InputTooLarge(5)), "{err:?}");
}

#[test]
fn worker_refuses_to_run_without_pool() {
    // Окружение тестового процесса не содержит лимитов: без них подпроцесс не стартует.
    let err = run_worker(|_| Ok(Vec::new())).expect_err("no limits in env");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
}
