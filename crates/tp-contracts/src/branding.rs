//! Брендинг заказчика: знак, название продукта и цвета ролей вместо IT TASK (CONTRACTS.md#Branding; решение 122,
//! CTR-91).
//!
//! Уровни — установка и организация ([`BrandingScope`]). Каждое поле набора необязательно: незаданное берётся с
//! уровня выше — организация → установка → встроенный IT TASK ([`Palette::it_task`], `design/tokens.toml`);
//! итог — [`BrandingLevels::effective`]. Цвета — основной цвет и роли `design/tokens.toml`: [`Role`] — отдельно для
//! светлой и тёмной темы, [`CliRole`] — подсветка вывода CLI; незаданные роли уровня с основным цветом выводятся из
//! него ([`Branding::palette_over`]). Контраст пар [`CONTRAST`] — [`Palette::contrast_issues`]: порог 4,5 у текста и
//! 3 у рамок и заливок, без исключений. Байты знака — в BLOB по пути [`BrandLogo::path`] (`BlobStore::put_raw`), в
//! хранилище — только описание. Реализация — `tp-meta` (META-84), in-memory — `tp-testkit`.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::error::{Error, Result};
use crate::ids::{BlobHash, TenantId};

/// Наибольшая длина названия продукта, символов.
pub const MAX_PRODUCT_NAME: usize = 64;

/// Наибольший размер знака, байт (256 КБ).
pub const MAX_LOGO_SIZE: u64 = 256 * 1024;

/// Наибольшая ширина и высота знака, пикселей.
pub const MAX_LOGO_SIDE: u32 = 1024;

/// Фирменный цвет `#RRGGBB`. `FromStr` принимает шестнадцатеричные цифры в любом регистре; `Display` и serde
/// (строкой) — верхний регистр; иное — `Error::Invalid`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BrandColor([u8; 3]);

impl BrandColor {
    /// Белый.
    pub const WHITE: BrandColor = BrandColor([0xff, 0xff, 0xff]);
    /// Чёрный.
    pub const BLACK: BrandColor = BrandColor([0, 0, 0]);

    /// Цвет из составляющих `[r, g, b]`.
    pub const fn from_rgb(rgb: [u8; 3]) -> Self {
        Self(rgb)
    }

    /// Цвет из `0xRRGGBB`.
    pub const fn from_hex(v: u32) -> Self {
        Self([(v >> 16) as u8, (v >> 8) as u8, v as u8])
    }

    /// Составляющие `[r, g, b]`.
    pub const fn rgb(self) -> [u8; 3] {
        self.0
    }

    /// Относительная яркость WCAG 2.x (0 — чёрный, 1 — белый).
    pub fn luminance(self) -> f64 {
        fn lin(c: u8) -> f64 {
            let c = f64::from(c) / 255.0;
            if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            }
        }
        let [r, g, b] = self.0;
        0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b)
    }

    /// Контраст WCAG 2.x с `other`: от 1 до 21, порядок цветов не важен.
    pub fn contrast(self, other: BrandColor) -> f64 {
        let (a, b) = (self.luminance(), other.luminance());
        (a.max(b) + 0.05) / (a.min(b) + 0.05)
    }

    /// Смесь с `target` в доле `t` (0 — этот цвет, 1 — `target`) по составляющим sRGB.
    fn mix(self, target: BrandColor, t: f64) -> BrandColor {
        let mut out = [0u8; 3];
        for (i, o) in out.iter_mut().enumerate() {
            let (a, b) = (f64::from(self.0[i]), f64::from(target.0[i]));
            // В пределах 0..=255: смесь двух значений u8 с долей из 0..=1.
            *o = (a + (b - a) * t).round().clamp(0.0, 255.0) as u8;
        }
        BrandColor(out)
    }

    /// Белый или чёрный — у кого контраст с этим цветом выше (текст на заливке).
    pub fn text_on(self) -> BrandColor {
        if BrandColor::WHITE.contrast(self) >= BrandColor::BLACK.contrast(self) {
            BrandColor::WHITE
        } else {
            BrandColor::BLACK
        }
    }
}

impl fmt::Display for BrandColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [r, g, b] = self.0;
        write!(f, "#{r:02X}{g:02X}{b:02X}")
    }
}

impl fmt::Debug for BrandColor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BrandColor({self})")
    }
}

impl FromStr for BrandColor {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self> {
        let bad = || Error::invalid(format!("brand color: expected #RRGGBB, got {s:?}"));
        let hex = s.strip_prefix('#').ok_or_else(bad)?;
        if hex.len() != 6 || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(bad());
        }
        let mut rgb = [0u8; 3];
        for (i, c) in rgb.iter_mut().enumerate() {
            *c = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).map_err(|_| bad())?;
        }
        Ok(Self(rgb))
    }
}

impl Serialize for BrandColor {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for BrandColor {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Тема веб-консоли и текстового интерфейса.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Theme {
    /// Светлая (`[mode.light]`).
    Light,
    /// Тёмная (`[mode.dark]`).
    Dark,
}

impl Theme {
    /// Обе темы.
    pub const ALL: [Theme; 2] = [Theme::Light, Theme::Dark];

    /// Имя темы — как в JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Theme::Light => "light",
            Theme::Dark => "dark",
        }
    }
}

/// Роль цвета темы — `[roles]` файла `design/tokens.toml`; имя в JSON — как в файле.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Role {
    /// Фон экрана.
    Screen,
    /// Фон панели (подложка).
    Panel,
    /// Текст.
    Text,
    /// Вторичный текст, счётчики.
    Muted,
    /// Рамка неактивной панели (декоративная).
    Frame,
    /// Рамка активной панели, прокрутка.
    #[serde(rename = "frameA")]
    FrameA,
    /// Заголовок активной панели, текст-акцент.
    #[serde(rename = "titleA")]
    TitleA,
    /// Заголовки колонок.
    Head,
    /// Выделенная строка, фон (акцент).
    Sel,
    /// Текст выделенной строки.
    SelFg,
    /// Выделенная строка неактивной панели, фон.
    #[serde(rename = "selI")]
    SelI,
    /// Текст выделенной строки неактивной панели.
    #[serde(rename = "selIFg")]
    SelIFg,
    /// Подписи F-клавиш, фон (основной).
    FkBg,
    /// Подписи F-клавиш, текст.
    FkFg,
    /// Норма.
    Ok,
    /// Внимание.
    Warn,
    /// Ошибка, блокировка.
    Err,
}

impl Role {
    /// Все роли — в порядке [`Branding::palette_over`]: фоны раньше текста на них.
    pub const ALL: [Role; 17] = [
        Role::Screen,
        Role::Panel,
        Role::SelI,
        Role::Frame,
        Role::Text,
        Role::Muted,
        Role::SelIFg,
        Role::Ok,
        Role::Warn,
        Role::Err,
        Role::FrameA,
        Role::FkBg,
        Role::Sel,
        Role::Head,
        Role::TitleA,
        Role::SelFg,
        Role::FkFg,
    ];

    /// Имя роли — как в `design/tokens.toml` и JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            Role::Screen => "screen",
            Role::Panel => "panel",
            Role::Text => "text",
            Role::Muted => "muted",
            Role::Frame => "frame",
            Role::FrameA => "frameA",
            Role::TitleA => "titleA",
            Role::Head => "head",
            Role::Sel => "sel",
            Role::SelFg => "selFg",
            Role::SelI => "selI",
            Role::SelIFg => "selIFg",
            Role::FkBg => "fkBg",
            Role::FkFg => "fkFg",
            Role::Ok => "ok",
            Role::Warn => "warn",
            Role::Err => "err",
        }
    }

    /// Цвет роли IT TASK (`[mode.light]`, `[mode.dark]` файла токенов).
    pub const fn it_task(self, theme: Theme) -> BrandColor {
        let v = match theme {
            Theme::Light => match self {
                Role::Screen => 0xE9EEF3,
                Role::Panel => 0xFFFFFF,
                Role::Text => 0x161616,
                Role::Muted => 0x5F6B78,
                Role::Frame => 0xBAC1C6,
                Role::FrameA => 0x177CAA,
                Role::TitleA => 0xC0106B,
                Role::Head => 0x177CAA,
                Role::Sel => 0xE3107F,
                Role::SelFg => 0xFFFFFF,
                Role::SelI => 0xD7E2EA,
                Role::SelIFg => 0x161616,
                Role::FkBg => 0x177CAA,
                Role::FkFg => 0xFFFFFF,
                Role::Ok => 0x0E8483,
                Role::Warn => 0x9A6F00,
                Role::Err => 0xC62828,
            },
            Theme::Dark => match self {
                Role::Screen => 0x0F1317,
                Role::Panel => 0x141A20,
                Role::Text => 0xD8E7F4,
                Role::Muted => 0x7E8A96,
                Role::Frame => 0x3A4652,
                Role::FrameA => 0x3A9BD5,
                Role::TitleA => 0xFB5FB0,
                Role::Head => 0x2EBCBA,
                Role::Sel => 0xE3107F,
                Role::SelFg => 0xFFFFFF,
                Role::SelI => 0x26313B,
                Role::SelIFg => 0xD8E7F4,
                Role::FkBg => 0x177CAA,
                Role::FkFg => 0xFFFFFF,
                Role::Ok => 0x2EBCBA,
                Role::Warn => 0xE7B512,
                Role::Err => 0xFF5C5C,
            },
        };
        BrandColor::from_hex(v)
    }
}

/// Роль подсветки вывода CLI — `[cli.roles]` файла `design/tokens.toml` (значение `brand`, при
/// `COLORTERM=truecolor`); тема у CLI одна — фон терминала неизвестен.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CliRole {
    /// Заголовки.
    Head,
    /// Акцент.
    Accent,
    /// Норма.
    Ok,
    /// Внимание.
    Warn,
    /// Ошибка.
    Err,
    /// Вторичный текст.
    Muted,
    /// Ключ JSON.
    JsonKey,
    /// Число JSON.
    JsonNumber,
    /// Логическое значение JSON.
    JsonBool,
}

impl CliRole {
    /// Все роли — в порядке [`Branding::palette_over`].
    pub const ALL: [CliRole; 9] = [
        CliRole::Ok,
        CliRole::Warn,
        CliRole::Err,
        CliRole::Muted,
        CliRole::JsonNumber,
        CliRole::Head,
        CliRole::JsonKey,
        CliRole::Accent,
        CliRole::JsonBool,
    ];

    /// Имя роли — как в `design/tokens.toml` и JSON.
    pub const fn as_str(self) -> &'static str {
        match self {
            CliRole::Head => "head",
            CliRole::Accent => "accent",
            CliRole::Ok => "ok",
            CliRole::Warn => "warn",
            CliRole::Err => "err",
            CliRole::Muted => "muted",
            CliRole::JsonKey => "json_key",
            CliRole::JsonNumber => "json_number",
            CliRole::JsonBool => "json_bool",
        }
    }

    /// Цвет роли IT TASK (`brand` в `[cli.roles]`).
    pub const fn it_task(self) -> BrandColor {
        BrandColor::from_hex(match self {
            CliRole::Head | CliRole::JsonKey => 0x2A8BC4,
            CliRole::Accent | CliRole::JsonBool => 0xE4107F,
            CliRole::Ok | CliRole::JsonNumber => 0x1A9694,
            CliRole::Warn => 0xB07D00,
            CliRole::Err => 0xE5484D,
            CliRole::Muted => 0x7E8A96,
        })
    }
}

/// Пара контраста DES-01 (`[[contrast]]` файла токенов): роль `fg` на каждом фоне `bg` — не ниже `min`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ContrastPair {
    /// Передний план.
    pub fg: Role,
    /// Фоны.
    pub bg: &'static [Role],
    /// Наименьший контраст: текст — 4,5, рамки и заливки — 3.
    pub min: f64,
}

/// Пары контраста тем — `[[contrast]]` файла токенов, без исключений (решение 122: текст — 4,5, рамки и
/// заливки — 3).
pub const CONTRAST: &[ContrastPair] = &[
    ContrastPair {
        fg: Role::Text,
        bg: &[Role::Screen, Role::Panel],
        min: 4.5,
    },
    ContrastPair {
        fg: Role::SelIFg,
        bg: &[Role::SelI],
        min: 4.5,
    },
    ContrastPair {
        fg: Role::Muted,
        bg: &[Role::Screen, Role::Panel],
        min: 4.5,
    },
    ContrastPair {
        fg: Role::Head,
        bg: &[Role::Panel],
        min: 4.5,
    },
    ContrastPair {
        fg: Role::TitleA,
        bg: &[Role::Panel],
        min: 4.5,
    },
    ContrastPair {
        fg: Role::SelFg,
        bg: &[Role::Sel],
        min: 4.5,
    },
    ContrastPair {
        fg: Role::FkFg,
        bg: &[Role::FkBg],
        min: 4.5,
    },
    ContrastPair {
        fg: Role::Ok,
        bg: &[Role::Panel],
        min: 4.5,
    },
    ContrastPair {
        fg: Role::Warn,
        bg: &[Role::Panel],
        min: 4.5,
    },
    ContrastPair {
        fg: Role::Err,
        bg: &[Role::Panel],
        min: 4.5,
    },
    ContrastPair {
        fg: Role::FrameA,
        bg: &[Role::Screen, Role::Panel],
        min: 3.0,
    },
    ContrastPair {
        fg: Role::Sel,
        bg: &[Role::Panel],
        min: 3.0,
    },
    ContrastPair {
        fg: Role::FkBg,
        bg: &[Role::Screen],
        min: 3.0,
    },
];

/// Фоны терминала для ролей CLI (`[cli].backgrounds`): тёмный и светлый.
pub const CLI_BACKGROUNDS: [BrandColor; 2] = [
    BrandColor::from_hex(0x0C0C0C),
    BrandColor::from_hex(0xFAFAFA),
];

/// Наименьший контраст роли CLI с каждым фоном [`CLI_BACKGROUNDS`] (`[cli].min`).
pub const CLI_MIN_CONTRAST: f64 = 3.0;

/// Пара, не набравшая контраста ([`Palette::contrast_issues`]).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ContrastIssue {
    /// Тема; `None` — роли CLI.
    pub theme: Option<Theme>,
    /// Роль переднего плана (имя из файла токенов).
    pub fg: &'static str,
    /// Роль фона; у CLI — цвет фона терминала (`#0C0C0C`, `#FAFAFA`).
    pub bg: String,
    /// Цвет переднего плана.
    pub fg_color: BrandColor,
    /// Цвет фона.
    pub bg_color: BrandColor,
    /// Контраст пары.
    pub contrast: f64,
    /// Нужный контраст.
    pub min: f64,
}

impl fmt::Display for ContrastIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let theme = self.theme.map_or("cli", Theme::as_str);
        write!(
            f,
            "{theme}: {} {} on {} {} — contrast {:.2}:1, need {:.2}:1",
            self.fg, self.fg_color, self.bg, self.bg_color, self.contrast, self.min
        )
    }
}

/// Все цвета ролей — заданные и выведенные ([`Branding::palette_over`]) или встроенные ([`Palette::it_task`]).
/// Нет роли в карте (ответ сервера старой версии) — [`Palette::role`] берёт цвет IT TASK.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Palette {
    /// Светлая тема.
    pub light: BTreeMap<Role, BrandColor>,
    /// Тёмная тема.
    pub dark: BTreeMap<Role, BrandColor>,
    /// Подсветка CLI.
    pub cli: BTreeMap<CliRole, BrandColor>,
}

impl Palette {
    /// Встроенный брендинг IT TASK — роли файла `design/tokens.toml`.
    pub fn it_task() -> Palette {
        let theme = |t| Role::ALL.iter().map(|r| (*r, r.it_task(t))).collect();
        Palette {
            light: theme(Theme::Light),
            dark: theme(Theme::Dark),
            cli: CliRole::ALL.iter().map(|r| (*r, r.it_task())).collect(),
        }
    }

    /// Карта темы.
    pub fn theme(&self, theme: Theme) -> &BTreeMap<Role, BrandColor> {
        match theme {
            Theme::Light => &self.light,
            Theme::Dark => &self.dark,
        }
    }

    /// Цвет роли темы.
    pub fn role(&self, theme: Theme, role: Role) -> BrandColor {
        self.theme(theme)
            .get(&role)
            .copied()
            .unwrap_or_else(|| role.it_task(theme))
    }

    /// Цвет роли CLI.
    pub fn cli_role(&self, role: CliRole) -> BrandColor {
        self.cli
            .get(&role)
            .copied()
            .unwrap_or_else(|| role.it_task())
    }

    /// Пары [`CONTRAST`] обеих тем и роли CLI на [`CLI_BACKGROUNDS`] ниже порога — в порядке тем, пар и фонов.
    pub fn contrast_issues(&self) -> Vec<ContrastIssue> {
        let mut out = Vec::new();
        for theme in Theme::ALL {
            for pair in CONTRAST {
                for bg in pair.bg {
                    let min = pair.min;
                    let (fg_color, bg_color) = (self.role(theme, pair.fg), self.role(theme, *bg));
                    let contrast = fg_color.contrast(bg_color);
                    if contrast < min {
                        out.push(ContrastIssue {
                            theme: Some(theme),
                            fg: pair.fg.as_str(),
                            bg: bg.as_str().to_owned(),
                            fg_color,
                            bg_color,
                            contrast,
                            min,
                        });
                    }
                }
            }
        }
        for role in CliRole::ALL {
            let fg_color = self.cli_role(role);
            for bg_color in CLI_BACKGROUNDS {
                let contrast = fg_color.contrast(bg_color);
                if contrast < CLI_MIN_CONTRAST {
                    out.push(ContrastIssue {
                        theme: None,
                        fg: role.as_str(),
                        bg: bg_color.to_string(),
                        fg_color,
                        bg_color,
                        contrast,
                        min: CLI_MIN_CONTRAST,
                    });
                }
            }
        }
        out
    }
}

/// Ближайший к `base` цвет, у которого контраст с каждым из `bgs` не ниже `min`: смесь с белым или чёрным с шагом
/// 1 %, сначала в сторону `first`. Не нашлось — `base` (пару назовёт [`Palette::contrast_issues`]).
fn fit(base: BrandColor, bgs: &[BrandColor], min: f64, first: BrandColor) -> BrandColor {
    let other = if first == BrandColor::WHITE {
        BrandColor::BLACK
    } else {
        BrandColor::WHITE
    };
    for step in 0..=100 {
        let t = f64::from(step) / 100.0;
        for target in [first, other] {
            let c = base.mix(target, t);
            if bgs.iter().all(|bg| c.contrast(*bg) >= min) {
                return c;
            }
        }
    }
    base
}

/// Знак заказчика — PNG в BLOB. Формат и размеры по содержимому проверяет вызывающий (Admin API) до записи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BrandLogo {
    /// SHA-256 содержимого.
    pub hash: BlobHash,
    /// Размер, байт.
    pub size: u64,
    /// Ширина, пикселей.
    pub width: u32,
    /// Высота, пикселей.
    pub height: u32,
}

impl BrandLogo {
    /// Путь знака в BLOB (`BlobStore::put_raw`): `branding/<hash>.png`. Путь по содержимому неизменяем — новый знак
    /// записывается по новому пути.
    pub fn path(&self) -> String {
        format!("branding/{}.png", self.hash)
    }

    /// Размер от 1 байта до [`MAX_LOGO_SIZE`], ширина и высота от 1 до [`MAX_LOGO_SIDE`]; иначе `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if self.size == 0 || self.size > MAX_LOGO_SIZE {
            return Err(Error::invalid(format!(
                "brand logo: size {} bytes, expected 1..={MAX_LOGO_SIZE}",
                self.size
            )));
        }
        let side = 1..=MAX_LOGO_SIDE;
        if !side.contains(&self.width) || !side.contains(&self.height) {
            return Err(Error::invalid(format!(
                "brand logo: {}x{}, expected at most {MAX_LOGO_SIDE}x{MAX_LOGO_SIDE}",
                self.width, self.height
            )));
        }
        Ok(())
    }
}

/// Набор брендинга одного уровня. Каждое поле необязательно: незаданное берётся с уровня выше
/// ([`BrandingLevels::effective`]); пустой набор ничего не меняет.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct Branding {
    /// Название продукта вместо встроенного.
    #[serde(default)]
    pub product_name: Option<String>,
    /// Основной цвет — источник незаданных ролей бренда этого уровня ([`Branding::palette_over`]).
    #[serde(default)]
    pub primary: Option<BrandColor>,
    /// Заданные роли светлой темы.
    #[serde(default)]
    pub light: BTreeMap<Role, BrandColor>,
    /// Заданные роли тёмной темы.
    #[serde(default)]
    pub dark: BTreeMap<Role, BrandColor>,
    /// Заданные роли CLI.
    #[serde(default)]
    pub cli: BTreeMap<CliRole, BrandColor>,
    /// Знак.
    #[serde(default)]
    pub logo: Option<BrandLogo>,
}

impl Branding {
    /// Заданные роли темы.
    pub fn roles(&self, theme: Theme) -> &BTreeMap<Role, BrandColor> {
        match theme {
            Theme::Light => &self.light,
            Theme::Dark => &self.dark,
        }
    }

    /// Задан ли уровнем хоть один цвет (основной или роль).
    pub fn has_colors(&self) -> bool {
        self.primary.is_some()
            || !self.light.is_empty()
            || !self.dark.is_empty()
            || !self.cli.is_empty()
    }

    /// Цвета ролей этого уровня поверх цветов уровня выше `parent`. Заданная роль — как задана (не сдвигается);
    /// незаданная — по порядку [`Role::ALL`] и [`CliRole::ALL`] (фоны раньше текста на них):
    ///
    /// - выводится, если у уровня есть источник: `frameA`, `fkBg`, `sel`, `head` и CLI `head`, `json_key` — из
    ///   основного цвета; `titleA` — из `sel` темы, а `selFg` — белый или чёрный с большим контрастом к `sel`
    ///   ([`BrandColor::text_on`]), если уровень задал основной цвет или `sel` темы; `fkFg` — так же к `fkBg`; CLI
    ///   `accent`, `json_bool` — из `sel` светлой темы, если уровень задал основной цвет или `sel` светлой темы;
    /// - выведенная роль подгоняется: смешивается с чёрным (светлая тема) или белым (тёмная) с шагом 1 %, пока не
    ///   наберёт порог каждой пары [`CONTRAST`], где она передний план, на уже найденных фонах; роль CLI — порог
    ///   [`CLI_MIN_CONTRAST`] на обоих [`CLI_BACKGROUNDS`] (сначала белый, если источник темнее порога на тёмном фоне);
    /// - иначе — цвет роли `parent` как есть (фоны, текст, `frame`, выделение неактивной панели, роли состояний; у
    ///   уровня без единого цвета — все роли `parent` целиком).
    ///
    /// Функция чистая: те же набор и `parent` — те же цвета.
    pub fn palette_over(&self, parent: &Palette) -> Palette {
        let mut themes = [BTreeMap::new(), BTreeMap::new()];
        for (i, theme) in Theme::ALL.into_iter().enumerate() {
            let (set, out) = (self.roles(theme), &mut themes[i]);
            let toward = match theme {
                Theme::Light => BrandColor::BLACK,
                Theme::Dark => BrandColor::WHITE,
            };
            let sel_here = self.primary.is_some() || set.contains_key(&Role::Sel);
            let fk_here = self.primary.is_some() || set.contains_key(&Role::FkBg);
            for role in Role::ALL {
                if let Some(c) = set.get(&role) {
                    out.insert(role, *c);
                    continue;
                }
                let resolved = |r: Role, out: &BTreeMap<Role, BrandColor>| {
                    out.get(&r)
                        .copied()
                        .unwrap_or_else(|| parent.role(theme, r))
                };
                let base = match role {
                    Role::FrameA | Role::FkBg | Role::Sel | Role::Head => self.primary,
                    Role::TitleA if sel_here => Some(resolved(Role::Sel, out)),
                    Role::SelFg if sel_here => Some(resolved(Role::Sel, out).text_on()),
                    Role::FkFg if fk_here => Some(resolved(Role::FkBg, out).text_on()),
                    _ => None,
                };
                let color = match base {
                    Some(mut color) => {
                        for pair in CONTRAST.iter().filter(|p| p.fg == role) {
                            let bgs: Vec<BrandColor> =
                                pair.bg.iter().map(|b| resolved(*b, out)).collect();
                            color = fit(color, &bgs, pair.min, toward);
                        }
                        color
                    }
                    None => parent.role(theme, role),
                };
                out.insert(role, color);
            }
        }
        let [light, dark] = themes;
        let accent_here = self.primary.is_some() || self.light.contains_key(&Role::Sel);
        let mut cli = BTreeMap::new();
        for role in CliRole::ALL {
            if let Some(c) = self.cli.get(&role) {
                cli.insert(role, *c);
                continue;
            }
            let base = match role {
                CliRole::Head | CliRole::JsonKey => self.primary,
                CliRole::Accent | CliRole::JsonBool if accent_here => {
                    light.get(&Role::Sel).copied()
                }
                _ => None,
            };
            let color = match base {
                Some(base) => {
                    let first = if base.contrast(CLI_BACKGROUNDS[0]) < CLI_MIN_CONTRAST {
                        BrandColor::WHITE
                    } else {
                        BrandColor::BLACK
                    };
                    fit(base, &CLI_BACKGROUNDS, CLI_MIN_CONTRAST, first)
                }
                None => parent.cli_role(role),
            };
            cli.insert(role, color);
        }
        Palette { light, dark, cli }
    }

    /// Название (если задано) — от 1 до [`MAX_PRODUCT_NAME`] символов без управляющих; знак (если задан) —
    /// [`BrandLogo::validate`]; иначе `Error::Invalid`. Контраст зависит от уровня выше — его проверяет вызывающий
    /// по [`BrandingLevels::effective`] ([`Palette::contrast_issues`]).
    pub fn validate(&self) -> Result<()> {
        if let Some(name) = &self.product_name {
            let len = name.chars().count();
            if len == 0 || len > MAX_PRODUCT_NAME {
                return Err(Error::invalid(format!(
                    "branding: product name of {len} characters, expected 1..={MAX_PRODUCT_NAME}"
                )));
            }
            if name.chars().any(char::is_control) {
                return Err(Error::invalid(
                    "branding: product name contains control characters",
                ));
            }
        }
        self.logo.as_ref().map_or(Ok(()), BrandLogo::validate)
    }
}

/// Наборы уровней для одной организации ([`BrandingStore::branding_levels`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BrandingLevels {
    /// Набор установки.
    pub installation: Option<Branding>,
    /// Свой набор организации.
    pub org: Option<Branding>,
}

impl BrandingLevels {
    /// Действующий брендинг: название и знак — с ближайшего уровня, где они заданы (организация → установка →
    /// встроенные); цвета — [`Branding::palette_over`] установки поверх [`Palette::it_task`], затем организации
    /// поверх итога (уровень без цветов их не меняет).
    pub fn effective(&self) -> EffectiveBranding {
        let mut out = EffectiveBranding {
            product_name: None,
            logo: None,
            palette: Palette::it_task(),
        };
        for level in [&self.installation, &self.org].into_iter().flatten() {
            if level.product_name.is_some() {
                out.product_name.clone_from(&level.product_name);
            }
            if level.logo.is_some() {
                out.logo = level.logo;
            }
            if level.has_colors() {
                out.palette = level.palette_over(&out.palette);
            }
        }
        out
    }
}

/// Действующий брендинг ([`BrandingLevels::effective`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveBranding {
    /// Название продукта; `None` — встроенное.
    pub product_name: Option<String>,
    /// Знак; `None` — встроенный знак IT TASK.
    pub logo: Option<BrandLogo>,
    /// Все цвета ролей; без цветов на уровнях — [`Palette::it_task`].
    pub palette: Palette,
}

/// Уровень брендинга.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrandingScope {
    /// Установка — для всех организаций без своего набора.
    Installation,
    /// Организация — заменяет набор установки целиком.
    Org(TenantId),
}

/// Хранилище брендинга по уровням. Права (администратор установки или организации), проверка контраста
/// ([`BrandingLevels::effective`], [`Palette::contrast_issues`]) и журнал аудита — у вызывающего (Admin API); байты
/// знака пишет и удаляет вызывающий.
#[async_trait]
pub trait BrandingStore: Send + Sync + 'static {
    /// Свой набор уровня; не задан — `None` (без подстановки уровня выше).
    async fn branding(&self, scope: BrandingScope) -> Result<Option<Branding>>;

    /// Заменяет набор уровня целиком. Неверный набор ([`Branding::validate`]) — `Error::Invalid`, прежний остаётся.
    async fn set_branding(&self, scope: BrandingScope, branding: &Branding) -> Result<()>;

    /// Удаляет набор уровня и возвращает прежний (`None` — не был задан). Знак из BLOB удаляет вызывающий, если
    /// [`BrandingStore::logo_in_use`] после сброса ложно.
    async fn reset_branding(&self, scope: BrandingScope) -> Result<Option<Branding>>;

    /// Наборы установки и организации `tenant` одним чтением (`tenant` = `None` — только установка: экран входа без
    /// известной организации); действующий брендинг — [`BrandingLevels::effective`].
    async fn branding_levels(&self, tenant: Option<TenantId>) -> Result<BrandingLevels>;

    /// Знак `hash` упомянут хоть одним уровнем.
    async fn logo_in_use(&self, hash: &BlobHash) -> Result<bool>;
}

#[cfg(test)]
mod tests;
