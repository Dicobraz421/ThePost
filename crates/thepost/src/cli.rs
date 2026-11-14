//! Командная строка `thepost` (APP-01..03; `thepost admin` — APP-19, APP-11; `thepost admin openapi` — APP-37).
//!
//! Путь к config.yaml — `--config`, иначе переменная `THEPOST_CONFIG`, иначе `/etc/thepost/config.yaml`.

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use tp_contracts::Error;
use tp_contracts::role::CancellationToken;
use tp_platform::config::ENV_CONFIG_PATH;

use crate::{admin_cli, doctor, local_admin, openapi, secrets, sections, serve, verify};

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
    /// Подпроцесс извлечения текста вложений для индекса поиска (запускает роль indexer).
    #[command(name = "search-extract", hide = true)]
    SearchExtract,
    /// Сверка хранилища BLOB и учёта объектов.
    Verify {
        /// Удалить «сирот» и пометить повреждённые записи.
        #[arg(long)]
        fix: bool,
        /// Отчёт в JSON.
        #[arg(long)]
        json: bool,
    },
    /// Администрирование: первый администратор (локальный сокет узла) и команды Admin API.
    Admin {
        /// Подключение к Admin API.
        #[command(flatten)]
        api: admin_cli::ApiArgs,
        /// Действие.
        #[command(subcommand)]
        action: AdminAction,
    },
}

/// Действия администрирования.
#[derive(Debug, Subcommand)]
pub enum AdminAction {
    /// Первый администратор без OIDC: учётная запись, пароль и одноразовый токен Admin API (выводятся один раз).
    Init {
        /// Адрес администратора (по умолчанию — admin@<domain из config.yaml>).
        #[arg(long)]
        email: Option<String>,
        /// Отображаемое имя.
        #[arg(long)]
        name: Option<String>,
        /// Повтор при уже заведённом администраторе (с записью в аудит).
        #[arg(long)]
        force: bool,
        /// Путь сокета узла (по умолчанию — admin.init.socket).
        #[arg(long)]
        socket: Option<PathBuf>,
    },
    /// Схема Admin API (OpenAPI, JSON) без запуска узла: config.yaml, сеть и токен не нужны.
    Openapi {
        /// Файл для схемы (по умолчанию — stdout).
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
    },
    /// Текстовый интерфейс администратора (TUI): те же ключи подключения, что у `thepost admin`
    /// (`--url`, `--ca-file`, `--insecure`, `--token` или THEPOST_ADMIN_TOKEN); выход — F10.
    Tui,
    /// Команды Admin API: `thepost admin <ресурс> <действие>`.
    #[command(flatten)]
    Api(admin_cli::ApiAction),
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
                out.push_str(&format!(
                    "ссылки на секреты (читаются из хранилища секретов при запуске): {}\n",
                    refs.join(", ")
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
        Command::Verify { fix, json } => match verify::command(&path, fix, None).await {
            Ok(report) => {
                if json {
                    match report.to_json() {
                        Ok(j) => println!("{j}"),
                        Err(e) => return fail(&e),
                    }
                } else {
                    print!("{}", verify::summary(&report));
                }
                verify::exit_code(&report)
            }
            Err(e) => fail(&e),
        },
        Command::Admin {
            action:
                AdminAction::Init {
                    email,
                    name,
                    force,
                    socket,
                },
            ..
        } => {
            let args = local_admin::InitArgs {
                email,
                name,
                force,
                socket,
            };
            local_admin::command(&path, args).await
        }
        Command::Admin {
            action: AdminAction::Openapi { output },
            ..
        } => openapi::command(output.as_deref()).await,
        Command::Admin {
            api,
            action: AdminAction::Api(action),
        } => admin_cli::command(&path, api, action).await,
        Command::Admin {
            api,
            action: AdminAction::Tui,
        } => admin_tui(api).await,
        Command::Serve => serve_until_signal(path).await,
        Command::SearchExtract => {
            // Подпроцесс запускается до рантайма (`main`): сюда приходит только вызов из кода.
            eprintln!("thepost: search-extract — подпроцесс роли indexer, запускается до рантайма");
            2
        }
    }
}

/// `thepost admin tui`: подключение — те же ключи, что у остальных команд `thepost admin`.
async fn admin_tui(api: admin_cli::ApiArgs) -> i32 {
    match tp_tui::run(tui_connect(api)).await {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("thepost: admin tui: {e}");
            1
        }
    }
}

/// Ключи `thepost admin` → подключение текстового интерфейса (токен и адрес без ключей — из THEPOST_ADMIN_*).
fn tui_connect(api: admin_cli::ApiArgs) -> tp_tui::Connect {
    tp_tui::Connect {
        url: api.url,
        token: api.token,
        options: tp_admin_client::Options {
            ca_file: api.ca_file,
            insecure: api.insecure,
            timeout: std::time::Duration::from_secs(api.timeout),
        },
        ..tp_tui::Connect::default()
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
        modules: crate::modules::BUILTIN,
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
        let cli = Cli::try_parse_from(["thepost", "verify", "--fix"]).expect("verify");
        assert!(matches!(
            cli.command,
            Command::Verify {
                fix: true,
                json: false
            }
        ));
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
        let cli = Cli::try_parse_from([
            "thepost",
            "admin",
            "--url",
            "https://127.0.0.1:8443",
            "--ca-file",
            "/ca.pem",
            "--insecure",
            "--token",
            "tps_x",
            "tui",
        ])
        .expect("admin tui");
        let Command::Admin {
            api,
            action: AdminAction::Tui,
        } = cli.command
        else {
            panic!("ждали admin tui");
        };
        let connect = tui_connect(api);
        assert_eq!(connect.url.as_deref(), Some("https://127.0.0.1:8443"));
        assert_eq!(connect.token.as_deref(), Some("tps_x"));
        assert!(connect.options.insecure);
        assert_eq!(connect.options.ca_file, Some(PathBuf::from("/ca.pem")));
        let help = Cli::try_parse_from(["thepost", "admin", "tui", "--help"])
            .expect_err("--help — это выход clap");
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(help.to_string().contains("THEPOST_ADMIN_TOKEN"));
        let cli = Cli::try_parse_from(["thepost", tp_search::extract::WORKER_SUBCOMMAND])
            .expect("worker");
        assert!(matches!(cli.command, Command::SearchExtract));
    }

    #[test]
    fn config_check_is_strict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.yaml");
        let base = "domain: example.org\nprofile: dev\ndatabase:\n  url: postgres://thepost@db/thepost?sslmode=disable\n  password: { secret: \"thepost/db#password\" }\n  tls: { allow_plaintext: true }\nblob:\n  url: file:///var/lib/thepost/blob\n";
        std::fs::write(&path, base).expect("write");
        let (code, out) = config_check(&path);
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("thepost/db#password"), "{out}");

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
