//! Командная строка `thepost` (APP-01..03; `thepost admin` — APP-19, APP-11; `thepost admin openapi` — APP-37).
//!
//! Путь к config.yaml — `--config`, иначе переменная `THEPOST_CONFIG`, иначе `/etc/thepost/config.yaml`.
//!
//! Справка, `doctor` и `config check` — в цвете по ролям [`tp_admin_client::style`] (APP-70, решение 120):
//! `--color auto|always|never`, как у `thepost admin`; данные — в stdout, ошибки — в stderr.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use tp_admin_client::style::{self, ColorChoice, Role, State, Theme};
use tp_contracts::Error;
use tp_contracts::role::CancellationToken;
use tp_platform::config::ENV_CONFIG_PATH;

use crate::admin_cli::ColorArg;
use crate::{admin_cli, doctor, help, local_admin, openapi, secrets, sections, serve, verify};

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
        /// Цвет вывода.
        #[command(flatten)]
        color: ColorArgs,
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
    Check {
        /// Цвет вывода.
        #[command(flatten)]
        color: ColorArgs,
    },
}

/// Ключ `--color` команд узла — тот же выбор, что у `thepost admin` ([`ColorArg`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::Args)]
pub struct ColorArgs {
    /// Цвет вывода: auto — только на терминал (учитывает NO_COLOR, CLICOLOR_FORCE, TERM=dumb), always, never.
    #[arg(long, value_enum, default_value_t = ColorArg::Auto)]
    pub color: ColorArg,
}

impl ColorArgs {
    /// Оформление stdout (данные).
    fn theme(self) -> Theme {
        Theme::stdout(choice(self.color))
    }

    /// Оформление stderr (ошибки).
    fn err_theme(self) -> Theme {
        Theme::stderr(choice(self.color))
    }
}

/// `--color` → выбор anstream.
fn choice(c: ColorArg) -> ColorChoice {
    match c {
        ColorArg::Auto => ColorChoice::Auto,
        ColorArg::Always => ColorChoice::Always,
        ColorArg::Never => ColorChoice::Never,
    }
}

/// Дерево команд `thepost` со справкой по-русски в стилях `theme`; цвет справки и ошибок разбора — `color`.
pub fn command(theme: Theme, color: ColorArg) -> clap::Command {
    let color = match color {
        ColorArg::Auto => clap::ColorChoice::Auto,
        ColorArg::Always => clap::ColorChoice::Always,
        ColorArg::Never => clap::ColorChoice::Never,
    };
    help::localize(Cli::command(), theme, color)
}

/// Разбор командной строки `args` (первый элемент — имя программы); справка и ошибка разбора — `Err`.
pub fn try_parse_from<I, T>(args: I) -> Result<Cli, clap::Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    // Стили — всегда в цвете: без цвета (выбор, переменные, не терминал) clap выводит тот же текст без ESC.
    let mut cmd = command(Theme::stdout(ColorChoice::Always), help::color_arg(&args));
    let matches = cmd.try_get_matches_from_mut(args)?;
    Cli::from_arg_matches(&matches).map_err(|e| e.format(&mut cmd))
}

/// Разбор командной строки процесса; справка, версия и ошибка разбора — вывод clap и выход.
pub fn parse() -> Cli {
    try_parse_from(std::env::args_os()).unwrap_or_else(|e| e.exit())
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

/// Результат `thepost config check`: код возврата и текст — при успехе для stdout в оформлении `out`
/// («✓ конфигурация верна»), при ошибке для stderr в оформлении `err` (× и путь к полю на каждую ошибку).
pub fn config_check(path: &Path, out: Theme, err: Theme) -> (i32, String) {
    let shown = path.display().to_string();
    match sections::registry().and_then(|r| r.check(path)) {
        Ok(cfg) => {
            let mut text = format!(
                "{}: {}\n",
                out.state(State::Done, "конфигурация верна"),
                out.paint(Role::Muted, &shown)
            );
            let refs = secrets::references(&cfg);
            if !refs.is_empty() {
                text.push_str(&format!(
                    "ссылки на секреты (читаются из хранилища секретов при запуске): {}\n",
                    refs.join(", ")
                ));
            }
            (0, text)
        }
        Err(e) => {
            let msg = match e {
                Error::Invalid(m) => m,
                other => other.to_string(),
            };
            let mut text = format!(
                "{}: {}\n",
                err.state(State::Err, "ошибки конфигурации"),
                err.paint(Role::Muted, &shown)
            );
            for line in msg.lines() {
                text.push_str(&format!(
                    "  {} {}\n",
                    err.paint(Role::Err, State::Err.sign()),
                    config_error(err, line)
                ));
            }
            (1, text)
        }
    }
}

/// Строка ошибки конфигурации: путь к полю (`telemetry.log_levl: …`) — `accent`; строка без пути — как есть.
fn config_error(theme: Theme, line: &str) -> String {
    match line.split_once(": ") {
        Some((field, rest)) if !field.is_empty() && !field.contains(char::is_whitespace) => {
            format!("{}: {rest}", theme.paint(Role::Accent, field))
        }
        _ => line.to_owned(),
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
            action: ConfigAction::Check { color },
        } => {
            let (out, err) = (color.theme(), color.err_theme());
            let (code, text) = config_check(&path, out, err);
            if code == 0 {
                style::print(out, &text);
            } else {
                style::eprint(err, &text);
            }
            code
        }
        Command::Doctor { json, color } => {
            let theme = color.theme();
            let report = doctor::run(&path, None).await;
            if json {
                match report.to_json(theme) {
                    Ok(j) => style::print(theme, &format!("{j}\n")),
                    Err(e) => return fail(&e),
                }
            } else {
                style::print(theme, &report.render(theme));
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
        let cli =
            try_parse_from(["thepost", "--config", "/x.yaml", "doctor", "--json"]).expect("doctor");
        assert!(matches!(cli.command, Command::Doctor { json: true, .. }));
        assert_eq!(cli.config_path(), PathBuf::from("/x.yaml"));
        let cli = try_parse_from(["thepost", "verify", "--fix"]).expect("verify");
        assert!(matches!(
            cli.command,
            Command::Verify {
                fix: true,
                json: false
            }
        ));
        let cli = try_parse_from([
            "thepost",
            "config",
            "check",
            "-c",
            "/y.yaml",
            "--color=never",
        ])
        .expect("check");
        assert!(matches!(
            cli.command,
            Command::Config {
                action: ConfigAction::Check {
                    color: ColorArgs {
                        color: ColorArg::Never
                    }
                }
            }
        ));
        assert_eq!(cli.config_path(), PathBuf::from("/y.yaml"));
        assert!(try_parse_from(["thepost", "bogus"]).is_err());
        let cli = try_parse_from([
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
        let help = try_parse_from(["thepost", "admin", "tui", "--help"])
            .expect_err("--help — это выход clap");
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(help.to_string().contains("THEPOST_ADMIN_TOKEN"));
        let cli =
            try_parse_from(["thepost", tp_search::extract::WORKER_SUBCOMMAND]).expect("worker");
        assert!(matches!(cli.command, Command::SearchExtract));
    }

    #[test]
    fn config_check_is_strict() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.yaml");
        let base = "domain: example.org\nprofile: dev\ndatabase:\n  url: postgres://thepost@db/thepost?sslmode=disable\n  password: { secret: \"thepost/db#password\" }\n  tls: { allow_plaintext: true }\nblob:\n  url: file:///var/lib/thepost/blob\n";
        std::fs::write(&path, base).expect("write");
        let (code, out) = config_check(&path, Theme::PLAIN, Theme::PLAIN);
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("thepost/db#password"), "{out}");

        std::fs::write(&path, format!("{base}telemetry:\n  log_levl: info\n")).expect("write");
        let (code, out) = config_check(&path, Theme::PLAIN, Theme::PLAIN);
        assert_eq!(code, 1);
        assert!(out.contains("telemetry.log_levl"), "{out}");

        // Тот же файл при запуске — мягкий режим: только предупреждение.
        let cfg = sections::registry()
            .expect("registry")
            .load(&path)
            .expect("lenient");
        assert_eq!(cfg.warnings().len(), 1);
    }

    /// ESC — `^[`, как в снимках `tp_admin_client::style`.
    fn show(s: &str) -> String {
        s.replace('\u{1b}', "^[")
    }

    /// Сверка со снимком; расхождение — весь вывод в stderr для правки снимка.
    fn snapshot(name: &str, got: &str, want: &str) {
        let got = show(got);
        if got != want {
            eprintln!("=== снимок {name} ===\n{got}=== конец ===");
        }
        assert_eq!(got, want, "{name}");
    }

    /// Справка `thepost <args> --help` в стилях `theme` (с ESC, как её отдаёт clap до выбора цвета).
    fn help(theme: Theme, args: &[&str]) -> String {
        let e = command(theme, ColorArg::Always)
            .try_get_matches_from(args)
            .expect_err("--help — это выход clap");
        assert_eq!(e.kind(), clap::error::ErrorKind::DisplayHelp);
        e.render().ansi().to_string()
    }

    /// Справка `thepost admin account --help`: разделы — `head`, подкоманды и флаги — `accent`, значения по
    /// умолчанию — `muted`; без цвета — тот же текст.
    #[test]
    fn help_snapshots() {
        let args = ["thepost", "admin", "account", "--help"];
        let plain = help(Theme::PLAIN, &args);
        let ansi = help(Theme::ANSI16, &args);
        assert_eq!(style::strip(&ansi), plain);
        snapshot("help/ansi16", &ansi, HELP_ANSI16);
        snapshot("help/plain", &plain, HELP_PLAIN);
    }

    /// Ошибка разбора аргументов — «error:» в роли `err`.
    #[test]
    fn parse_error_is_err() {
        let e = command(Theme::ANSI16, ColorArg::Always)
            .try_get_matches_from(["thepost", "doctor", "--colour"])
            .expect_err("неизвестный ключ");
        let err = Theme::ANSI16.style(Role::Err);
        let shown = e.render().ansi().to_string();
        assert!(
            shown.starts_with(&format!("{}error:{}", err.render(), err.render_reset())),
            "{}",
            show(&shown)
        );
    }

    /// `--color` до разбора: последний ключ сильнее, после `--` — не ключ, неверное значение — `auto`.
    #[test]
    fn color_arg_before_parse() {
        let color = |a: &[&str]| help::color_arg(&a.iter().map(OsString::from).collect::<Vec<_>>());
        assert_eq!(color(&["thepost", "doctor"]), ColorArg::Auto);
        assert_eq!(
            color(&["thepost", "doctor", "--color", "never"]),
            ColorArg::Never
        );
        assert_eq!(
            color(&["thepost", "--color=never", "doctor", "--color=always"]),
            ColorArg::Always
        );
        assert_eq!(
            color(&["thepost", "--", "--color", "never"]),
            ColorArg::Auto
        );
        assert_eq!(color(&["thepost", "--color", "x"]), ColorArg::Auto);
    }

    /// Отчёт `doctor` для снимков: по проверке каждого итога.
    fn sample_report() -> doctor::Report {
        let mut r = doctor::Report::default();
        r.push(
            "config.load",
            doctor::Status::Ok,
            "/etc/thepost/config.yaml",
            "конфигурация разобрана",
        );
        r.push(
            "secrets.refs",
            doctor::Status::Skipped,
            "",
            "ссылок на секреты нет",
        );
        r.push(
            "database.tls",
            doctor::Status::Warn,
            "prefer",
            "канал без проверки сертификата",
        );
        r.push(
            "blob.write",
            doctor::Status::Fail,
            "local",
            "нет прав на запись",
        );
        r
    }

    /// `doctor`: знак и метка итога, пути — `muted`, счёт в цвете худшего итога; без цвета знаки те же.
    #[test]
    fn doctor_snapshots() {
        let r = sample_report();
        snapshot("doctor/plain", &r.render(Theme::PLAIN), DOCTOR_PLAIN);
        snapshot("doctor/ansi16", &r.render(Theme::ANSI16), DOCTOR_ANSI16);
        let truecolor = r.render(Theme::TRUECOLOR);
        assert_eq!(style::strip(&truecolor), DOCTOR_PLAIN);
        assert!(truecolor.contains("\u{1b}[38;2;"), "{}", show(&truecolor));
        // Счёт: формы слова и цвет худшего итога.
        let mut r = doctor::Report::default();
        r.push("a", doctor::Status::Ok, "", "");
        assert_eq!(
            show(&r.render(Theme::ANSI16)).lines().last(),
            Some("^[[92m✓ итог: ошибок и предупреждений нет^[[0m")
        );
        for _ in 0..2 {
            r.push("b", doctor::Status::Warn, "", "");
        }
        assert!(
            r.render(Theme::PLAIN)
                .ends_with("▲ итог: 2 предупреждения\n")
        );
        for _ in 0..5 {
            r.push("c", doctor::Status::Fail, "", "");
        }
        assert!(
            r.render(Theme::PLAIN)
                .ends_with("× итог: 5 ошибок, 2 предупреждения\n")
        );
        assert_eq!(r.exit_code(), 1);
        // JSON без цвета — прежний текст байт в байт.
        assert_eq!(
            r.to_json(Theme::PLAIN).expect("json"),
            serde_json::to_string_pretty(&r).expect("pretty")
        );
    }

    /// `config check`: ✓ при успехе; × и путь к полю на каждую ошибку.
    #[test]
    fn config_check_snapshots() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.yaml");
        let shown = path.display().to_string();
        let base = "domain: example.org\nprofile: dev\ndatabase:\n  url: postgres://thepost@db/thepost?sslmode=disable\n  tls: { allow_plaintext: true }\nblob:\n  url: file:///var/lib/thepost/blob\n";
        let run = |yaml: &str, t: Theme| {
            std::fs::write(&path, yaml).expect("write");
            let (code, out) = config_check(&path, t, t);
            (code, out.replace(&shown, "<путь>"))
        };
        let bad = format!("{base}telemetry:\n  log_levl: info\n");
        for (name, t, ok, err) in [
            ("plain", Theme::PLAIN, CHECK_OK_PLAIN, CHECK_ERR_PLAIN),
            ("ansi16", Theme::ANSI16, CHECK_OK_ANSI16, CHECK_ERR_ANSI16),
        ] {
            let (code, out) = run(base, t);
            assert_eq!(code, 0, "{out}");
            snapshot(&format!("config-ok/{name}"), &out, ok);
            let (code, out) = run(&bad, t);
            assert_eq!(code, 1, "{out}");
            snapshot(&format!("config-err/{name}"), &out, err);
        }
    }

    /// Дочерний процесс для [`stream_choice`]: `thepost` с ключами из `TP_CLI_ARGS` (через `\x1f`), вывод — между
    /// метками.
    #[tokio::test]
    #[ignore = "запускается из stream_choice"]
    async fn child() {
        let Ok(args) = std::env::var("TP_CLI_ARGS") else {
            return;
        };
        style::print(Theme::PLAIN, "<<<");
        match try_parse_from(std::iter::once("thepost").chain(args.split('\u{1f}'))) {
            Ok(cli) => {
                run(cli).await;
            }
            Err(e) => {
                let _ = e.print();
            }
        }
        style::print(Theme::PLAIN, ">>>");
    }

    /// Выбор цвета по потоку и переменным: `doctor` и справка в конвейер при `auto` — без ESC; `--color always` и
    /// `CLICOLOR_FORCE` красят и в конвейер, `NO_COLOR` сильнее `CLICOLOR_FORCE`.
    #[test]
    fn stream_choice() {
        let run = |args: &[&str], env: &[(&str, &str)]| {
            let mut cmd = std::process::Command::new(std::env::current_exe().expect("exe"));
            cmd.args(["--exact", "cli::tests::child", "--ignored", "--nocapture"])
                .env("TP_CLI_ARGS", args.join("\u{1f}"));
            for var in [
                "NO_COLOR",
                "CLICOLOR",
                "CLICOLOR_FORCE",
                "COLORTERM",
                "TERM",
            ] {
                cmd.env_remove(var);
            }
            cmd.envs(env.iter().copied());
            let out = cmd.output().expect("дочерний процесс");
            let stdout = String::from_utf8(out.stdout).expect("utf-8");
            let (_, rest) = stdout.split_once("<<<").expect("начало вывода");
            let (body, _) = rest.split_once(">>>").expect("конец вывода");
            body.to_owned()
        };
        let missing = ["-c", "/нет/config.yaml", "doctor"];
        let piped = run(&missing, &[]);
        assert!(piped.contains("× FAIL"), "{piped}");
        assert!(!piped.contains('\u{1b}'), "{}", show(&piped));
        let forced = run(&[&missing[..], &["--color", "always"]].concat(), &[]);
        assert!(forced.contains('\u{1b}'), "{forced}");
        assert_eq!(style::strip(&forced), piped);
        let json = run(&[&missing[..], &["--json"]].concat(), &[]);
        assert!(!json.contains('\u{1b}'), "{}", show(&json));
        serde_json::from_str::<serde_json::Value>(&json).expect("чистый JSON");

        let help = ["doctor", "--help"];
        let plain = run(&help, &[]);
        assert!(plain.contains("Использование:"), "{plain}");
        assert!(!plain.contains('\u{1b}'), "{}", show(&plain));
        assert!(run(&help, &[("CLICOLOR_FORCE", "1")]).contains('\u{1b}'));
        assert!(run(&["doctor", "--color", "always", "--help"], &[]).contains('\u{1b}'));
        assert_eq!(
            run(&help, &[("CLICOLOR_FORCE", "1"), ("NO_COLOR", "1")]),
            plain
        );
        assert_eq!(
            run(
                &["doctor", "--color=never", "--help"],
                &[("CLICOLOR_FORCE", "1")]
            ),
            plain
        );
    }

    const HELP_PLAIN: &str = r#"Учётные записи

Использование: thepost admin account [OPTIONS] <КОМАНДА>

Команды:
  create   Новая локальная учётная запись в принятом домене; пароль — `app-password create`
  list     Учётные записи по возрастанию имени входа (страницы Admin API — по ссылке Link rel="next"); --login — поиск одной
  get      Учётная запись по идентификатору или имени входа
  block    Блокировка входа локальной учётной записи: новый вход запрещён, открытые сеансы не разрываются. Запись каталога блокируется в каталоге (ответ 409)
  unblock  Снятие блокировки входа локальной учётной записи
  rename   Смена отображаемого имени локальной учётной записи (у записи каталога — 409)
  set      Псевдонимы и скрытие из книги «Организация» локальной учётной записи (у записи каталога — 409, их ведёт каталог). Нужен хотя бы один ключ

Параметры:
  -c, --config <CONFIG>    Путь к config.yaml (по умолчанию — THEPOST_CONFIG или /etc/thepost/config.yaml)
      --url <URL>          Адрес Admin API (по умолчанию — THEPOST_ADMIN_URL или http.admin.listen из config.yaml)
      --token <TOKEN>      Токен: сессия tps_… или токен OIDC (по умолчанию — THEPOST_ADMIN_TOKEN; ключ виден в списке процессов)
      --org <ORG>          Организация, UUID (по умолчанию — THEPOST_ADMIN_ORG или организация администратора)
      --json               Вывод — ответ Admin API в JSON
      --ca-file <CA_FILE>  Корневые сертификаты (PEM) для проверки TLS Admin API
      --insecure           Не проверять сертификат TLS (как curl -k) — только для loopback узла с сертификатом на другое имя
      --timeout <TIMEOUT>  Предельное время запроса, секунд [по умолчанию: 30]
      --color <COLOR>      Цвет вывода: auto — только на терминал (учитывает NO_COLOR, CLICOLOR_FORCE, TERM=dumb), always, never [значения: auto, always, never] [по умолчанию: auto]
  -h, --help               Справка
"#;

    const HELP_ANSI16: &str = "";

    const DOCTOR_PLAIN: &str = "ИТОГ    ПРОВЕРКА      ЗНАЧЕНИЕ                  ПОЯСНЕНИЕ
✓ OK    config.load   /etc/thepost/config.yaml  конфигурация разобрана
  SKIP  secrets.refs                            ссылок на секреты нет
▲ WARN  database.tls  prefer                    канал без проверки сертификата
× FAIL  blob.write    local                     нет прав на запись
× итог: 1 ошибка, 1 предупреждение
";

    const DOCTOR_ANSI16: &str =
        "^[[1m^[[94mИТОГ    ПРОВЕРКА      ЗНАЧЕНИЕ                  ПОЯСНЕНИЕ^[[0m
^[[92m✓ OK^[[0m    config.load   ^[[2m/etc/thepost/config.yaml^[[0m  конфигурация разобрана
  ^[[2mSKIP^[[0m  secrets.refs                            ссылок на секреты нет
^[[1m^[[33m▲ WARN^[[0m  database.tls  prefer                    канал без проверки сертификата
^[[1m^[[91m× FAIL^[[0m  blob.write    local                     нет прав на запись
^[[1m^[[91m× итог: 1 ошибка, 1 предупреждение^[[0m
";

    const CHECK_OK_PLAIN: &str = "✓ конфигурация верна: <путь>\n";

    const CHECK_OK_ANSI16: &str = "^[[92m✓ конфигурация верна^[[0m: ^[[2m<путь>^[[0m\n";

    const CHECK_ERR_PLAIN: &str = "× ошибки конфигурации: <путь>
  × telemetry.log_levl: unknown parameter
";

    const CHECK_ERR_ANSI16: &str = "^[[1m^[[91m× ошибки конфигурации^[[0m: ^[[2m<путь>^[[0m
  ^[[1m^[[91m×^[[0m ^[[95mtelemetry.log_levl^[[0m: unknown parameter
";
}
