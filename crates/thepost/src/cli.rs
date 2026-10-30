//! Командная строка `thepost` (APP-01..03).
//!
//! Путь к config.yaml — `--config`, иначе переменная `THEPOST_CONFIG`, иначе `/etc/thepost/config.yaml`.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use tp_contracts::Error;
use tp_contracts::role::CancellationToken;
use tp_platform::config::ENV_CONFIG_PATH;

use crate::{doctor, secrets, sections, serve};

/// Путь к config.yaml по умолчанию.
pub const DEFAULT_CONFIG: &str = "/etc/thepost/config.yaml";

/// Почтовый сервер The Post.
#[derive(Debug, Parser)]
#[command(
    name = "thepost",
    version,
    about = "The Post — корпоративный почтовый сервер"
)]
pub struct Cli {
    /// Путь к config.yaml (по умолчанию — THEPOST_CONFIG или /etc/thepost/config.yaml).
    #[arg(long, short = 'c', global = true)]
    pub config: Option<PathBuf>,
    /// Команда.
    #[command(subcommand)]
    pub command: Command,
}

/// Команды.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// Запуск узла с ролями из config.yaml.
    Serve,
    /// Схема и проверка config.yaml.
    Config {
        /// Действие.
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// Проверка окружения: СУБД, BLOB, время, секреты.
    Doctor {
        /// Вывод в JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Действия с конфигурацией.
#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// JSON Schema config.yaml (для редакторов и GUI развёртывания).
    Schema,
    /// Строгая проверка: неизвестные и неверные параметры — ошибка.
    Check,
}

impl Cli {
    /// Путь к config.yaml.
    pub fn config_path(&self) -> PathBuf {
        self.config
            .clone()
            .or_else(|| std::env::var_os(ENV_CONFIG_PATH).map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG))
    }
}

/// Результат `thepost config check`: код возврата и текст.
pub fn config_check(path: &Path) -> (i32, String) {
    let checked = sections::registry().and_then(|r| r.check(path));
    match checked {
        Ok(cfg) => {
            let mut out = format!("{}: конфигурация верна\n", path.display());
            let refs = secrets::references(&cfg);
            if !refs.is_empty() {
                let params: Vec<&str> = refs.iter().map(|(p, _)| *p).collect();
                out.push_str(&format!(
                    "ссылки на секреты (читаются из хранилища секретов при запуске): {}\n",
                    params.join(", ")
                ));
            }
            (0, out)
        }
        Err(e) => {
            let msg = match e {
                Error::Invalid(m) => m,
                other => other.to_string(),
            };
            (
                1,
                format!("{}: ошибки конфигурации:\n{msg}\n", path.display()),
            )
        }
    }
}

/// Выполняет команду; возвращает код возврата процесса.
pub async fn run(cli: Cli) -> i32 {
    let path = cli.config_path();
    match cli.command {
        Command::Config {
            action: ConfigAction::Schema,
        } => match sections::registry() {
            Ok(r) => {
                println!("{}", r.schema_json());
                0
            }
            Err(e) => fail(&e),
        },
        Command::Config {
            action: ConfigAction::Check,
        } => {
            let (code, out) = config_check(&path);
            if code == 0 {
                print!("{out}");
            } else {
                eprint!("{out}");
            }
            code
        }
        Command::Doctor { json } => {
            let report = doctor::run(&path, None).await;
            if json {
                match report.to_json() {
                    Ok(j) => println!("{j}"),
                    Err(e) => return fail(&e),
                }
            } else {
                print!("{}", report.to_table());
            }
            report.exit_code()
        }
        Command::Serve => serve_until_signal(path).await,
    }
}

/// `thepost serve`: работа до SIGTERM/SIGINT; 0 — штатная остановка.
async fn serve_until_signal(path: PathBuf) -> i32 {
    let shutdown = CancellationToken::new();
    let opts = serve::ServeOptions {
        config: path,
        secrets: None,
        telemetry: true,
        shutdown: shutdown.clone(),
    };
    let running = match serve::start(opts).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "узел не запущен");
            return fail(&e);
        }
    };
    let signal = shutdown.clone();
    tokio::spawn(async move {
        if let Err(e) = tp_platform::roles::shutdown_signal().await {
            tracing::error!(error = %e, "обработчик сигналов");
        }
        signal.cancel();
    });
    match running.wait().await {
        Ok(report) if report.failed.is_empty() => 0,
        Ok(report) => {
            for (role, e) in &report.failed {
                eprintln!("thepost: роль {role} завершилась с ошибкой: {e}");
            }
            1
        }
        Err(e) => fail(&e),
    }
}

fn fail(e: &Error) -> i32 {
    match e {
        Error::Invalid(m) => eprintln!("thepost: {m}"),
        other => eprintln!("thepost: {other}"),
    }
    1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands() {
        let cli = Cli::try_parse_from(["thepost", "--config", "/x.yaml", "doctor", "--json"])
            .expect("doctor");
        assert!(matches!(cli.command, Command::Doctor { json: true }));
        assert_eq!(cli.config_path(), PathBuf::from("/x.yaml"));
        let cli =
            Cli::try_parse_from(["thepost", "config", "check", "-c", "/y.yaml"]).expect("check");
        assert!(matches!(
            cli.command,
            Command::Config {
                action: ConfigAction::Check
            }
        ));
        assert_eq!(cli.config_path(), PathBuf::from("/y.yaml"));
        assert!(Cli::try_parse_from(["thepost", "bogus"]).is_err());
    }

    #[test]
    fn config_check_is_strict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.yaml");
        let base = "domain: example.org\nprofile: dev\ndatabase:\n  url: postgres://thepost@db/thepost?sslmode=disable\n  password: { secret: \"thepost/db#password\" }\n  tls: { allow_plaintext: true }\nblob:\n  url: file:///var/lib/thepost/blob\n";
        std::fs::write(&path, base).expect("write");
        let (code, out) = config_check(&path);
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("database.password"), "{out}");

        std::fs::write(&path, format!("{base}telemetry:\n  log_levl: info\n")).expect("write");
        let (code, out) = config_check(&path);
        assert_eq!(code, 1);
        assert!(out.contains("telemetry.log_levl"), "{out}");

        // Тот же файл при запуске — мягкий режим: только предупреждение.
        let cfg = sections::registry()
            .expect("registry")
            .load(&path)
            .expect("lenient");
        assert_eq!(cfg.warnings().len(), 1);
    }
}
