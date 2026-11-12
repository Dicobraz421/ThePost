//! `thepost admin openapi` (APP-37): схема Admin API без запуска узла — без config.yaml, сети и токена.
//!
//! Схема — `tp_admin::server::openapi_spec()` (её же узел отдаёт по `GET /api/openapi.json`) в JSON с отступами,
//! ключи объектов — по алфавиту (порядок не зависит от сборки), в конце — перевод строки. Так получают
//! `console/src/api/openapi.json`.

use std::io::Write as _;
use std::path::Path;

use serde_json::{Map, Value};
use tp_contracts::{Error, Result};

/// Значение с ключами объектов по алфавиту на всех уровнях; порядок элементов массивов сохраняется.
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> =
                map.into_iter().map(|(k, v)| (k, sorted(v))).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(Map::from_iter(entries))
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted).collect()),
        other => other,
    }
}

/// Текст схемы Admin API: JSON с отступами, ключи по алфавиту, перевод строки в конце.
pub fn render() -> Result<String> {
    let spec = serde_json::to_value(tp_admin::server::openapi_spec())
        .map_err(|e| Error::Internal(format!("схема Admin API: {e}")))?;
    let mut text = serde_json::to_string_pretty(&sorted(spec))
        .map_err(|e| Error::Internal(format!("схема Admin API: {e}")))?;
    text.push('\n');
    Ok(text)
}

/// Команда: схема — в файл `output` либо в stdout; код возврата процесса.
pub async fn command(output: Option<&Path>) -> i32 {
    let text = match render() {
        Ok(text) => text,
        Err(e) => {
            eprintln!("thepost: {e}");
            return 1;
        }
    };
    match output {
        Some(path) => match tokio::fs::write(path, text).await {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("thepost: {}: {e}", path.display());
                1
            }
        },
        // Не `print!`: закрытый получатель (`| head`) — код 1, а не паника.
        None => match std::io::stdout().lock().write_all(text.as_bytes()) {
            Ok(()) => 0,
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => 1,
            Err(e) => {
                eprintln!("thepost: stdout: {e}");
                1
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    use super::*;
    use crate::cli::{AdminAction, Cli, Command};

    /// Ключи объектов на всех уровнях идут по алфавиту.
    fn is_sorted(value: &Value) -> bool {
        match value {
            Value::Object(map) => {
                let keys: Vec<&String> = map.keys().collect();
                keys.windows(2).all(|w| w[0] < w[1]) && map.values().all(is_sorted)
            }
            Value::Array(items) => items.iter().all(is_sorted),
            _ => true,
        }
    }

    #[test]
    fn output_equals_openapi_spec() {
        let text = render().expect("render");
        assert!(text.ends_with("}\n") && !text.ends_with("\n\n"), "конец");
        let parsed: Value = serde_json::from_str(&text).expect("json");
        let spec = serde_json::to_value(tp_admin::server::openapi_spec()).expect("spec");
        assert_eq!(parsed, spec);
        assert!(parsed["paths"].as_object().is_some_and(|p| !p.is_empty()));
        // Стабильный порядок: ключи по алфавиту, повторный вывод — байт в байт.
        assert!(is_sorted(&sorted(spec)));
        let top: Vec<usize> = ["\n  \"components\"", "\n  \"info\"", "\n  \"paths\""]
            .iter()
            .map(|key| text.find(key).expect("ключ верхнего уровня"))
            .collect();
        assert!(top.windows(2).all(|w| w[0] < w[1]), "{top:?}");
        assert_eq!(text, render().expect("render"));
    }

    /// Команда не читает config.yaml (путь — несуществующий файл) и пишет схему в файл.
    #[tokio::test]
    async fn works_without_config() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("openapi.json");
        let missing = dir.path().join("нет.yaml");
        let cli = Cli::try_parse_from([
            "thepost".as_ref(),
            "--config".as_ref(),
            missing.as_os_str(),
            "admin".as_ref(),
            "openapi".as_ref(),
            "--output".as_ref(),
            out.as_os_str(),
        ])
        .expect("cli");
        assert!(matches!(
            &cli.command,
            Command::Admin {
                action: AdminAction::Openapi { output: Some(_) },
                ..
            }
        ));
        assert_eq!(crate::cli::run(cli).await, 0);
        let written = std::fs::read_to_string(&out).expect("файл схемы");
        assert_eq!(written, render().expect("render"));

        // Без `--output` — stdout; каталога для файла нет — ошибка с кодом 1.
        let cli = Cli::try_parse_from(["thepost", "admin", "openapi"]).expect("cli");
        assert!(matches!(
            cli.command,
            Command::Admin {
                action: AdminAction::Openapi { output: None },
                ..
            }
        ));
        assert_eq!(command(Some(&dir.path().join("нет/openapi.json"))).await, 1);
    }
}
