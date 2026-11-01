//! Бинарный файл The Post: разбор CLI, сборка ролей по config.yaml (SRS 2.1).
//!
//! До запуска рантайма tokio: подпроцесс извлечения текста вложений (`search-extract`, seccomp на все потоки) и
//! для `serve` — проверка учётной записи и ограничения ролей (seccomp, Landlock; PLT-16, SYS-07).

use clap::Parser;
use thepost::cli::{Cli, Command};

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::SearchExtract => {
            return match tp_search::extract::run_worker() {
                Ok(()) => std::process::ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("thepost: search-extract: {e}");
                    std::process::ExitCode::FAILURE
                }
            };
        }
        Command::Serve => match thepost::startup::prepare_serve(&cli.config_path()) {
            Ok(report) => {
                for w in report.warnings() {
                    eprintln!("thepost: {w}");
                }
            }
            Err(e) => {
                eprintln!("thepost: {e}");
                return std::process::ExitCode::FAILURE;
            }
        },
        _ => {}
    }
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("thepost: runtime tokio: {e}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(thepost::cli::run(cli));
    std::process::ExitCode::from(u8::try_from(code).unwrap_or(1))
}
