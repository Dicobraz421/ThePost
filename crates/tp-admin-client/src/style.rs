//! Подсветка вывода CLI (решение 120, ADM-46): роли, знаки состояний и готовые куски вывода `thepost admin` —
//! таблицы, отказ Admin API, было/стало, JSON.
//!
//! - Цвета ролей — только из раздела `[cli]` файла `design/tokens.toml` (вшит при сборке, разбор `toml`); второго
//!   списка цветов в коде нет. По умолчанию — 16 цветов ANSI (параметры SGR `ansi`), при
//!   `COLORTERM=truecolor|24bit` — фирменные значения (`brand` с атрибутами `attrs`).
//! - Цвет или нет — решает [`anstream`]: `--color auto|always|never`, `NO_COLOR`, `CLICOLOR_FORCE`, `TERM=dumb`,
//!   терминал ли поток. Не терминал (файл, конвейер) — [`Theme::PLAIN`]: ни одной последовательности ESC, текст тот
//!   же. Вывод — [`print`] (stdout, данные) и [`eprint`] (stderr, ошибки и подсказки).
//! - У каждого состояния есть знак ([`State`]): без цвета вывод читается так же.

use std::io::Write as _;
use std::sync::LazyLock;

use anstream::AutoStream;
pub use anstream::ColorChoice;
use anstream::stream::RawStream;
use anstyle::{AnsiColor, Color, Effects, RgbColor, Style};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::ser::{CompactFormatter, Formatter, PrettyFormatter};
use tp_admin::accounts::AccountInfo;

use crate::ops::Op;

/// Токены фирменного стиля (решение 119, DES-01): единственный источник цветов.
const TOKENS: &str = include_str!("../../../design/tokens.toml");

/// Знак успешного действия («✓ Сохранено»); знаки состояний ● ▲ × — из `[signs]` токенов.
const DONE_SIGN: &str = "✓";

/// Роль куска вывода (`[cli.roles]` токенов).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Шапки таблиц, разделы справки, «подсказка:».
    Head,
    /// Знак `$`, метод и путь API, флаги и подкоманды.
    Accent,
    /// Успех, норма, добавленная строка.
    Ok,
    /// Внимание: квота.
    Warn,
    /// «ошибка:», код HTTP, удалённая строка.
    Err,
    /// Листание, пути файлов, `null`, значения по умолчанию.
    Muted,
    /// Ключ JSON.
    JsonKey,
    /// Число JSON.
    JsonNumber,
    /// `true` и `false` JSON.
    JsonBool,
}

impl Role {
    /// Все роли; индекс в палитре — `role as usize`.
    const ALL: [Role; 9] = [
        Role::Head,
        Role::Accent,
        Role::Ok,
        Role::Warn,
        Role::Err,
        Role::Muted,
        Role::JsonKey,
        Role::JsonNumber,
        Role::JsonBool,
    ];
}

/// Состояние: знак и роль.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// ✓ — действие выполнено.
    Done,
    /// ● — норма.
    Ok,
    /// ▲ — внимание.
    Warn,
    /// × — ошибка, заблокировано.
    Err,
}

impl State {
    /// Знак состояния.
    pub fn sign(self) -> &'static str {
        let signs = &palette().signs;
        match self {
            State::Done => DONE_SIGN,
            State::Ok => &signs.ok,
            State::Warn => &signs.warn,
            State::Err => &signs.err,
        }
    }

    fn role(self) -> Role {
        match self {
            State::Done | State::Ok => Role::Ok,
            State::Warn => Role::Warn,
            State::Err => Role::Err,
        }
    }
}

/// Раздел токенов, нужный CLI.
#[derive(Debug, Deserialize)]
struct Tokens {
    signs: Signs,
    cli: CliTokens,
}

#[derive(Debug, Deserialize)]
struct Signs {
    ok: String,
    warn: String,
    err: String,
}

#[derive(Debug, Deserialize)]
struct CliTokens {
    roles: Roles,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Roles {
    head: RoleToken,
    accent: RoleToken,
    ok: RoleToken,
    warn: RoleToken,
    err: RoleToken,
    muted: RoleToken,
    json_key: RoleToken,
    json_number: RoleToken,
    json_bool: RoleToken,
}

impl Roles {
    fn get(&self, role: Role) -> &RoleToken {
        match role {
            Role::Head => &self.head,
            Role::Accent => &self.accent,
            Role::Ok => &self.ok,
            Role::Warn => &self.warn,
            Role::Err => &self.err,
            Role::Muted => &self.muted,
            Role::JsonKey => &self.json_key,
            Role::JsonNumber => &self.json_number,
            Role::JsonBool => &self.json_bool,
        }
    }
}

/// Роль в токенах: параметры SGR (16 цветов), фирменный цвет и его атрибуты (truecolor).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleToken {
    ansi: String,
    brand: String,
    attrs: Vec<String>,
}

/// Разобранные токены: стили ролей в двух режимах и знаки.
#[derive(Debug)]
struct Palette {
    ansi16: [Style; 9],
    truecolor: [Style; 9],
    signs: Signs,
}

/// Палитра из вшитых токенов; токены проверяет тест [`tests::tokens_parse`] — сбой разбора здесь невозможен
/// без красного CI.
fn palette() -> &'static Palette {
    static PALETTE: LazyLock<Palette> = LazyLock::new(|| {
        load(TOKENS).unwrap_or_else(|e| panic!("design/tokens.toml, раздел [cli]: {e}"))
    });
    &PALETTE
}

fn load(text: &str) -> Result<Palette, String> {
    let t: Tokens = toml::from_str(text).map_err(|e| e.to_string())?;
    let mut ansi16 = [Style::new(); 9];
    let mut truecolor = [Style::new(); 9];
    for (i, role) in Role::ALL.into_iter().enumerate() {
        let token = t.cli.roles.get(role);
        ansi16[i] = sgr(&token.ansi).map_err(|e| format!("{role:?}: ansi: {e}"))?;
        truecolor[i] = brand(&token.brand, &token.attrs).map_err(|e| format!("{role:?}: {e}"))?;
    }
    Ok(Palette {
        ansi16,
        truecolor,
        signs: t.signs,
    })
}

/// Стиль из параметров SGR (`1;94`): атрибуты 1–4, цвета 30–37 и 90–97.
fn sgr(params: &str) -> Result<Style, String> {
    const ANSI: [AnsiColor; 8] = [
        AnsiColor::Black,
        AnsiColor::Red,
        AnsiColor::Green,
        AnsiColor::Yellow,
        AnsiColor::Blue,
        AnsiColor::Magenta,
        AnsiColor::Cyan,
        AnsiColor::White,
    ];
    const BRIGHT: [AnsiColor; 8] = [
        AnsiColor::BrightBlack,
        AnsiColor::BrightRed,
        AnsiColor::BrightGreen,
        AnsiColor::BrightYellow,
        AnsiColor::BrightBlue,
        AnsiColor::BrightMagenta,
        AnsiColor::BrightCyan,
        AnsiColor::BrightWhite,
    ];
    let mut style = Style::new();
    for p in params.split(';') {
        let n: usize = p
            .trim()
            .parse()
            .map_err(|_| format!("{params:?}: не число"))?;
        style = match n {
            1 => style.bold(),
            2 => style.dimmed(),
            3 => style.italic(),
            4 => style.underline(),
            30..=37 => style.fg_color(Some(Color::Ansi(ANSI[n - 30]))),
            90..=97 => style.fg_color(Some(Color::Ansi(BRIGHT[n - 90]))),
            _ => return Err(format!("{params:?}: параметр {n} не поддерживается")),
        };
    }
    Ok(style)
}

/// Фирменный стиль: цвет `#RRGGBB` и атрибуты (`bold`, `dim`).
fn brand(hex: &str, attrs: &[String]) -> Result<Style, String> {
    let digits = hex
        .strip_prefix('#')
        .filter(|d| d.len() == 6)
        .ok_or_else(|| format!("brand {hex:?}: нужен #RRGGBB"))?;
    let byte = |i: usize| {
        u8::from_str_radix(&digits[i..i + 2], 16).map_err(|e| format!("brand {hex:?}: {e}"))
    };
    let mut effects = Effects::new();
    for a in attrs {
        effects |= match a.as_str() {
            "bold" => Effects::BOLD,
            "dim" => Effects::DIMMED,
            other => return Err(format!("attrs: {other:?} не поддерживается")),
        };
    }
    Ok(Style::new()
        .fg_color(Some(Color::Rgb(RgbColor(byte(0)?, byte(2)?, byte(4)?))))
        .effects(effects))
}

/// Режим цвета вывода.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Plain,
    Ansi16,
    TrueColor,
}

/// Оформление одного потока вывода: без цвета, 16 цветов ANSI или фирменные значения.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme(Mode);

impl Theme {
    /// Без цвета: файл, конвейер, `--color never`, `NO_COLOR`.
    pub const PLAIN: Theme = Theme(Mode::Plain);
    /// 16 цветов ANSI — оттенки задаёт палитра терминала.
    pub const ANSI16: Theme = Theme(Mode::Ansi16);
    /// Фирменные значения (`COLORTERM=truecolor` или `24bit`).
    pub const TRUECOLOR: Theme = Theme(Mode::TrueColor);

    /// Оформление stdout при выборе `choice`.
    pub fn stdout(choice: ColorChoice) -> Self {
        Self::detect(choice, &std::io::stdout())
    }

    /// Оформление stderr при выборе `choice`.
    pub fn stderr(choice: ColorChoice) -> Self {
        Self::detect(choice, &std::io::stderr())
    }

    /// `auto` решает anstream (терминал ли `raw`, `NO_COLOR`, `CLICOLOR_FORCE`, `TERM`); цвет — 16 или truecolor
    /// по `COLORTERM`.
    fn detect<S: RawStream>(choice: ColorChoice, raw: &S) -> Self {
        let choice = match choice {
            ColorChoice::Auto => AutoStream::<S>::choice(raw),
            other => other,
        };
        if choice == ColorChoice::Never {
            return Self::PLAIN;
        }
        let truecolor = std::env::var("COLORTERM")
            .is_ok_and(|v| v.eq_ignore_ascii_case("truecolor") || v.eq_ignore_ascii_case("24bit"));
        if truecolor {
            Self::TRUECOLOR
        } else {
            Self::ANSI16
        }
    }

    /// Без цвета.
    pub fn is_plain(self) -> bool {
        self.0 == Mode::Plain
    }

    /// Стиль роли (без цвета — пустой); годится и для `clap::builder::Styles`.
    pub fn style(self, role: Role) -> Style {
        let i = role as usize;
        match self.0 {
            Mode::Plain => Style::new(),
            Mode::Ansi16 => palette().ansi16[i],
            Mode::TrueColor => palette().truecolor[i],
        }
    }

    /// Текст `text` в стиле роли `role`.
    pub fn paint(self, role: Role, text: &str) -> String {
        let s = self.style(role);
        format!("{}{text}{}", s.render(), s.render_reset())
    }

    /// Знак состояния и текст в его цвете: «● норма».
    pub fn state(self, state: State, text: &str) -> String {
        self.paint(state.role(), &format!("{} {text}", state.sign()))
    }

    /// Выбор anstream для записи уже оформленного текста.
    fn choice(self) -> ColorChoice {
        if self.is_plain() {
            ColorChoice::Never
        } else {
            ColorChoice::Always
        }
    }
}

/// Данные — в stdout; ошибка записи (закрытый конвейер) не роняет процесс.
pub fn print(theme: Theme, text: &str) {
    let mut out = AutoStream::new(std::io::stdout(), theme.choice());
    let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
}

/// Ошибки и подсказки — в stderr.
pub fn eprint(theme: Theme, text: &str) {
    let mut out = AutoStream::new(std::io::stderr(), theme.choice());
    let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
}

/// Текст без последовательностей ESC (ширина столбцов, проверки).
pub fn strip(text: &str) -> String {
    anstream::adapter::strip_str(text).to_string()
}

/// Значение JSON для вывода: строка — без кавычек, `null` — «—».
pub fn value_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "—".to_owned(),
        other => other.to_string(),
    }
}

/// Таблица с шапкой `cols` (`head`); ячейки могут быть уже оформлены — ширина по видимому тексту. Строк нет —
/// «нет записей» (`muted`).
pub fn table(theme: Theme, cols: &[&str], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return format!("{}\n", theme.paint(Role::Muted, "нет записей"));
    }
    let width = |s: &str| strip(s).chars().count();
    let widths: Vec<usize> = cols
        .iter()
        .enumerate()
        .map(|(i, t)| {
            rows.iter()
                .map(|r| r.get(i).map_or(0, |c| width(c)))
                .chain([t.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: Vec<&str>| {
        let last = cells.len().saturating_sub(1);
        let mut s = cells
            .iter()
            .zip(&widths)
            .enumerate()
            .map(|(i, (c, w))| {
                let pad = if i == last {
                    0
                } else {
                    w.saturating_sub(width(c))
                };
                format!("{c}{}", " ".repeat(pad))
            })
            .collect::<Vec<_>>()
            .join("  ");
        s.truncate(s.trim_end().len());
        s
    };
    let mut out = format!("{}\n", theme.paint(Role::Head, &line(cols.to_vec())));
    for r in rows {
        out.push_str(&line(r.iter().map(String::as_str).collect()));
        out.push('\n');
    }
    out
}

/// Строка листания («есть ещё, следующая страница: …») — `muted`.
pub fn more(theme: Theme, text: &str) -> String {
    format!("{}\n", theme.paint(Role::Muted, text))
}

/// Столбцы таблицы учётных записей ([`account_row`]).
pub const ACCOUNT_COLUMNS: &[&str] = &["ИДЕНТИФИКАТОР", "ИМЯ ВХОДА", "ИМЯ", "СОСТОЯНИЕ"];

/// Учётная запись — строка таблицы [`ACCOUNT_COLUMNS`]; имени нет — «—».
pub fn account_row(theme: Theme, a: &AccountInfo) -> Vec<String> {
    let name = if a.display_name.is_empty() {
        "—".to_owned()
    } else {
        a.display_name.clone()
    };
    vec![a.id.clone(), a.login.clone(), name, account_state(theme, a)]
}

/// Состояние учётной записи: × заблокирована, ▲ квота (занято не меньше лимита), ● норма.
pub fn account_state(theme: Theme, a: &AccountInfo) -> String {
    let full = a
        .quota
        .limit_bytes
        .is_some_and(|limit| a.quota.used_bytes >= limit);
    if !a.enabled {
        theme.state(State::Err, "заблокирована")
    } else if full {
        theme.state(State::Warn, "квота")
    } else {
        theme.state(State::Ok, "норма")
    }
}

/// Отказ или ошибка для stderr: «ошибка:» (`err`); у отказа Admin API (`МЕТОД /путь: КОД …`, [`crate::Error::Api`])
/// метод и путь — `accent`, код HTTP — `err`; строки «подсказка:» — метка `head`.
pub fn error(theme: Theme, message: &str) -> String {
    let mut out = String::new();
    for (i, line) in message.lines().enumerate() {
        if i == 0 {
            out.push_str(&theme.paint(Role::Err, "ошибка:"));
            out.push(' ');
            out.push_str(&api_line(theme, line));
        } else if let Some(rest) = line.strip_prefix(HINT) {
            out.push_str(&theme.paint(Role::Head, HINT));
            out.push_str(rest);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

/// Метка подсказки в тексте ошибки.
pub(crate) const HINT: &str = "подсказка:";

/// Первая строка отказа Admin API с оформлением; другая строка — как есть.
fn api_line(theme: Theme, line: &str) -> String {
    const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];
    let parsed = line.split_once(": ").and_then(|(op, rest)| {
        let (method, path) = op.split_once(' ')?;
        let status = rest
            .get(..3)
            .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))?;
        (METHODS.contains(&method) && path.starts_with('/')).then_some((op, status, &rest[3..]))
    });
    match parsed {
        Some((op, status, rest)) => format!(
            "{}: {}{rest}",
            theme.paint(Role::Accent, op),
            theme.paint(Role::Err, status)
        ),
        None => line.to_owned(),
    }
}

/// Было/стало по изменённым полям объекта: `- поле: было` (`err`) и `+ поле: стало` (`ok`); итог — «✓ Сохранено»
/// и команда записи аудита операции `op`. Один вывод для всех `set` CLI; не заданное (`null`, записи не было) строки
/// не даёт — у новой записи все поля со знаком `+`.
pub fn changes<B: Serialize, A: Serialize>(theme: Theme, op: Op, before: &B, after: &A) -> String {
    let fields = |v: serde_json::Result<Value>| match v.unwrap_or(Value::Null) {
        Value::Object(map) => map,
        Value::Null => serde_json::Map::new(),
        other => serde_json::Map::from_iter([(String::new(), other)]),
    };
    let before = fields(serde_json::to_value(before));
    let after = fields(serde_json::to_value(after));
    let mut out = String::new();
    let names = after
        .keys()
        .chain(before.keys().filter(|k| !after.contains_key(*k)));
    for name in names {
        let was = before.get(name).unwrap_or(&Value::Null);
        let now = after.get(name).unwrap_or(&Value::Null);
        if was == now {
            continue;
        }
        let label = if name.is_empty() {
            String::new()
        } else {
            format!("{name}: ")
        };
        // Не заданное (`null`) строки не даёт: новое поле — только `+`, снятое — только `-`.
        for (value, sign, role) in [(was, '-', Role::Err), (now, '+', Role::Ok)] {
            if !value.is_null() {
                let line = format!("{sign} {label}{}", value_text(value));
                out.push_str(&format!("{}\n", theme.paint(role, &line)));
            }
        }
    }
    if out.is_empty() {
        out.push_str(&format!("{}\n", theme.paint(Role::Muted, "изменений нет")));
    }
    let audit = format!(
        "запись аудита: thepost admin audit search --method {} --route {}",
        op.method, op.path
    );
    out.push_str(&format!(
        "{} · {}\n",
        theme.state(State::Done, "Сохранено"),
        theme.paint(Role::Muted, &audit)
    ));
    out
}

/// Ответ в JSON (как `serde_json::to_string_pretty`): ключи — `json_key`, числа — `json_number`, `true` и `false` —
/// `json_bool`, `null` — `muted`. Без цвета — тот же текст байт в байт.
pub fn json<T: Serialize + ?Sized>(theme: Theme, v: &T) -> serde_json::Result<String> {
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(
        &mut buf,
        Highlight {
            pretty: PrettyFormatter::new(),
            theme,
        },
    );
    v.serialize(&mut ser)?;
    String::from_utf8(buf).map_err(|e| serde::ser::Error::custom(e.to_string()))
}

/// Форматтер serde_json: отступы — [`PrettyFormatter`], вокруг ключей и значений — стили ролей.
struct Highlight<'a> {
    pretty: PrettyFormatter<'a>,
    theme: Theme,
}

impl Highlight<'_> {
    fn styled<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        role: Role,
        body: impl FnOnce(&mut W) -> std::io::Result<()>,
    ) -> std::io::Result<()> {
        let s = self.theme.style(role);
        write!(w, "{}", s.render())?;
        body(w)?;
        write!(w, "{}", s.render_reset())
    }
}

macro_rules! numbers {
    ($($name:ident: $t:ty),* $(,)?) => {
        $(
            fn $name<W: ?Sized + std::io::Write>(&mut self, w: &mut W, v: $t) -> std::io::Result<()> {
                self.styled(w, Role::JsonNumber, |w| CompactFormatter.$name(w, v))
            }
        )*
    };
}

impl Formatter for Highlight<'_> {
    fn write_null<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.styled(w, Role::Muted, |w| w.write_all(b"null"))
    }

    fn write_bool<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        v: bool,
    ) -> std::io::Result<()> {
        let text: &[u8] = if v { b"true" } else { b"false" };
        self.styled(w, Role::JsonBool, |w| w.write_all(text))
    }

    numbers! {
        write_i8: i8, write_i16: i16, write_i32: i32, write_i64: i64, write_i128: i128,
        write_u8: u8, write_u16: u16, write_u32: u32, write_u64: u64, write_u128: u128,
        write_f32: f32, write_f64: f64,
    }

    fn write_number_str<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        v: &str,
    ) -> std::io::Result<()> {
        self.styled(w, Role::JsonNumber, |w| w.write_all(v.as_bytes()))
    }

    fn begin_array<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.pretty.begin_array(w)
    }

    fn end_array<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.pretty.end_array(w)
    }

    fn begin_array_value<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        self.pretty.begin_array_value(w, first)
    }

    fn end_array_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.pretty.end_array_value(w)
    }

    fn begin_object<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.pretty.begin_object(w)
    }

    fn end_object<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.pretty.end_object(w)
    }

    fn begin_object_key<W: ?Sized + std::io::Write>(
        &mut self,
        w: &mut W,
        first: bool,
    ) -> std::io::Result<()> {
        self.pretty.begin_object_key(w, first)?;
        write!(w, "{}", self.theme.style(Role::JsonKey).render())
    }

    fn end_object_key<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        write!(w, "{}", self.theme.style(Role::JsonKey).render_reset())?;
        self.pretty.end_object_key(w)
    }

    fn begin_object_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.pretty.begin_object_value(w)
    }

    fn end_object_value<W: ?Sized + std::io::Write>(&mut self, w: &mut W) -> std::io::Result<()> {
        self.pretty.end_object_value(w)
    }
}

#[cfg(test)]
mod tests {
    use reqwest::StatusCode;
    use serde_json::json;
    use tp_admin::accounts::PolicyBody;

    use super::*;
    use tp_contracts::transport::{DestinationPolicy, TlsRequirement};

    use crate::ops::{ACCOUNT_CREATE, ORG_POLICY_PUT, ROUTE_PUT};

    const THEMES: [(&str, Theme); 3] = [
        ("plain", Theme::PLAIN),
        ("ansi16", Theme::ANSI16),
        ("truecolor", Theme::TRUECOLOR),
    ];

    /// Снимок: ESC — видимым `^[`.
    fn show(s: &str) -> String {
        s.replace('\u{1b}', "^[")
    }

    fn account(login: &str, enabled: bool, used: u64, limit: Option<u64>) -> AccountInfo {
        serde_json::from_value(json!({
            "id": format!("id-{login}"), "domain": "b.test", "login": login, "display_name": "",
            "external_id": null, "enabled": enabled, "aliases": [], "hidden_from_gal": false,
            "quota": { "used_bytes": used, "messages": 1, "limit_bytes": limit },
            "policy": {}, "effective": {},
        }))
        .expect("AccountInfo")
    }

    fn accounts_list(t: Theme) -> String {
        let list = [
            account("a@b.test", true, 10, Some(100)),
            account("quota@b.test", true, 100, Some(100)),
            account("off@b.test", false, 0, None),
        ];
        let rows: Vec<Vec<String>> = list.iter().map(|a| account_row(t, a)).collect();
        let mut out = table(t, ACCOUNT_COLUMNS, &rows);
        out.push_str(&more(
            t,
            "Показано 3; есть ещё. Следующая страница: --after off@b.test",
        ));
        out
    }

    fn conflict(t: Theme) -> String {
        let body =
            r#"{"status":409,"title":"Conflict","detail":"login a@b.test занят"}"#.as_bytes();
        let e = crate::problem(ACCOUNT_CREATE, StatusCode::CONFLICT, body)
            .hint(409, "thepost admin account get a@b.test");
        error(t, &e.to_string())
    }

    fn policy_set(t: Theme) -> String {
        let before: PolicyBody =
            serde_json::from_value(json!({"quota_bytes": 1000, "send_per_hour": 5})).expect("до");
        let after: PolicyBody = serde_json::from_value(
            json!({"quota_bytes": 2000, "send_per_hour": 5, "dkim_selector": "s1"}),
        )
        .expect("после");
        changes(t, ORG_POLICY_PUT, &before, &after)
    }

    /// `transport route set`: новая политика и изменение существующей (только изменённое поле).
    fn route_set(t: Theme) -> String {
        let policy = |tls, egress: Option<&str>| DestinationPolicy {
            pattern: "example.org".into(),
            tls,
            smart_host: Some("relay.example.net:587".into()),
            egress_ip: egress.map(|ip| ip.parse().expect("ip")),
        };
        let new = policy(TlsRequirement::Required, None);
        let mut out = changes(t, ROUTE_PUT, &None::<DestinationPolicy>, &Some(new.clone()));
        let changed = policy(TlsRequirement::Required, Some("192.0.2.1"));
        out.push_str(&changes(t, ROUTE_PUT, &Some(new), &Some(changed)));
        out
    }

    fn cases(t: Theme) -> [(&'static str, String); 5] {
        [
            ("accounts", accounts_list(t)),
            ("conflict", conflict(t)),
            ("policy", policy_set(t)),
            ("json", json(t, &sample()).expect("json")),
            ("route", route_set(t)),
        ]
    }

    fn sample() -> Value {
        json!({"login": "a@b.test", "enabled": true, "quota": {"used": 10, "ratio": 0.5}, "aliases": [], "id": null})
    }

    /// Снимки вывода (ESC — `^[`): список учётных записей, отказ 409, `policy set`, `--json`, `route set` в трёх
    /// режимах.
    const SNAPSHOTS: &[(&str, &str, &str)] = &[
        (
            "plain",
            "accounts",
            r#"ИДЕНТИФИКАТОР    ИМЯ ВХОДА     ИМЯ  СОСТОЯНИЕ
id-a@b.test      a@b.test      —    ● норма
id-quota@b.test  quota@b.test  —    ▲ квота
id-off@b.test    off@b.test    —    × заблокирована
Показано 3; есть ещё. Следующая страница: --after off@b.test
"#,
        ),
        (
            "plain",
            "conflict",
            r#"ошибка: POST /api/v1/orgs/{org}/accounts: 409 Conflict: login a@b.test занят
подсказка: thepost admin account get a@b.test
"#,
        ),
        (
            "plain",
            "policy",
            r#"+ dkim_selector: s1
- quota_bytes: 1000
+ quota_bytes: 2000
✓ Сохранено · запись аудита: thepost admin audit search --method PUT --route /api/v1/orgs/{org}/policy
"#,
        ),
        (
            "plain",
            "json",
            r#"{
  "aliases": [],
  "enabled": true,
  "id": null,
  "login": "a@b.test",
  "quota": {
    "ratio": 0.5,
    "used": 10
  }
}"#,
        ),
        (
            "plain",
            "route",
            r#"+ pattern: example.org
+ smart_host: relay.example.net:587
+ tls: required
✓ Сохранено · запись аудита: thepost admin audit search --method PUT --route /api/v1/orgs/{org}/transport/routes/{pattern}
+ egress_ip: 192.0.2.1
✓ Сохранено · запись аудита: thepost admin audit search --method PUT --route /api/v1/orgs/{org}/transport/routes/{pattern}
"#,
        ),
        (
            "ansi16",
            "accounts",
            r#"^[[1m^[[94mИДЕНТИФИКАТОР    ИМЯ ВХОДА     ИМЯ  СОСТОЯНИЕ^[[0m
id-a@b.test      a@b.test      —    ^[[92m● норма^[[0m
id-quota@b.test  quota@b.test  —    ^[[1m^[[33m▲ квота^[[0m
id-off@b.test    off@b.test    —    ^[[1m^[[91m× заблокирована^[[0m
^[[2mПоказано 3; есть ещё. Следующая страница: --after off@b.test^[[0m
"#,
        ),
        (
            "ansi16",
            "conflict",
            r#"^[[1m^[[91mошибка:^[[0m ^[[95mPOST /api/v1/orgs/{org}/accounts^[[0m: ^[[1m^[[91m409^[[0m Conflict: login a@b.test занят
^[[1m^[[94mподсказка:^[[0m thepost admin account get a@b.test
"#,
        ),
        (
            "ansi16",
            "policy",
            r#"^[[92m+ dkim_selector: s1^[[0m
^[[1m^[[91m- quota_bytes: 1000^[[0m
^[[92m+ quota_bytes: 2000^[[0m
^[[92m✓ Сохранено^[[0m · ^[[2mзапись аудита: thepost admin audit search --method PUT --route /api/v1/orgs/{org}/policy^[[0m
"#,
        ),
        (
            "ansi16",
            "json",
            r#"{
  ^[[94m"aliases"^[[0m: [],
  ^[[94m"enabled"^[[0m: ^[[95mtrue^[[0m,
  ^[[94m"id"^[[0m: ^[[2mnull^[[0m,
  ^[[94m"login"^[[0m: "a@b.test",
  ^[[94m"quota"^[[0m: {
    ^[[94m"ratio"^[[0m: ^[[96m0.5^[[0m,
    ^[[94m"used"^[[0m: ^[[96m10^[[0m
  }
}"#,
        ),
        (
            "ansi16",
            "route",
            r#"^[[92m+ pattern: example.org^[[0m
^[[92m+ smart_host: relay.example.net:587^[[0m
^[[92m+ tls: required^[[0m
^[[92m✓ Сохранено^[[0m · ^[[2mзапись аудита: thepost admin audit search --method PUT --route /api/v1/orgs/{org}/transport/routes/{pattern}^[[0m
^[[92m+ egress_ip: 192.0.2.1^[[0m
^[[92m✓ Сохранено^[[0m · ^[[2mзапись аудита: thepost admin audit search --method PUT --route /api/v1/orgs/{org}/transport/routes/{pattern}^[[0m
"#,
        ),
        (
            "truecolor",
            "accounts",
            r#"^[[1m^[[38;2;42;139;196mИДЕНТИФИКАТОР    ИМЯ ВХОДА     ИМЯ  СОСТОЯНИЕ^[[0m
id-a@b.test      a@b.test      —    ^[[38;2;26;150;148m● норма^[[0m
id-quota@b.test  quota@b.test  —    ^[[1m^[[38;2;176;125;0m▲ квота^[[0m
id-off@b.test    off@b.test    —    ^[[1m^[[38;2;229;72;77m× заблокирована^[[0m
^[[2m^[[38;2;126;138;150mПоказано 3; есть ещё. Следующая страница: --after off@b.test^[[0m
"#,
        ),
        (
            "truecolor",
            "conflict",
            r#"^[[1m^[[38;2;229;72;77mошибка:^[[0m ^[[38;2;228;16;127mPOST /api/v1/orgs/{org}/accounts^[[0m: ^[[1m^[[38;2;229;72;77m409^[[0m Conflict: login a@b.test занят
^[[1m^[[38;2;42;139;196mподсказка:^[[0m thepost admin account get a@b.test
"#,
        ),
        (
            "truecolor",
            "policy",
            r#"^[[38;2;26;150;148m+ dkim_selector: s1^[[0m
^[[1m^[[38;2;229;72;77m- quota_bytes: 1000^[[0m
^[[38;2;26;150;148m+ quota_bytes: 2000^[[0m
^[[38;2;26;150;148m✓ Сохранено^[[0m · ^[[2m^[[38;2;126;138;150mзапись аудита: thepost admin audit search --method PUT --route /api/v1/orgs/{org}/policy^[[0m
"#,
        ),
        (
            "truecolor",
            "json",
            r#"{
  ^[[38;2;42;139;196m"aliases"^[[0m: [],
  ^[[38;2;42;139;196m"enabled"^[[0m: ^[[38;2;228;16;127mtrue^[[0m,
  ^[[38;2;42;139;196m"id"^[[0m: ^[[2m^[[38;2;126;138;150mnull^[[0m,
  ^[[38;2;42;139;196m"login"^[[0m: "a@b.test",
  ^[[38;2;42;139;196m"quota"^[[0m: {
    ^[[38;2;42;139;196m"ratio"^[[0m: ^[[38;2;26;150;148m0.5^[[0m,
    ^[[38;2;42;139;196m"used"^[[0m: ^[[38;2;26;150;148m10^[[0m
  }
}"#,
        ),
        (
            "truecolor",
            "route",
            r#"^[[38;2;26;150;148m+ pattern: example.org^[[0m
^[[38;2;26;150;148m+ smart_host: relay.example.net:587^[[0m
^[[38;2;26;150;148m+ tls: required^[[0m
^[[38;2;26;150;148m✓ Сохранено^[[0m · ^[[2m^[[38;2;126;138;150mзапись аудита: thepost admin audit search --method PUT --route /api/v1/orgs/{org}/transport/routes/{pattern}^[[0m
^[[38;2;26;150;148m+ egress_ip: 192.0.2.1^[[0m
^[[38;2;26;150;148m✓ Сохранено^[[0m · ^[[2m^[[38;2;126;138;150mзапись аудита: thepost admin audit search --method PUT --route /api/v1/orgs/{org}/transport/routes/{pattern}^[[0m
"#,
        ),
    ];

    /// Снимки: без цвета, 16 цветов и truecolor.
    #[test]
    fn snapshots() {
        for (name, t) in THEMES {
            for (case, out) in cases(t) {
                let want = SNAPSHOTS
                    .iter()
                    .find(|(n, c, _)| *n == name && *c == case)
                    .map(|(_, _, s)| *s)
                    .expect("снимок");
                assert_eq!(show(&out), want, "{name}/{case}");
            }
        }
    }

    /// Токены `[cli]` разбираются; роли — по решению 120; знаки — из `[signs]`.
    #[test]
    fn tokens_parse() {
        let p = load(TOKENS).expect("design/tokens.toml");
        let head = p.ansi16[Role::Head as usize];
        assert_eq!(
            head,
            Style::new()
                .bold()
                .fg_color(Some(Color::Ansi(AnsiColor::BrightBlue)))
        );
        assert_eq!(
            p.truecolor[Role::Ok as usize].get_fg_color(),
            Some(Color::Rgb(RgbColor(0x1A, 0x96, 0x94)))
        );
        assert_eq!(
            [State::Done, State::Ok, State::Warn, State::Err].map(State::sign),
            ["✓", "●", "▲", "×"]
        );
        assert!(sgr("1;38").is_err() && sgr("x").is_err());
        assert!(brand("#12345", &[]).is_err() && brand("#123456", &["blink".into()]).is_err());
    }

    /// `--json`: без ESC — тот же текст, что `serde_json::to_string_pretty`, байт в байт, в каждом режиме.
    #[test]
    fn json_strips_to_plain() {
        let v = sample();
        let plain = serde_json::to_string_pretty(&v).expect("pretty");
        for (name, t) in THEMES {
            let out = json(t, &v).expect("json");
            assert_eq!(strip(&out), plain, "{name}");
            assert_eq!(t.is_plain(), out == plain, "{name}");
        }
        assert_eq!(json(Theme::PLAIN, &()).expect("unit"), "null");
    }

    /// Ширина столбцов — по видимому тексту: оформленная ячейка не сдвигает таблицу.
    #[test]
    fn table_width_ignores_esc() {
        let rows = |t: Theme| vec![vec![t.state(State::Ok, "норма"), "x".to_owned()]];
        for (name, t) in THEMES {
            assert_eq!(
                strip(&table(t, &["А", "Б"], &rows(t))),
                "А        Б\n● норма  x\n",
                "{name}"
            );
        }
        assert_eq!(table(Theme::PLAIN, &["А"], &[]), "нет записей\n");
    }

    /// Ошибка не Admin API — «ошибка:» и текст как есть.
    #[test]
    fn client_error() {
        assert_eq!(
            show(&error(
                Theme::ANSI16,
                "нет токена: задайте THEPOST_ADMIN_TOKEN"
            )),
            "^[[1m^[[91mошибка:^[[0m нет токена: задайте THEPOST_ADMIN_TOKEN\n"
        );
    }

    /// Дочерний процесс для [`stream_choice`]: вывод `print` между метками; `TP_STYLE_CHOICE` — `--color`.
    #[test]
    #[ignore = "запускается из stream_choice"]
    fn child() {
        let Ok(choice) = std::env::var("TP_STYLE_CHOICE") else {
            return;
        };
        let choice = match choice.as_str() {
            "always" => ColorChoice::Always,
            "never" => ColorChoice::Never,
            _ => ColorChoice::Auto,
        };
        let t = Theme::stdout(choice);
        let [(_, accounts), _, _, (_, json), _] = cases(t);
        print(t, &format!("<<<{accounts}{json}>>>"));
    }

    /// Выбор цвета по потоку и переменным: конвейер при auto — без ESC, `NO_COLOR` сильнее `CLICOLOR_FORCE`,
    /// `CLICOLOR_FORCE` и `--color always` красят и в конвейер, `COLORTERM=truecolor` — фирменные значения.
    #[test]
    fn stream_choice() {
        let run = |choice: &str, env: &[(&str, &str)]| {
            let mut cmd = std::process::Command::new(std::env::current_exe().expect("exe"));
            cmd.args(["--exact", "style::tests::child", "--ignored", "--nocapture"])
                .env("TP_STYLE_CHOICE", choice);
            for var in ["NO_COLOR", "CLICOLOR", "CLICOLOR_FORCE", "COLORTERM"] {
                cmd.env_remove(var);
            }
            cmd.envs(env.iter().copied());
            let out = cmd.output().expect("дочерний процесс");
            let stdout = String::from_utf8(out.stdout).expect("utf-8");
            let (_, rest) = stdout.split_once("<<<").expect("начало вывода");
            let (body, _) = rest.split_once(">>>").expect("конец вывода");
            body.to_owned()
        };
        let snapshot = |t: Theme| {
            let [(_, accounts), _, _, (_, json), _] = cases(t);
            format!("{accounts}{json}")
        };
        let plain = snapshot(Theme::PLAIN);
        let piped = run("auto", &[]);
        assert!(!piped.contains('\u{1b}'), "{piped}");
        assert_eq!(piped, plain);
        let json_start = piped.find('{').expect("JSON");
        serde_json::from_str::<Value>(&piped[json_start..]).expect("чистый JSON без остатка");
        assert_eq!(
            run("auto", &[("NO_COLOR", "1"), ("CLICOLOR_FORCE", "1")]),
            plain
        );
        assert_eq!(run("never", &[("CLICOLOR_FORCE", "1")]), plain);
        assert_eq!(
            run("auto", &[("CLICOLOR_FORCE", "1")]),
            snapshot(Theme::ANSI16)
        );
        assert_eq!(run("always", &[]), snapshot(Theme::ANSI16));
        assert_eq!(
            run("always", &[("COLORTERM", "truecolor")]),
            snapshot(Theme::TRUECOLOR)
        );
        assert_eq!(
            run("always", &[("COLORTERM", "24bit")]),
            snapshot(Theme::TRUECOLOR)
        );
    }
}
