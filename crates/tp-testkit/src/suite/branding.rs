//! Набор проверок [`BrandingStore`] (CTR-91, CONTRACTS.md#Branding): in-memory `MemBranding` и `PgMeta` (META-84)
//! проходят одни и те же проверки.

use tp_contracts::branding::{
    BrandColor, BrandLogo, Branding, BrandingLevels, BrandingScope, BrandingStore, CliRole,
    Palette, Role,
};
use tp_contracts::{BlobHash, Error, TenantId};

fn logo(byte: u8) -> BrandLogo {
    BrandLogo {
        hash: BlobHash([byte; 32]),
        size: 4096,
        width: 256,
        height: 128,
    }
}

fn color(s: &str) -> BrandColor {
    s.parse().expect("color")
}

fn levels(installation: Option<&Branding>, org: Option<&Branding>) -> BrandingLevels {
    BrandingLevels {
        installation: installation.cloned(),
        org: org.cloned(),
    }
}

/// Набор проверок брендинга по уровням. Хранилище в начале пустое; `org` и `foreign` — разные организации.
///
/// Без записей уровней нет (IT TASK); набор установки виден каждой организации; свой набор организации виден только
/// ей, заменяется и сбрасывается целиком, все поля и роли сохраняются как заданы (пустой набор — тоже набор); поле,
/// не заданное организацией, берётся с установки; `logo_in_use` после сброса последнего упоминания — `false`;
/// неверный набор — `Invalid` без изменений.
pub async fn branding(store: &dyn BrandingStore, org: TenantId, foreign: TenantId) {
    let (inst, own) = (BrandingScope::Installation, BrandingScope::Org(org));
    let (logo_a, logo_b) = (logo(0xa1), logo(0xb2));

    // Без записей — встроенный IT TASK.
    assert_eq!(
        store.branding_levels(None).await,
        Ok(BrandingLevels::default())
    );
    let empty = store.branding_levels(Some(org)).await.expect("levels");
    assert_eq!(empty, BrandingLevels::default());
    assert_eq!(empty.effective().palette, Palette::it_task());
    assert_eq!(store.branding(inst).await, Ok(None));
    assert_eq!(store.branding(own).await, Ok(None));
    assert_eq!(store.reset_branding(own).await, Ok(None), "сброс пустого");
    assert_eq!(store.logo_in_use(&logo_a.hash).await, Ok(false));

    // Неверный набор не записывается.
    let invalid = Branding {
        product_name: Some(String::new()),
        ..Branding::default()
    };
    assert!(matches!(
        store.set_branding(inst, &invalid).await,
        Err(Error::Invalid(_))
    ));
    let invalid = Branding {
        logo: Some(BrandLogo {
            width: 2048,
            ..logo_a
        }),
        ..Branding::default()
    };
    assert!(matches!(
        store.set_branding(own, &invalid).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        store.branding_levels(Some(org)).await,
        Ok(BrandingLevels::default())
    );

    // Набор установки виден всем организациям и экрану входа без организации.
    let mut installation = Branding {
        product_name: Some("Почта Холдинга".into()),
        primary: Some(color("#177CAA")),
        logo: Some(logo_a),
        ..Branding::default()
    };
    installation.light.insert(Role::Sel, color("#C0106B"));
    installation.cli.insert(CliRole::JsonKey, color("#2A8BC4"));
    store
        .set_branding(inst, &installation)
        .await
        .expect("set installation");
    assert_eq!(store.branding(inst).await, Ok(Some(installation.clone())));
    assert_eq!(store.branding(own).await, Ok(None), "без подстановки");
    for tenant in [None, Some(org), Some(foreign)] {
        assert_eq!(
            store.branding_levels(tenant).await,
            Ok(levels(Some(&installation), None)),
            "{tenant:?}"
        );
    }
    assert_eq!(store.logo_in_use(&logo_a.hash).await, Ok(true));
    assert_eq!(store.logo_in_use(&logo_b.hash).await, Ok(false));

    // Организация сменила только название: знак и цвета — с установки.
    let only_name = Branding {
        product_name: Some("Ромашка".into()),
        ..Branding::default()
    };
    store.set_branding(own, &only_name).await.expect("set org");
    let got = store.branding_levels(Some(org)).await.expect("levels");
    assert_eq!(got, levels(Some(&installation), Some(&only_name)));
    let effective = got.effective();
    assert_eq!(effective.product_name.as_deref(), Some("Ромашка"));
    assert_eq!(effective.logo, Some(logo_a));
    assert_eq!(
        effective.palette,
        levels(Some(&installation), None).effective().palette
    );
    assert_eq!(
        store.branding_levels(Some(foreign)).await,
        Ok(levels(Some(&installation), None)),
        "чужая организация не видит набор org"
    );
    assert_eq!(store.branding(BrandingScope::Org(foreign)).await, Ok(None));
    assert_eq!(
        store.branding_levels(None).await,
        Ok(levels(Some(&installation), None))
    );

    // Замена набора уровня целиком (прежнее название не остаётся); роли — как заданы; знак, общий с установкой,
    // упомянут обоими уровнями.
    let mut org_set = Branding {
        primary: Some(color("#2E7D32")),
        logo: Some(logo_a),
        ..Branding::default()
    };
    org_set.dark.insert(Role::Head, color("#2EBCBA"));
    org_set.dark.insert(Role::SelFg, color("#000000"));
    store
        .set_branding(own, &org_set)
        .await
        .expect("replace org");
    assert_eq!(store.branding(own).await, Ok(Some(org_set.clone())));
    let effective = store
        .branding_levels(Some(org))
        .await
        .expect("levels")
        .effective();
    assert_eq!(effective.product_name.as_deref(), Some("Почта Холдинга"));
    let installation_b = Branding {
        logo: Some(logo_b),
        ..installation.clone()
    };
    store
        .set_branding(inst, &installation_b)
        .await
        .expect("replace installation");
    assert_eq!(store.logo_in_use(&logo_a.hash).await, Ok(true), "знак org");
    assert_eq!(store.logo_in_use(&logo_b.hash).await, Ok(true));

    // Пустой набор организации — тоже набор: всё с установки.
    store
        .set_branding(BrandingScope::Org(foreign), &Branding::default())
        .await
        .expect("empty set");
    assert_eq!(
        store.branding_levels(Some(foreign)).await,
        Ok(levels(Some(&installation_b), Some(&Branding::default())))
    );
    assert_eq!(
        store.reset_branding(BrandingScope::Org(foreign)).await,
        Ok(Some(Branding::default()))
    );

    // Сброс организации возвращает прежний набор; дальше — только установка.
    assert_eq!(store.reset_branding(own).await, Ok(Some(org_set)));
    assert_eq!(store.branding(own).await, Ok(None));
    assert_eq!(
        store.branding_levels(Some(org)).await,
        Ok(levels(Some(&installation_b), None))
    );
    assert_eq!(
        store.logo_in_use(&logo_a.hash).await,
        Ok(false),
        "последнее упоминание сброшено"
    );

    // Сброс установки — снова встроенный IT TASK.
    assert_eq!(store.reset_branding(inst).await, Ok(Some(installation_b)));
    assert_eq!(store.logo_in_use(&logo_b.hash).await, Ok(false));
    for tenant in [None, Some(org), Some(foreign)] {
        assert_eq!(
            store.branding_levels(tenant).await,
            Ok(BrandingLevels::default()),
            "{tenant:?}"
        );
    }
}
