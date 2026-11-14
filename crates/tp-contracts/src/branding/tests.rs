use std::collections::BTreeMap;

use super::*;

fn c(s: &str) -> BrandColor {
    s.parse().expect("color")
}

fn logo() -> BrandLogo {
    BrandLogo {
        hash: BlobHash([0xab; 32]),
        size: 1000,
        width: 64,
        height: 32,
    }
}

fn primary_only(primary: &str) -> Branding {
    Branding {
        primary: Some(c(primary)),
        ..Branding::default()
    }
}

#[test]
fn object_safe() {
    fn _store(_: std::sync::Arc<dyn BrandingStore>) {}
}

#[test]
fn color_parse() {
    let x = c("#177CAA");
    assert_eq!(x.rgb(), [0x17, 0x7c, 0xaa]);
    assert_eq!(x.to_string(), "#177CAA");
    let x = c("#e4107f");
    assert_eq!(x.to_string(), "#E4107F");
    assert_eq!(BrandColor::from_rgb([0xe4, 0x10, 0x7f]), x);
    assert_eq!(BrandColor::from_hex(0xE4107F), x);
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
    let x = c("#e4107f");
    assert_eq!(serde_json::to_string(&x).expect("ser"), "\"#E4107F\"");
    let back: BrandColor = serde_json::from_str("\"#e4107f\"").expect("de");
    assert_eq!(back, x);
    assert!(serde_json::from_str::<BrandColor>("\"#FFF\"").is_err());
}

#[test]
fn contrast_wcag() {
    let (w, b) = (BrandColor::WHITE, BrandColor::BLACK);
    assert!((w.contrast(b) - 21.0).abs() < 1e-9);
    assert!((w.contrast(w) - 1.0).abs() < 1e-9);
    assert_eq!(w.contrast(c("#177CAA")), c("#177CAA").contrast(w));
    // Розовый IT TASK: #E4107F с белым — 4,488 (ниже 4,5), #E3107F — 4,52.
    assert!(w.contrast(c("#E4107F")) < 4.5);
    assert!(w.contrast(c("#E3107F")) >= 4.5);
    assert_eq!(c("#E4107F").text_on(), b, "чёрный 4,68 выше белого 4,49");
    assert_eq!(c("#177CAA").text_on(), w);
    assert_eq!(c("#F5E663").text_on(), b);
}

#[test]
fn logo_path() {
    assert_eq!(logo().path(), format!("branding/{}.png", "ab".repeat(32)));
}

#[test]
fn validate() {
    Branding::default().validate().expect("пустой набор");
    let full = Branding {
        product_name: Some("Я".repeat(MAX_PRODUCT_NAME)),
        logo: Some(logo()),
        ..primary_only("#177CAA")
    };
    full.validate().expect("64 символа и знак");
    BrandLogo {
        size: MAX_LOGO_SIZE,
        width: MAX_LOGO_SIDE,
        height: MAX_LOGO_SIDE,
        ..logo()
    }
    .validate()
    .expect("предельный знак");

    for name in [
        String::new(),
        "Я".repeat(MAX_PRODUCT_NAME + 1),
        "Почта\nРомашки".into(),
        "\u{7f}".into(),
    ] {
        let b = Branding {
            product_name: Some(name.clone()),
            ..Branding::default()
        };
        assert!(matches!(b.validate(), Err(Error::Invalid(_))), "{name:?}");
    }
    for l in [
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
    ] {
        let b = Branding {
            logo: Some(l),
            ..Branding::default()
        };
        assert!(matches!(b.validate(), Err(Error::Invalid(_))), "{l:?}");
    }
}

#[test]
fn serde_names() {
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
    for role in Role::ALL {
        assert_eq!(
            serde_json::to_string(&role).expect("ser"),
            format!("\"{}\"", role.as_str())
        );
    }
    for role in CliRole::ALL {
        assert_eq!(
            serde_json::to_string(&role).expect("ser"),
            format!("\"{}\"", role.as_str())
        );
    }
    // Все поля необязательны: пустой объект — пустой набор.
    assert_eq!(
        serde_json::from_str::<Branding>("{}").expect("de"),
        Branding::default()
    );
    let b: Branding = serde_json::from_str(
        r##"{"primary":"#177caa","light":{"frameA":"#177CAA"},"cli":{"json_key":"#2A8BC4"}}"##,
    )
    .expect("de");
    assert_eq!(b.light.get(&Role::FrameA), Some(&c("#177CAA")));
    assert_eq!(b.cli.get(&CliRole::JsonKey), Some(&c("#2A8BC4")));
}

/// Значения `key = "..."` секции `[name]` файла токенов (до следующей секции).
fn section(toml: &str, name: &str) -> BTreeMap<String, String> {
    let header = format!("[{name}]");
    let mut out = BTreeMap::new();
    let mut inside = false;
    for line in toml.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line == header;
            continue;
        }
        if !inside || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=') {
            out.insert(k.trim().to_owned(), v.trim().to_owned());
        }
    }
    out
}

/// Строка в кавычках без комментария после неё.
fn unquote(v: &str) -> &str {
    let v = v.trim();
    v.strip_prefix('"')
        .and_then(|s| s.split('"').next())
        .unwrap_or(v)
}

/// Роли и цвета IT TASK в контракте — те же, что в `design/tokens.toml` (единственный источник цветов, решение 119).
/// До волны design у `sel` в файле — прежний `#E4107F` (решение 122: розовый выделения — `#E3107F`).
#[test]
fn it_task_matches_tokens() {
    let toml = include_str!("../../../../design/tokens.toml");
    let roles: Vec<String> = section(toml, "roles").into_keys().collect();
    let mut ours: Vec<String> = Role::ALL.iter().map(|r| r.as_str().to_owned()).collect();
    ours.sort();
    assert_eq!(roles, ours, "[roles]");
    for theme in Theme::ALL {
        let mode = section(toml, &format!("mode.{}", theme.as_str()));
        for role in Role::ALL {
            let file = c(unquote(&mode[role.as_str()]));
            let ours = role.it_task(theme);
            if role == Role::Sel && file == c("#E4107F") {
                assert_eq!(ours, c("#E3107F"));
                continue;
            }
            assert_eq!(ours, file, "{}.{}", theme.as_str(), role.as_str());
        }
    }
    let cli = section(toml, "cli.roles");
    assert_eq!(cli.len(), CliRole::ALL.len(), "[cli.roles]");
    for role in CliRole::ALL {
        let v = &cli[role.as_str()];
        let brand = v
            .split("brand = \"")
            .nth(1)
            .and_then(|r| r.get(..7))
            .expect("brand");
        assert_eq!(role.it_task(), c(brand), "cli.{}", role.as_str());
    }
    let cli_main = section(toml, "cli");
    assert_eq!(cli_main["backgrounds"], "[\"#0C0C0C\", \"#FAFAFA\"]");
    assert_eq!(cli_main["min"], CLI_MIN_CONTRAST.to_string() + ".0");
}

/// Пары [`CONTRAST`] — те же, что `[[contrast]]` файла токенов; порог `selFg` на `sel` — 4,5 (в файле до волны
/// design — 4,48 с исключением, решение 122 его отменило).
#[test]
fn contrast_pairs_match_tokens() {
    let toml = include_str!("../../../../design/tokens.toml");
    let mut pairs = Vec::new();
    for block in toml.split("[[contrast]]").skip(1) {
        let block = block.split("\n[").next().unwrap_or(block);
        let get = |k: &str| {
            block
                .lines()
                .find_map(|l| l.trim().strip_prefix(k)?.trim().strip_prefix('='))
                .map(str::trim)
                .expect("поле пары")
        };
        let fg = unquote(get("fg")).to_owned();
        let bg: Vec<String> = get("bg")
            .trim_matches(['[', ']'])
            .split(',')
            .map(|s| s.trim().trim_matches('"').to_owned())
            .collect();
        let min: f64 = get("min").parse().expect("min");
        pairs.push((fg, bg, min));
    }
    assert_eq!(pairs.len(), CONTRAST.len());
    for ((fg, bg, min), pair) in pairs.iter().zip(CONTRAST) {
        assert_eq!(fg, pair.fg.as_str());
        let ours: Vec<&str> = pair.bg.iter().map(|r| r.as_str()).collect();
        assert_eq!(bg, &ours, "{fg}");
        if pair.fg == Role::SelFg && (*min - 4.48).abs() < 1e-9 {
            assert!((pair.min - 4.5).abs() < 1e-9);
            continue;
        }
        assert!((pair.min - min).abs() < 1e-9, "{fg}: {min}");
    }
}

#[test]
fn it_task_passes_contrast() {
    assert_eq!(Palette::it_task().contrast_issues(), Vec::new());
    assert_eq!(
        BrandingLevels::default().effective().palette,
        Palette::it_task()
    );
}

#[test]
fn contrast_issue_names_role_and_pair() {
    let mut p = Palette::it_task();
    p.light.insert(Role::Sel, c("#E4107F"));
    let issues = p.contrast_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    let i = &issues[0];
    assert_eq!(
        (i.theme, i.fg, i.bg.as_str()),
        (Some(Theme::Light), "selFg", "sel")
    );
    assert_eq!((i.fg_color, i.bg_color), (BrandColor::WHITE, c("#E4107F")));
    assert!(i.contrast < 4.5 && (i.min - 4.5).abs() < 1e-9);
    assert!(
        i.to_string().contains("selFg #FFFFFF on sel #E4107F"),
        "{i}"
    );

    p.cli.insert(CliRole::Muted, c("#808080"));
    p.cli.insert(CliRole::Head, c("#202020"));
    let cli: Vec<_> = p
        .contrast_issues()
        .into_iter()
        .filter(|i| i.theme.is_none())
        .map(|i| (i.fg, i.bg))
        .collect();
    assert_eq!(cli, [("head", "#0C0C0C".to_owned())]);
}

/// Уровень без единого цвета берёт цвета уровня выше целиком.
#[test]
fn no_colors_inherit_whole() {
    let mut parent = primary_only("#2E7D32").palette_over(&Palette::it_task());
    parent.dark.insert(Role::Screen, c("#000000"));
    let named = Branding {
        product_name: Some("Ромашка".into()),
        logo: Some(logo()),
        ..Branding::default()
    };
    assert!(!named.has_colors());
    assert_eq!(named.palette_over(&parent), parent);
}

/// Задан только основной цвет: роли бренда выводятся из него, остальные — с уровня выше; заданные роли не
/// сдвигаются; функция чистая.
#[test]
fn derive_from_primary() {
    let it = Palette::it_task();
    let b = primary_only("#177CAA");
    let p = b.palette_over(&it);
    assert_eq!(p, b.palette_over(&it), "чистая функция");
    for theme in Theme::ALL {
        for role in [
            Role::Screen,
            Role::Panel,
            Role::Text,
            Role::Muted,
            Role::Frame,
            Role::SelI,
            Role::SelIFg,
            Role::Ok,
            Role::Warn,
            Role::Err,
        ] {
            assert_eq!(
                p.role(theme, role),
                it.role(theme, role),
                "{theme:?} {role:?}"
            );
        }
    }
    // Светлая тема: #177CAA набирает пороги без сдвига.
    for role in [Role::FrameA, Role::FkBg, Role::Sel, Role::Head] {
        assert_eq!(p.role(Theme::Light, role), c("#177CAA"), "{role:?}");
    }
    assert_eq!(p.role(Theme::Light, Role::SelFg), BrandColor::WHITE);
    assert_eq!(p.role(Theme::Light, Role::FkFg), BrandColor::WHITE);
    // Тёмная тема: заголовок колонок на тёмной панели светлеет до 4,5.
    let head = p.role(Theme::Dark, Role::Head);
    assert_ne!(head, c("#177CAA"));
    assert!(head.contrast(p.role(Theme::Dark, Role::Panel)) >= 4.5);
    assert_eq!(p.cli_role(CliRole::Ok), CliRole::Ok.it_task());
    assert_eq!(p.contrast_issues(), Vec::new());

    // Заданная роль остаётся как задана, даже ниже порога; текст на ней выводится.
    let mut b = primary_only("#177CAA");
    b.light.insert(Role::Sel, c("#F5E663"));
    let p = b.palette_over(&it);
    assert_eq!(p.role(Theme::Light, Role::Sel), c("#F5E663"));
    assert_eq!(p.role(Theme::Light, Role::SelFg), BrandColor::BLACK);
    let issues = p.contrast_issues();
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!((issues[0].fg, issues[0].bg.as_str()), ("sel", "panel"));
    // Акцент CLI — из sel светлой темы, подогнанный к обоим фонам терминала.
    let accent = p.cli_role(CliRole::Accent);
    assert!(
        CLI_BACKGROUNDS
            .iter()
            .all(|bg| accent.contrast(*bg) >= CLI_MIN_CONTRAST)
    );
}

/// Только `sel` без основного: `titleA` и `selFg` выводятся из него, роли основного — с уровня выше.
#[test]
fn derive_from_sel_only() {
    let it = Palette::it_task();
    let mut b = Branding::default();
    b.dark.insert(Role::Sel, c("#2E7D32"));
    let p = b.palette_over(&it);
    assert_eq!(
        p.role(Theme::Dark, Role::FkBg),
        it.role(Theme::Dark, Role::FkBg)
    );
    assert_eq!(p.role(Theme::Dark, Role::SelFg), c("#2E7D32").text_on());
    assert_ne!(
        p.role(Theme::Dark, Role::TitleA),
        it.role(Theme::Dark, Role::TitleA)
    );
    assert_eq!(p.light, it.light, "светлая тема не тронута");
    assert_eq!(p.cli, it.cli, "акцент CLI — от sel светлой темы");
}

/// Любой основной цвет даёт палитру без нарушений контраста.
#[test]
fn any_primary_passes() {
    let it = Palette::it_task();
    for r in (0..=255u8).step_by(51) {
        for g in (0..=255u8).step_by(51) {
            for b in (0..=255u8).step_by(51) {
                let primary = BrandColor::from_rgb([r, g, b]);
                let p = Branding {
                    primary: Some(primary),
                    ..Branding::default()
                }
                .palette_over(&it);
                assert_eq!(p.contrast_issues(), Vec::new(), "{primary}");
            }
        }
    }
}

/// Каждое поле — с ближайшего уровня, где оно задано: организация → установка → IT TASK.
#[test]
fn levels_effective() {
    let installation = Branding {
        product_name: Some("Почта Холдинга".into()),
        logo: Some(logo()),
        ..primary_only("#2E7D32")
    };
    let inst_palette = installation.palette_over(&Palette::it_task());
    let only_name = Branding {
        product_name: Some("Ромашка".into()),
        ..Branding::default()
    };
    let e = BrandingLevels {
        installation: Some(installation.clone()),
        org: Some(only_name),
    }
    .effective();
    assert_eq!(e.product_name.as_deref(), Some("Ромашка"));
    assert_eq!(e.logo, Some(logo()), "знак установки");
    assert_eq!(e.palette, inst_palette, "цвета установки");

    let mut org = primary_only("#6A1B9A");
    org.light.insert(Role::Text, c("#000000"));
    let e = BrandingLevels {
        installation: Some(installation.clone()),
        org: Some(org.clone()),
    }
    .effective();
    assert_eq!(e.product_name.as_deref(), Some("Почта Холдинга"));
    assert_eq!(e.palette, org.palette_over(&inst_palette));
    assert_eq!(e.palette.role(Theme::Light, Role::Text), c("#000000"));

    let e = BrandingLevels {
        installation: None,
        org: Some(Branding::default()),
    }
    .effective();
    assert_eq!(e.product_name, None);
    assert_eq!(e.logo, None);
    assert_eq!(e.palette, Palette::it_task());
}
