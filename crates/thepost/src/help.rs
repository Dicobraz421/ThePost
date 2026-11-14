//! Справка и ошибки разбора аргументов `thepost` (APP-70, решение 120): `clap::builder::Styles` по ролям
//! [`tp_admin_client::style`] и разделы по-русски.
//!
//! - Разделы («Использование:», «Команды:», «Аргументы:», «Параметры:») — `head`; подкоманды и флаги — `accent`;
//!   «[по умолчанию: …]» и «[значения: …]» — `muted`; ошибки разбора — `err`. Своих цветов здесь нет.
//! - Цвет или нет — решает clap через anstream по тому же выбору, что у команд: `--color auto|always|never`,
//!   `NO_COLOR`, `CLICOLOR_FORCE`, `TERM=dumb`, терминал ли поток. Без цвета — тот же текст без ESC.

use std::ffi::OsString;

use clap::builder::{StyledStr, Styles};
use clap::{Arg, ArgAction, Command};
use tp_admin_client::style::{Role, Theme};

use crate::admin_cli::ColorArg;

/// Раздел команд.
const COMMANDS: &str = "Команды";
/// Раздел позиционных аргументов.
const ARGUMENTS: &str = "Аргументы";
/// Раздел флагов и ключей.
const OPTIONS: &str = "Параметры";

/// Стили clap по ролям `theme`: без цвета — пустые стили.
pub fn styles(theme: Theme) -> Styles {
    Styles::plain()
        .header(theme.style(Role::Head))
        .usage(theme.style(Role::Head))
        .literal(theme.style(Role::Accent))
        .error(theme.style(Role::Err))
        .invalid(theme.style(Role::Warn))
        .valid(theme.style(Role::Ok))
        .context(theme.style(Role::Muted))
        .context_value(theme.style(Role::Muted))
}

/// Дерево команд `cmd` с разделами по-русски и стилями `theme`; цвет при выводе — `color`.
pub fn localize(cmd: Command, theme: Theme, color: clap::ColorChoice) -> Command {
    let version = Arg::new("version")
        .short('V')
        .long("version")
        .action(ArgAction::Version)
        .help("Версия")
        .help_heading(OPTIONS);
    command(cmd, theme)
        .disable_version_flag(true)
        .arg(version)
        .styles(styles(theme))
        .color(color)
}

/// Одна команда и её подкоманды: разделы, флаг справки, значения по умолчанию.
fn command(cmd: Command, theme: Theme) -> Command {
    let head = theme.style(Role::Head);
    let template = format!(
        "{{before-help}}{{about-with-newline}}\n{}Использование:{} {{usage}}\n\n{{all-args}}{{after-help}}",
        head.render(),
        head.render_reset()
    );
    let help = Arg::new("help")
        .short('h')
        .long("help")
        .action(ArgAction::Help)
        .help("Справка")
        .help_heading(OPTIONS);
    cmd.help_template(StyledStr::from(template))
        .subcommand_help_heading(COMMANDS)
        .subcommand_value_name("КОМАНДА")
        .disable_help_subcommand(true)
        .disable_help_flag(true)
        .mut_args(|a| arg(a, theme))
        .arg(help)
        .mut_subcommands(|c| command(c, theme))
}

/// Аргумент: раздел по виду, «[по умолчанию: …]» и «[значения: …]» — в тексте справки (`muted`).
fn arg(a: Arg, theme: Theme) -> Arg {
    let heading = if a.is_positional() {
        ARGUMENTS
    } else {
        OPTIONS
    };
    // До сборки команды clap ещё не отметил, принимает ли аргумент значение: решает действие.
    let takes_values = a.get_action().takes_values();
    let mut notes = Vec::new();
    let values: Vec<String> = a
        .get_value_parser()
        .possible_values()
        .filter(|_| takes_values)
        .into_iter()
        .flatten()
        .filter(|v| !v.is_hide_set())
        .map(|v| quote(v.get_name()))
        .collect();
    if !values.is_empty() {
        notes.push(format!("[значения: {}]", values.join(", ")));
    }
    if takes_values && !a.get_default_values().is_empty() {
        let defaults: Vec<String> = a
            .get_default_values()
            .iter()
            .map(|v| quote(&v.to_string_lossy()))
            .collect();
        notes.push(format!("[по умолчанию: {}]", defaults.join(" ")));
    }
    let a = if a.get_help_heading().is_none() {
        a.help_heading(heading)
    } else {
        a
    };
    if notes.is_empty() {
        return a;
    }
    let note = theme.paint(Role::Muted, &notes.join(" "));
    let help = a.get_help().map(|h| format!("{} {note}", h.ansi()));
    let long = a.get_long_help().map(|h| format!("{}\n\n{note}", h.ansi()));
    let a = a
        .hide_possible_values(true)
        .hide_default_value(true)
        .help(StyledStr::from(help.unwrap_or(note)));
    match long {
        Some(long) => a.long_help(StyledStr::from(long)),
        None => a,
    }
}

/// Значение в кавычках, если оно пустое или с пробелами (как у clap).
fn quote(v: &str) -> String {
    if v.is_empty() || v.contains(char::is_whitespace) {
        format!("{v:?}")
    } else {
        v.to_owned()
    }
}

/// Выбор `--color` из командной строки до разбора: справка и ошибки разбора выводятся до того, как clap разберёт
/// ключи команды. Последний ключ сильнее; неверное значение — `auto` (ошибку даст разбор).
pub fn color_arg(args: &[OsString]) -> ColorArg {
    use clap::ValueEnum as _;
    let mut found = ColorArg::Auto;
    let mut it = args.iter().skip(1).map(|a| a.to_string_lossy());
    while let Some(a) = it.next() {
        if a == "--" {
            break;
        }
        let value = match a.strip_prefix("--color") {
            Some("") => it.next().map(|v| v.into_owned()),
            Some(rest) => rest.strip_prefix('=').map(str::to_owned),
            None => None,
        };
        if let Some(v) = value.and_then(|v| ColorArg::from_str(&v, false).ok()) {
            found = v;
        }
    }
    found
}
