//! Бинарный файл The Post: разбор CLI, сборка ролей по config.yaml (SRS 2.1).

use clap::Parser;

fn main() -> std::process::ExitCode {
    let cli = thepost::cli::Cli::parse();
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
