//! Тема экрана по токенам `design/tokens.toml` (TUI-06, решение 119): пять режимов, роли как у веб-консоли,
//! градиент строки меню, знак бренда на экране входа и палитра консоли Linux.
//!
//! Цвета берутся только из токенов (`include_str!`), второго списка в коде нет. Бренд — набор [`Brand`]
//! «название, знак (PNG), основной цвет, акцент» (решение 122): IT TASK — набор по умолчанию, его роли
//! совпадают с токенами; набор заказчика (из Admin API) перекрашивает роли бренда и доводит их до контраста
//! по правилам `[[contrast]]` тех же токенов.

use std::fmt;
use std::io::{self, Write};
use std::sync::{Arc, OnceLock};

use ratatui::buffer::Buffer;
use ratatui::layout::{Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Widget;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};

/// Токены фирменного стиля — единственный источник цветов.
const TOKENS: &str = include_str!("../../../design/tokens.toml");
/// Знак IT TASK — куб из `design/logo.svg` (грани в цветах бренда), PNG с прозрачным фоном.
const IT_TASK_MARK: &[u8] = include_bytes!("../assets/it-task.png");
/// Переменная окружения с режимом темы: перекрывает автоопределение, ключ `--theme` перекрывает её.
pub const ENV_THEME: &str = "THEPOST_TUI_THEME";
/// Высота знака на экране входа, строк.
pub const MARK_ROWS: u16 = 4;
/// Наибольшая ширина знака на экране входа, колонок.
pub const MARK_COLS: u16 = 8;
/// Уровни каналов куба xterm-256.
const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

/// Роль ячейки экрана (как в `design/tokens.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Фон экрана.
    Screen,
    /// Фон панели.
    Panel,
    /// Текст.
    Text,
    /// Вторичный текст.
    Muted,
    /// Рамка неактивной панели.
    Frame,
    /// Рамка активной панели.
    FrameA,
    /// Заголовок активной панели.
    TitleA,
    /// Заголовки колонок.
    Head,
    /// Выделенная строка активной панели.
    Sel,
    /// Выделенная строка неактивной панели.
    SelI,
    /// Номер F-клавиши.
    FkNum,
    /// Подпись F-клавиши.
    Fk,
    /// Строка меню, сплошная (градиент — [`Theme::menu`]).
    Menu,
    /// Знак `$` строки команды.
    Prompt,
    /// Команда CLI.
    Cmd,
    /// Название бренда на экране входа: белое или чёрное по фону режима.
    Name,
    /// Норма ●.
    Ok,
    /// Внимание ▲.
    Warn,
    /// Ошибка ×.
    Err,
}

/// Режим цвета.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Truecolor, тёмный фон.
    Dark,
    /// Truecolor, светлый фон.
    Light,
    /// 256 цветов xterm.
    C256,
    /// 16 цветов терминала.
    C16,
    /// Без цвета: атрибуты.
    Mono,
}

impl Mode {
    /// Режим по имени: `dark`, `light`, `256`, `16`, `mono`.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "256" => Some(Self::C256),
            "16" => Some(Self::C16),
            "mono" => Some(Self::Mono),
            _ => None,
        }
    }

    /// Режим по ключу `--theme` (`flag`), `THEPOST_TUI_THEME` и окружению терминала (`var` читает
    /// переменную). `auto` или ничего — автоопределение: `NO_COLOR` → монохром; `COLORTERM=truecolor|24bit`
    /// или Windows Terminal (`WT_SESSION`) → truecolor тёмный; `TERM` с `256color` → 256; `TERM=dumb` →
    /// монохром; прочее, и `TERM=linux`, → 16.
    pub fn detect(
        var: impl Fn(&str) -> Option<String>,
        flag: Option<&str>,
    ) -> Result<Self, String> {
        let set = |name: &str| var(name).filter(|v| !v.is_empty());
        let choice = flag.map(str::to_owned).or_else(|| set(ENV_THEME));
        if let Some(c) = choice.filter(|c| c.trim() != "auto") {
            return Self::parse(&c)
                .ok_or_else(|| format!("тема «{c}»: нужно auto, dark, light, 256, 16 или mono"));
        }
        if set("NO_COLOR").is_some() {
            return Ok(Self::Mono);
        }
        let colorterm = set("COLORTERM").unwrap_or_default().to_ascii_lowercase();
        if colorterm == "truecolor" || colorterm == "24bit" || set("WT_SESSION").is_some() {
            return Ok(Self::Dark);
        }
        let term = set("TERM").unwrap_or_default();
        Ok(if term.contains("256color") {
            Self::C256
        } else if term == "dumb" {
            Self::Mono
        } else {
            Self::C16
        })
    }

    /// Показывается ли знак картинкой: протоколом графики терминала или цветными полублоками.
    pub fn graphics(self) -> bool {
        matches!(self, Self::Dark | Self::Light | Self::C256)
    }
}

/// Цвет `#RRGGBB`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    const WHITE: Self = Self(255, 255, 255);
    const BLACK: Self = Self(0, 0, 0);

    /// Цвет из `#RRGGBB`.
    pub fn parse(s: &str) -> Option<Self> {
        let h = s.strip_prefix('#')?;
        if h.len() != 6 || !h.is_ascii() {
            return None;
        }
        let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).ok();
        Some(Self(byte(0)?, byte(2)?, byte(4)?))
    }

    /// Относительная яркость (WCAG 2.1).
    fn luminance(self) -> f64 {
        let lin = |c: u8| {
            let c = f64::from(c) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * lin(self.0) + 0.7152 * lin(self.1) + 0.0722 * lin(self.2)
    }

    /// Контраст двух цветов (WCAG 2.1), от 1 до 21.
    pub fn contrast(self, other: Self) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Смесь: `t` = 0 — этот цвет, 1 — `other`.
    fn mix(self, other: Self, t: f64) -> Self {
        let m = |a: u8, b: u8| (f64::from(a) + (f64::from(b) - f64::from(a)) * t).round() as u8;
        Self(m(self.0, other.0), m(self.1, other.1), m(self.2, other.2))
    }

    fn color(self) -> Color {
        Color::Rgb(self.0, self.1, self.2)
    }

    /// Ближайший цвет xterm-256: куб 6×6×6 (ближайший уровень по каждому каналу) или серая шкала 232–255.
    pub fn xterm256(self) -> u8 {
        let near = |v: u8| {
            (0..6u8)
                .min_by_key(|&i| LEVELS[usize::from(i)].abs_diff(v))
                .unwrap_or_default()
        };
        let dist = |i: u8| {
            let c = xterm_rgb(i);
            [(c.0, self.0), (c.1, self.1), (c.2, self.2)]
                .iter()
                .map(|&(a, b)| u32::from(a.abs_diff(b)).pow(2))
                .sum::<u32>()
        };
        let cube = 16 + 36 * near(self.0) + 6 * near(self.1) + near(self.2);
        (232..=255u8).fold(cube, |best, g| if dist(g) < dist(best) { g } else { best })
    }
}

/// Цвет xterm-256 с номером `i` (куб и серая шкала; 0–15 — по палитре `[palette16]`).
fn xterm_rgb(i: u8) -> Rgb {
    match i {
        0..=15 => tokens().palette16[usize::from(i)],
        16..=231 => {
            let n = usize::from(i - 16);
            Rgb(LEVELS[n / 36], LEVELS[n / 6 % 6], LEVELS[n % 6])
        }
        _ => {
            let g = 8 + 10 * (i - 232);
            Rgb(g, g, g)
        }
    }
}

impl fmt::Display for Rgb {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "#{:02X}{:02X}{:02X}", self.0, self.1, self.2)
    }
}

/// PNG знака бренда.
#[derive(Clone, PartialEq, Eq)]
pub struct Png(pub Arc<[u8]>);

impl fmt::Debug for Png {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Png({} байт)", self.0.len())
    }
}

/// Бренд, из которого строятся роли (решение 122): название, знак, основной цвет и акцент.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brand {
    /// Название на экране входа и в строке меню.
    pub name: String,
    /// Знак для экрана входа; нет — только название.
    pub mark: Option<Png>,
    /// Основной цвет: F-клавиши, рамка активной панели, конец градиента меню.
    pub primary: Rgb,
    /// Акцент: выделение, главное действие, начало градиента меню.
    pub accent: Rgb,
}

impl Default for Brand {
    /// IT TASK: цвета `[brand]` токенов и знак из `design/logo.svg`.
    fn default() -> Self {
        let t = tokens();
        Self {
            name: "IT TASK".to_owned(),
            mark: Some(Png(Arc::from(IT_TASK_MARK))),
            primary: t.blue,
            accent: t.pink,
        }
    }
}

/// Роли токенов — столбцы таблиц `[mode.*]`, в порядке [`NAMES`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum T {
    Screen,
    Panel,
    Text,
    Muted,
    Frame,
    FrameA,
    TitleA,
    Head,
    Sel,
    SelFg,
    SelI,
    SelIFg,
    FkBg,
    FkFg,
    Ok,
    Warn,
    Err,
}

const N: usize = 17;
const NAMES: [&str; N] = [
    "screen", "panel", "text", "muted", "frame", "frameA", "titleA", "head", "sel", "selFg",
    "selI", "selIFg", "fkBg", "fkFg", "ok", "warn", "err",
];

/// Правило `[[contrast]]`: роль `fg` на каждом из фонов `bg` — не ниже `min`.
#[derive(Debug)]
struct Rule {
    fg: usize,
    bg: Vec<usize>,
    min: f64,
}

/// Разобранные токены.
#[derive(Debug)]
struct Tokens {
    pink: Rgb,
    blue: Rgb,
    dark: [Rgb; N],
    light: [Rgb; N],
    c256: [u8; N],
    c16: [u8; N],
    mono: [Modifier; N],
    attrs: [Modifier; N],
    palette16: [Rgb; 16],
    contrast: Vec<Rule>,
}

fn tokens() -> &'static Tokens {
    static TOK: OnceLock<Tokens> = OnceLock::new();
    TOK.get_or_init(|| parse_tokens(TOKENS).unwrap_or_else(|e| panic!("design/tokens.toml: {e}")))
}

fn parse_tokens(src: &str) -> Result<Tokens, String> {
    let root: toml::Table = src.parse().map_err(|e| format!("{e}"))?;
    let table = |path: &str| -> Result<&toml::Table, String> {
        path.split('.')
            .try_fold(&root, |t, k| t.get(k).and_then(toml::Value::as_table))
            .ok_or_else(|| format!("нет таблицы [{path}]"))
    };
    let rgb = |v: Option<&toml::Value>, at: &str| {
        v.and_then(toml::Value::as_str)
            .and_then(Rgb::parse)
            .ok_or_else(|| format!("{at}: нужен цвет #RRGGBB"))
    };
    let attrs = |v: Option<&toml::Value>, at: &str| -> Result<Modifier, String> {
        let list = v
            .and_then(toml::Value::as_array)
            .ok_or_else(|| format!("{at}: нужен список атрибутов"))?;
        list.iter().try_fold(Modifier::empty(), |m, a| {
            Ok(m | match a.as_str() {
                Some("bold") => Modifier::BOLD,
                Some("dim") => Modifier::DIM,
                Some("reverse") => Modifier::REVERSED,
                Some("underline") => Modifier::UNDERLINED,
                _ => return Err(format!("{at}: неизвестный атрибут {a:?}")),
            })
        })
    };
    let colors = |path: &str| -> Result<[Rgb; N], String> {
        let t = table(path)?;
        let mut out = [Rgb::BLACK; N];
        for (c, name) in out.iter_mut().zip(NAMES) {
            *c = rgb(t.get(name), &format!("{path}.{name}"))?;
        }
        Ok(out)
    };
    let index = |path: &str, max: u8| -> Result<[u8; N], String> {
        let t = table(path)?;
        let mut out = [0; N];
        for (c, name) in out.iter_mut().zip(NAMES) {
            *c = t
                .get(name)
                .and_then(toml::Value::as_integer)
                .and_then(|n| u8::try_from(n).ok())
                .filter(|n| *n <= max)
                .ok_or_else(|| format!("{path}.{name}: нужен номер 0–{max}"))?;
        }
        Ok(out)
    };
    let (mono_t, attrs_t) = (table("mode.mono")?, table("attrs")?);
    let mut mono = [Modifier::empty(); N];
    let mut role_attrs = [Modifier::empty(); N];
    for ((m, a), name) in mono.iter_mut().zip(role_attrs.iter_mut()).zip(NAMES) {
        *m = attrs(mono_t.get(name), &format!("mode.mono.{name}"))?;
        if let Some(v) = attrs_t.get(name) {
            *a = attrs(Some(v), &format!("attrs.{name}"))?;
        }
    }
    let pal = table("palette16")?
        .get("colors")
        .and_then(toml::Value::as_array)
        .filter(|a| a.len() == 16)
        .ok_or("palette16.colors: нужно 16 цветов")?;
    let mut palette16 = [Rgb::BLACK; 16];
    for (i, (p, c)) in palette16.iter_mut().zip(pal).enumerate() {
        *p = rgb(Some(c), &format!("palette16.colors[{i}]"))?;
    }
    let role = |v: &toml::Value| {
        v.as_str()
            .and_then(|s| NAMES.iter().position(|n| *n == s))
            .ok_or_else(|| format!("contrast: неизвестная роль {v:?}"))
    };
    let mut contrast = Vec::new();
    for r in root
        .get("contrast")
        .and_then(toml::Value::as_array)
        .ok_or("нет [[contrast]]")?
    {
        let fg = role(r.get("fg").ok_or("contrast: нет fg")?)?;
        let bg = r
            .get("bg")
            .and_then(toml::Value::as_array)
            .ok_or("contrast: нет bg")?
            .iter()
            .map(role)
            .collect::<Result<_, _>>()?;
        let min = r
            .get("min")
            .and_then(toml::Value::as_float)
            .ok_or("contrast: нет min")?;
        contrast.push(Rule { fg, bg, min });
    }
    let brand = table("brand")?;
    Ok(Tokens {
        pink: rgb(brand.get("pink"), "brand.pink")?,
        blue: rgb(brand.get("blue"), "brand.blue")?,
        dark: colors("mode.dark")?,
        light: colors("mode.light")?,
        c256: index("mode.c256", 255)?,
        c16: index("mode.c16", 15)?,
        mono,
        attrs: role_attrs,
        palette16,
        contrast,
    })
}

/// Роли truecolor-палитры `base` под бренд и признак «роль — цвет бренда». Набор с цветами `[brand]` (IT TASK)
/// ничего не меняет. Иначе роли, равные в токенах основному цвету или акценту, получают цвет бренда, рамка
/// активной панели следует основному, заголовок и выделение — акценту; затем по `[[contrast]]` перекрашенные текст и рамки
/// смешиваются с белым (тёмный фон) или чёрным (светлый) до нужного контраста, а текст на фоне бренда
/// становится белым или чёрным.
fn branded(base: &[Rgb; N], brand: &Brand) -> ([Rgb; N], [bool; N]) {
    let t = tokens();
    let mut c = *base;
    let mut touched = [false; N];
    for (from, to, derived) in [
        (t.blue, brand.primary, &[T::FrameA][..]),
        (t.pink, brand.accent, &[T::TitleA, T::Sel][..]),
    ] {
        if from == to {
            continue;
        }
        for ((c, touched), b) in c.iter_mut().zip(touched.iter_mut()).zip(base) {
            if *b == from {
                (*c, *touched) = (to, true);
            }
        }
        for &r in derived {
            (c[r as usize], touched[r as usize]) = (to, true);
        }
    }
    let toward = if base[T::Panel as usize].luminance() < 0.5 {
        Rgb::WHITE
    } else {
        Rgb::BLACK
    };
    let meets = |c: &[Rgb; N], fg: Rgb, r: &Rule| r.bg.iter().all(|&b| fg.contrast(c[b]) >= r.min);
    for r in t.contrast.iter().filter(|r| touched[r.fg]) {
        let fg = c[r.fg];
        c[r.fg] = (0..=20)
            .map(|k| fg.mix(toward, f64::from(k) / 20.0))
            .find(|&x| meets(&c, x, r))
            .unwrap_or(toward);
    }
    for r in &t.contrast {
        if !touched[r.fg] && r.bg.iter().any(|&b| touched[b]) && !meets(&c, c[r.fg], r) {
            let bg = c[r.bg[0]];
            c[r.fg] = if Rgb::WHITE.contrast(bg) >= Rgb::BLACK.contrast(bg) {
                Rgb::WHITE
            } else {
                Rgb::BLACK
            };
        }
    }
    (c, touched)
}

/// Знак бренда, подготовленный для терминала (протокол графики или полублоки).
#[derive(Clone)]
pub struct Mark(Arc<Protocol>);

impl Mark {
    /// Размер знака в ячейках.
    pub fn size(&self) -> Size {
        self.0.size()
    }
}

impl fmt::Debug for Mark {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = self.size();
        write!(f, "Mark({}×{})", s.width, s.height)
    }
}

impl PartialEq for Mark {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

/// Тема: режим, роли бренда и знак.
#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    mode: Mode,
    brand: Brand,
    colors: [Color; N],
    attrs: [Modifier; N],
    /// Фон панели в RGB: подложка знака.
    panel: Rgb,
    /// 16 цветов консоли Linux под бренд.
    palette: [Rgb; 16],
    mark: Option<Mark>,
}

impl Theme {
    /// Тема режима `mode` для бренда `brand`; знак — [`Theme::with_mark`].
    pub fn new(mode: Mode, brand: Brand) -> Self {
        let t = tokens();
        let (dark, touched) = branded(&t.dark, &brand);
        let light = branded(&t.light, &brand).0;
        let mut palette = t.palette16;
        for i in (0..N).filter(|&i| touched[i]) {
            palette[usize::from(t.c16[i])] = dark[i];
        }
        let c256: [u8; N] = std::array::from_fn(|i| {
            if touched[i] {
                dark[i].xterm256()
            } else {
                t.c256[i]
            }
        });
        let p = T::Panel as usize;
        let (colors, panel) = match mode {
            Mode::Dark => (dark.map(Rgb::color), dark[p]),
            Mode::Light => (light.map(Rgb::color), light[p]),
            Mode::C256 => (c256.map(Color::Indexed), xterm_rgb(c256[p])),
            Mode::C16 => (t.c16.map(ansi16), palette[usize::from(t.c16[p])]),
            Mode::Mono => ([Color::Reset; N], Rgb::BLACK),
        };
        Self {
            mode,
            brand,
            colors,
            attrs: if mode == Mode::Mono { t.mono } else { t.attrs },
            panel,
            palette,
            mark: None,
        }
    }

    /// Тема по окружению ([`Mode::detect`]) и ключу `--theme` (`flag`), бренд IT TASK.
    pub fn from_env(flag: Option<&str>) -> Result<Self, String> {
        let mode = Mode::detect(|k| std::env::var(k).ok(), flag)?;
        Ok(Self::new(mode, Brand::default()))
    }

    /// Режим.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Бренд.
    pub fn brand(&self) -> &Brand {
        &self.brand
    }

    /// 16 цветов консоли Linux: `[palette16]`, у бренда заказчика — его цвета в слотах ролей бренда.
    pub fn console_palette(&self) -> &[Rgb; 16] {
        &self.palette
    }

    /// Подготовить знак бренда протоколом `picker`. В 16 цветах и монохроме знака нет — только название;
    /// прозрачный фон знака — цвет панели.
    pub fn with_mark(mut self, picker: &Picker) -> Self {
        self.mark = self.load_mark(picker);
        self
    }

    fn load_mark(&self, picker: &Picker) -> Option<Mark> {
        if !self.mode.graphics() {
            return None;
        }
        let png = self.brand.mark.as_ref()?;
        let img = image::load_from_memory_with_format(&png.0, image::ImageFormat::Png).ok()?;
        let bg = image::Rgba([self.panel.0, self.panel.1, self.panel.2, 255]);
        let mut flat = image::RgbaImage::from_pixel(img.width(), img.height(), bg);
        image::imageops::overlay(&mut flat, &img.to_rgba8(), 0, 0);
        let protocol = picker
            .new_protocol(
                image::DynamicImage::ImageRgba8(flat),
                Size::new(MARK_COLS, MARK_ROWS),
                Resize::Fit(None),
            )
            .ok()?;
        Some(Mark(Arc::new(protocol)))
    }

    /// Знак бренда, если терминал его покажет.
    pub fn mark(&self) -> Option<&Mark> {
        self.mark.as_ref()
    }

    /// Нарисовать знак в `area`; в 256 цветах полублоки переводятся в ближайшие цвета xterm-256.
    pub fn render_mark(&self, mark: &Mark, area: Rect, buf: &mut Buffer) {
        Image::new(&mark.0).render(area, buf);
        if self.mode != Mode::C256 {
            return;
        }
        let area = area.intersection(buf.area);
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let cell = &mut buf[(x, y)];
                for c in [&mut cell.fg, &mut cell.bg] {
                    if let Color::Rgb(r, g, b) = *c {
                        *c = Color::Indexed(Rgb(r, g, b).xterm256());
                    }
                }
            }
        }
    }

    fn fg(&self, r: T) -> Style {
        Style::default()
            .fg(self.colors[r as usize])
            .add_modifier(self.attrs[r as usize])
    }

    fn bg(&self, r: T) -> Style {
        Style::default()
            .bg(self.colors[r as usize])
            .add_modifier(self.attrs[r as usize])
    }

    /// Стиль роли.
    pub fn style(&self, role: Role) -> Style {
        // Жирный в цвете; в монохроме атрибуты — из `[mode.mono]`.
        let bold = if self.mode == Mode::Mono {
            Modifier::empty()
        } else {
            Modifier::BOLD
        };
        match role {
            Role::Screen => self.bg(T::Screen).patch(self.fg(T::Text)),
            Role::Panel => self.bg(T::Panel).patch(self.fg(T::Text)),
            Role::Text | Role::Cmd => self.fg(T::Text),
            Role::Muted => self.fg(T::Muted),
            Role::Frame => self.fg(T::Frame),
            Role::FrameA => self.fg(T::FrameA),
            Role::TitleA | Role::Prompt => self.fg(T::TitleA).add_modifier(bold),
            Role::Head => self.fg(T::Head).add_modifier(bold),
            Role::Sel => self.bg(T::Sel).patch(self.fg(T::SelFg)),
            Role::SelI => self.bg(T::SelI).patch(self.fg(T::SelIFg)),
            Role::FkNum => self.fg(T::Text).add_modifier(Modifier::BOLD),
            Role::Fk | Role::Menu => self.bg(T::FkBg).patch(self.fg(T::FkFg)),
            Role::Name => {
                let fg = if self.mode == Mode::Light {
                    T::Text
                } else {
                    T::SelFg
                };
                Style::default()
                    .fg(self.colors[fg as usize])
                    .add_modifier(Modifier::BOLD)
            }
            Role::Ok => self.fg(T::Ok),
            Role::Warn => self.fg(T::Warn).add_modifier(bold),
            Role::Err => self.fg(T::Err).add_modifier(bold),
        }
    }

    /// Ячейка `x` строки меню шириной `width`: градиент от акцента к основному цвету (truecolor; в 256 —
    /// ближайшие цвета), в 16 цветах и монохроме — сплошной [`Role::Menu`].
    pub fn menu(&self, x: u16, width: u16) -> Style {
        let t = f64::from(x) / f64::from(width.saturating_sub(1).max(1));
        let c = self.brand.accent.mix(self.brand.primary, t.min(1.0));
        let base = self.style(Role::Menu);
        match self.mode {
            Mode::Dark | Mode::Light => base.bg(c.color()),
            Mode::C256 => base.bg(Color::Indexed(c.xterm256())),
            Mode::C16 | Mode::Mono => base,
        }
    }
}

/// Цвет 0–15 — именованным цветом ANSI (SGR 30–37 и 90–97: их понимает и консоль Linux).
fn ansi16(i: u8) -> Color {
    [
        Color::Black,
        Color::Red,
        Color::Green,
        Color::Yellow,
        Color::Blue,
        Color::Magenta,
        Color::Cyan,
        Color::Gray,
        Color::DarkGray,
        Color::LightRed,
        Color::LightGreen,
        Color::LightYellow,
        Color::LightBlue,
        Color::LightMagenta,
        Color::LightCyan,
        Color::White,
    ][usize::from(i & 15)]
}

/// Возврат палитры консоли Linux по умолчанию.
pub const CONSOLE_RESET: &[u8] = b"\x1b]R";

/// Нужно ли перепрограммировать палитру: консоль Linux (`TERM=linux`) в 16 цветах.
pub fn console_palette_needed(term: Option<&str>, mode: Mode) -> bool {
    term == Some("linux") && mode == Mode::C16
}

/// Палитра консоли Linux под бренд (`ESC ] P nrrggbb`) на время работы интерфейса. При сбросе — выход,
/// ошибка, `SIGTERM` (цикл завершается штатно) — выводится `ESC ] R`; при панике — из обработчика паники
/// ([`ConsolePalette::reset_on_panic`]).
#[derive(Debug)]
pub struct ConsolePalette<W: Write> {
    out: W,
}

impl<W: Write> ConsolePalette<W> {
    /// Поставить 16 цветов `colors` в `out`.
    pub fn set(mut out: W, colors: &[Rgb; 16]) -> io::Result<Self> {
        for (i, c) in colors.iter().enumerate() {
            write!(out, "\x1b]P{i:X}{:02X}{:02X}{:02X}", c.0, c.1, c.2)?;
        }
        out.flush()?;
        Ok(Self { out })
    }

    /// При панике вернуть палитру: обработчик пишет `ESC ] R` в stdout и зовёт прежний.
    pub fn reset_on_panic() {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let mut out = io::stdout();
            let _ = out.write_all(CONSOLE_RESET);
            let _ = out.flush();
            prev(info);
        }));
    }
}

impl<W: Write> Drop for ConsolePalette<W> {
    fn drop(&mut self) {
        let _ = self.out.write_all(CONSOLE_RESET);
        let _ = self.out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb(s: &str) -> Rgb {
        Rgb::parse(s).unwrap_or_else(|| panic!("{s}"))
    }

    /// Автоопределение режима по окружению; ключ и переменная перекрывают выбор.
    #[test]
    fn detect_table() {
        type Case<'a> = (&'a [(&'a str, &'a str)], Option<&'a str>, Mode);
        let cases: &[Case] = &[
            (&[], None, Mode::C16),
            (&[("TERM", "linux")], None, Mode::C16),
            (&[("TERM", "xterm")], None, Mode::C16),
            (&[("TERM", "xterm-256color")], None, Mode::C256),
            (&[("TERM", "screen-256color")], None, Mode::C256),
            (&[("TERM", "dumb")], None, Mode::Mono),
            (
                &[("TERM", "xterm-256color"), ("COLORTERM", "truecolor")],
                None,
                Mode::Dark,
            ),
            (&[("COLORTERM", "24bit")], None, Mode::Dark),
            (&[("WT_SESSION", "1")], None, Mode::Dark),
            (
                &[("COLORTERM", "truecolor"), ("NO_COLOR", "1")],
                None,
                Mode::Mono,
            ),
            (
                &[("NO_COLOR", ""), ("TERM", "xterm-256color")],
                None,
                Mode::C256,
            ),
            (&[("NO_COLOR", "1")], Some("light"), Mode::Light),
            (
                &[(ENV_THEME, "256"), ("COLORTERM", "truecolor")],
                None,
                Mode::C256,
            ),
            (&[(ENV_THEME, "mono")], Some("dark"), Mode::Dark),
            (
                &[(ENV_THEME, "auto"), ("TERM", "xterm-256color")],
                None,
                Mode::C256,
            ),
            (&[("COLORTERM", "truecolor")], Some("auto"), Mode::Dark),
            (&[], Some("16"), Mode::C16),
        ];
        for (env, flag, want) in cases {
            let var = |k: &str| {
                env.iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| (*v).to_owned())
            };
            assert_eq!(Mode::detect(var, *flag), Ok(*want), "{env:?} {flag:?}");
        }
        assert!(Mode::detect(|_| None, Some("pink")).is_err());
    }

    /// Бренд IT TASK — цвета `[brand]`, его роли совпадают с токенами.
    #[test]
    fn default_brand_is_tokens() {
        let t = tokens();
        assert_eq!(t.pink, rgb("#E4107F"));
        assert_eq!(t.blue, rgb("#177CAA"));
        let b = Brand::default();
        assert_eq!(branded(&t.dark, &b), (t.dark, [false; N]));
        assert_eq!(branded(&t.light, &b).0, t.light);
        let th = Theme::new(Mode::Dark, b.clone());
        assert_eq!(th.console_palette(), &t.palette16);
        // Выделение — #E3107F (белый на нём 4,5:1), текст жирный (решение владельца).
        let sel = th.style(Role::Sel);
        assert_eq!(sel.bg, Some(Color::Rgb(0xE3, 0x10, 0x7F)));
        assert!(sel.add_modifier.contains(Modifier::BOLD));
        assert_eq!(th.style(Role::Fk).bg, Some(Color::Rgb(0x17, 0x7C, 0xAA)));
        // 16 цветов: неактивное выделение — цвет 8, текст 15.
        let c16 = Theme::new(Mode::C16, b.clone());
        assert_eq!(c16.style(Role::SelI).bg, Some(Color::DarkGray));
        assert_eq!(c16.style(Role::SelI).fg, Some(Color::White));
        assert_eq!(
            Theme::new(Mode::C256, b).style(Role::Fk).bg,
            Some(Color::Indexed(26))
        );
    }

    /// Бренд заказчика: роли бренда перекрашены, все правила `[[contrast]]` соблюдены.
    #[test]
    fn customer_brand() {
        let t = tokens();
        let brand = Brand {
            name: "ACME".into(),
            mark: None,
            primary: rgb("#FFD400"),
            accent: rgb("#00A651"),
        };
        for base in [&t.dark, &t.light] {
            let (c, touched) = branded(base, &brand);
            for r in [T::Sel, T::FkBg, T::FrameA, T::TitleA] {
                assert!(touched[r as usize], "{r:?}");
            }
            assert!(!touched[T::Err as usize]);
            for r in &t.contrast {
                for &b in &r.bg {
                    let k = c[r.fg].contrast(c[b]);
                    assert!(k >= r.min, "{} на {}: {k:.2}", NAMES[r.fg], NAMES[b]);
                }
            }
        }
        let dark = branded(&t.dark, &brand).0;
        assert_eq!(dark[T::Sel as usize], brand.accent);
        assert_eq!(dark[T::FkBg as usize], brand.primary);
        let th = Theme::new(Mode::Dark, brand.clone());
        assert_eq!(th.menu(0, 80).bg, Some(Color::Rgb(0x00, 0xA6, 0x51)));
        assert_eq!(th.menu(79, 80).bg, Some(Color::Rgb(0xFF, 0xD4, 0x00)));
        assert_eq!(th.console_palette()[5], brand.accent);
        assert_eq!(th.console_palette()[4], brand.primary);
        assert_eq!(th.console_palette()[1], t.palette16[1]);
    }

    #[test]
    fn xterm256() {
        assert_eq!(rgb("#E4107F").xterm256(), 162);
        assert_eq!(rgb("#000000").xterm256(), 16);
        assert_eq!(rgb("#FFFFFF").xterm256(), 231);
        assert_eq!(rgb("#121212").xterm256(), 233);
        assert_eq!(xterm_rgb(26), rgb("#005FD7"));
    }

    /// Градиент меню от #E4107F к #177CAA; в 16 цветах — сплошной основной, в монохроме — инверсия.
    #[test]
    fn menu_gradient() {
        let dark = Theme::new(Mode::Dark, Brand::default());
        assert_eq!(dark.menu(0, 80).bg, Some(Color::Rgb(0xE4, 0x10, 0x7F)));
        assert_eq!(dark.menu(79, 80).bg, Some(Color::Rgb(0x17, 0x7C, 0xAA)));
        assert_ne!(dark.menu(40, 80).bg, dark.menu(0, 80).bg);
        let c256 = Theme::new(Mode::C256, Brand::default());
        assert_eq!(c256.menu(0, 80).bg, Some(Color::Indexed(162)));
        assert_eq!(
            Theme::new(Mode::C16, Brand::default()).menu(40, 80).bg,
            Some(Color::Blue)
        );
        let mono = Theme::new(Mode::Mono, Brand::default()).menu(40, 80);
        assert!(mono.add_modifier.contains(Modifier::REVERSED));
    }

    /// Палитра консоли Linux: 16 цветов `ESC ] P` при запуске, `ESC ] R` при выходе.
    #[test]
    fn console_palette() {
        assert!(console_palette_needed(Some("linux"), Mode::C16));
        assert!(!console_palette_needed(Some("xterm"), Mode::C16));
        assert!(!console_palette_needed(Some("linux"), Mode::Mono));
        let th = Theme::new(Mode::C16, Brand::default());
        let mut out = Vec::new();
        let guard = ConsolePalette::set(&mut out, th.console_palette()).expect("палитра");
        drop(guard);
        let s = String::from_utf8(out).expect("utf-8");
        assert!(s.starts_with("\x1b]P00F1317"), "{s:?}");
        assert!(s.contains("\x1b]P5E3107F"), "{s:?}");
        assert!(s.contains("\x1b]PFFFFFFF"), "{s:?}");
        assert!(s.ends_with("\x1b]R"), "{s:?}");
        assert_eq!(s.matches("\x1b]P").count(), 16);
    }

    /// Знак IT TASK: картинкой в цветных режимах; в 16 цветах, монохроме и с битым PNG — нет.
    #[test]
    fn mark() {
        let picker = Picker::halfblocks();
        for mode in [Mode::Dark, Mode::Light, Mode::C256] {
            let th = Theme::new(mode, Brand::default()).with_mark(&picker);
            let size = th.mark().map(Mark::size).expect("знак");
            assert!(
                size.width <= MARK_COLS && size.height <= MARK_ROWS,
                "{size:?}"
            );
        }
        for mode in [Mode::C16, Mode::Mono] {
            let th = Theme::new(mode, Brand::default()).with_mark(&picker);
            assert!(th.mark().is_none());
        }
        let broken = Brand {
            mark: Some(Png(Arc::from(&b"not png"[..]))),
            ..Brand::default()
        };
        let th = Theme::new(Mode::Dark, broken).with_mark(&picker);
        assert!(th.mark().is_none());
    }
}
