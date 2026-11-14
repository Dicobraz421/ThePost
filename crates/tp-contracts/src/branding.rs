//! Брендинг заказчика: знак, название продукта и фирменные цвета вместо IT TASK (CONTRACTS.md#Branding; решение 122,
//! CTR-91).
//!
//! Уровни — установка и организация ([`BrandingScope`]): действует ближайший заданный уровень целиком
//! ([`BrandingStore::effective_branding`]) — свой набор организации, иначе установки, иначе встроенный брендинг IT TASK
//! (`design/tokens.toml`). Байты знака — в BLOB по пути [`BrandLogo::path`] (`BlobStore::put_raw`), в хранилище —
//! только описание. Контраст цветов и цвета ролей проверяет Admin API (ADM-48), не контракт. Реализация — `tp-meta`
//! (META-84), in-memory — `tp-testkit`.

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
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct BrandColor([u8; 3]);

impl BrandColor {
    /// Цвет из составляющих `[r, g, b]`.
    pub const fn from_rgb(rgb: [u8; 3]) -> Self {
        Self(rgb)
    }

    /// Составляющие `[r, g, b]`.
    pub const fn rgb(self) -> [u8; 3] {
        self.0
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

/// Набор брендинга одного уровня: название продукта, основной цвет и акцент, знак (без знака — только название).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Branding {
    /// Название продукта вместо «The Post».
    pub product_name: String,
    /// Основной цвет.
    pub primary: BrandColor,
    /// Акцент.
    pub accent: BrandColor,
    /// Знак.
    pub logo: Option<BrandLogo>,
}

impl Branding {
    /// Название — от 1 до [`MAX_PRODUCT_NAME`] символов без управляющих; знак — [`BrandLogo::validate`]; иначе
    /// `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        let len = self.product_name.chars().count();
        if len == 0 || len > MAX_PRODUCT_NAME {
            return Err(Error::invalid(format!(
                "branding: product name of {len} characters, expected 1..={MAX_PRODUCT_NAME}"
            )));
        }
        if self.product_name.chars().any(char::is_control) {
            return Err(Error::invalid(
                "branding: product name contains control characters",
            ));
        }
        self.logo.as_ref().map_or(Ok(()), BrandLogo::validate)
    }
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

/// Хранилище брендинга по уровням. Права (администратор установки или организации) и журнал аудита — у вызывающего
/// (Admin API); байты знака пишет и удаляет вызывающий.
#[async_trait]
pub trait BrandingStore: Send + Sync + 'static {
    /// Свой набор уровня; не задан — `None` (без подстановки установки).
    async fn branding(&self, scope: BrandingScope) -> Result<Option<Branding>>;

    /// Заменяет набор уровня целиком. Неверный набор ([`Branding::validate`]) — `Error::Invalid`, прежний остаётся.
    async fn set_branding(&self, scope: BrandingScope, branding: &Branding) -> Result<()>;

    /// Удаляет набор уровня и возвращает прежний (`None` — не был задан). Знак из BLOB удаляет вызывающий, если
    /// [`BrandingStore::logo_in_use`] после сброса ложно.
    async fn reset_branding(&self, scope: BrandingScope) -> Result<Option<Branding>>;

    /// Действующий брендинг — ближайший заданный уровень целиком: свой набор организации `tenant`, иначе набор
    /// установки, иначе `None` (встроенный IT TASK). `tenant` = `None` — уровень установки (экран входа без
    /// известной организации).
    async fn effective_branding(&self, tenant: Option<TenantId>) -> Result<Option<Branding>>;

    /// Знак `hash` упомянут хоть одним уровнем.
    async fn logo_in_use(&self, hash: &BlobHash) -> Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logo() -> BrandLogo {
        BrandLogo {
            hash: BlobHash([0xab; 32]),
            size: 1000,
            width: 64,
            height: 32,
        }
    }

    fn branding() -> Branding {
        Branding {
            product_name: "Почта Ромашки".into(),
            primary: "#177CAA".parse().expect("primary"),
            accent: "#E4107F".parse().expect("accent"),
            logo: Some(logo()),
        }
    }

    #[test]
    fn object_safe() {
        fn _store(_: std::sync::Arc<dyn BrandingStore>) {}
    }

    #[test]
    fn color_parse() {
        let c: BrandColor = "#177CAA".parse().expect("upper");
        assert_eq!(c.rgb(), [0x17, 0x7c, 0xaa]);
        assert_eq!(c.to_string(), "#177CAA");
        let c: BrandColor = "#e4107f".parse().expect("lower");
        assert_eq!(c.to_string(), "#E4107F");
        assert_eq!(BrandColor::from_rgb([0xe4, 0x10, 0x7f]), c);
        for bad in [
            "#FFF", "177CAA", "#GGGGGG", "", "#", "#177CAA0", "# 77CAA", "#+7CAA0",
        ] {
            assert!(
                matches!(bad.parse::<BrandColor>(), Err(Error::Invalid(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn color_serde() {
        let c: BrandColor = "#e4107f".parse().expect("color");
        assert_eq!(serde_json::to_string(&c).expect("ser"), "\"#E4107F\"");
        let back: BrandColor = serde_json::from_str("\"#e4107f\"").expect("de");
        assert_eq!(back, c);
        assert!(serde_json::from_str::<BrandColor>("\"#FFF\"").is_err());
    }

    #[test]
    fn logo_path() {
        assert_eq!(logo().path(), format!("branding/{}.png", "ab".repeat(32)));
    }

    #[test]
    fn validate() {
        branding().validate().expect("valid");
        let mut b = branding();
        b.product_name = "Я".repeat(MAX_PRODUCT_NAME);
        b.validate().expect("64 символа");
        b.logo = None;
        b.validate().expect("без знака");
        let edge = BrandLogo {
            size: MAX_LOGO_SIZE,
            width: MAX_LOGO_SIDE,
            height: MAX_LOGO_SIDE,
            ..logo()
        };
        edge.validate().expect("предельный знак");

        let bad_names = [
            String::new(),
            "Я".repeat(MAX_PRODUCT_NAME + 1),
            "Почта\nРомашки".into(),
            "\u{7f}".into(),
        ];
        for name in bad_names {
            let b = Branding {
                product_name: name.clone(),
                ..branding()
            };
            assert!(matches!(b.validate(), Err(Error::Invalid(_))), "{name:?}");
        }
        let bad_logos = [
            BrandLogo { size: 0, ..logo() },
            BrandLogo {
                size: MAX_LOGO_SIZE + 1,
                ..logo()
            },
            BrandLogo { width: 0, ..logo() },
            BrandLogo {
                width: MAX_LOGO_SIDE + 1,
                ..logo()
            },
            BrandLogo {
                height: MAX_LOGO_SIDE + 1,
                ..logo()
            },
        ];
        for l in bad_logos {
            let b = Branding {
                logo: Some(l),
                ..branding()
            };
            assert!(matches!(b.validate(), Err(Error::Invalid(_))), "{l:?}");
        }
    }

    #[test]
    fn scope_serde() {
        assert_eq!(
            serde_json::to_string(&BrandingScope::Installation).expect("ser"),
            "\"installation\""
        );
        let org = BrandingScope::Org(TenantId::DEFAULT);
        let json = serde_json::to_string(&org).expect("ser");
        assert_eq!(
            serde_json::from_str::<BrandingScope>(&json).expect("de"),
            org
        );
    }
}
