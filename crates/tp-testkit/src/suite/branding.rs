//! Набор проверок [`BrandingStore`] (CTR-91, CONTRACTS.md#Branding): in-memory `MemBranding` и `PgMeta` (META-84)
//! проходят одни и те же проверки.

use tp_contracts::branding::{BrandLogo, Branding, BrandingScope, BrandingStore};
use tp_contracts::{BlobHash, Error, TenantId};

fn logo(byte: u8) -> BrandLogo {
    BrandLogo {
        hash: BlobHash([byte; 32]),
        size: 4096,
        width: 256,
        height: 128,
    }
}

fn set(name: &str, primary: &str, accent: &str, logo: Option<BrandLogo>) -> Branding {
    Branding {
        product_name: name.into(),
        primary: primary.parse().expect("primary"),
        accent: accent.parse().expect("accent"),
        logo,
    }
}

/// Набор проверок брендинга по уровням. Хранилище в начале пустое; `org` и `foreign` — разные организации.
///
/// Без записей действующего брендинга нет; уровень установки виден организации без своего набора; свой набор
/// организации заменяет установку целиком (без знака — без знака, а не знак установки); сброс организации
/// возвращает установку; чужая организация своего набора не видит; `logo_in_use` после сброса последнего
/// упоминания — `false`; неверный набор — `Invalid` без изменений.
pub async fn branding(store: &dyn BrandingStore, org: TenantId, foreign: TenantId) {
    let (inst, own) = (BrandingScope::Installation, BrandingScope::Org(org));
    let (logo_a, logo_b) = (logo(0xa1), logo(0xb2));

    // Без записей — встроенный IT TASK.
    assert_eq!(store.effective_branding(None).await, Ok(None));
    assert_eq!(store.effective_branding(Some(org)).await, Ok(None));
    assert_eq!(store.branding(inst).await, Ok(None));
    assert_eq!(store.branding(own).await, Ok(None));
    assert_eq!(store.reset_branding(own).await, Ok(None), "сброс пустого");
    assert_eq!(store.logo_in_use(&logo_a.hash).await, Ok(false));

    // Неверный набор не записывается.
    let invalid = set("", "#177CAA", "#E4107F", None);
    assert!(matches!(
        store.set_branding(inst, &invalid).await,
        Err(Error::Invalid(_))
    ));
    let big_logo = BrandLogo {
        width: 2048,
        ..logo_a
    };
    let invalid = set("Ромашка", "#177CAA", "#E4107F", Some(big_logo));
    assert!(matches!(
        store.set_branding(own, &invalid).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(store.effective_branding(Some(org)).await, Ok(None));

    // Уровень установки виден всем организациям без своего набора и экрану входа без организации.
    let installation = set("Почта Холдинга", "#177CAA", "#E4107F", Some(logo_a));
    store
        .set_branding(inst, &installation)
        .await
        .expect("set installation");
    assert_eq!(store.branding(inst).await, Ok(Some(installation.clone())));
    assert_eq!(store.branding(own).await, Ok(None), "без подстановки");
    for tenant in [None, Some(org), Some(foreign)] {
        assert_eq!(
            store.effective_branding(tenant).await,
            Ok(Some(installation.clone())),
            "{tenant:?}"
        );
    }
    assert_eq!(store.logo_in_use(&logo_a.hash).await, Ok(true));
    assert_eq!(store.logo_in_use(&logo_b.hash).await, Ok(false));

    // Свой набор организации заменяет установку целиком: без знака — без знака.
    let org_set = set("Ромашка", "#2E7D32", "#FFB300", None);
    store.set_branding(own, &org_set).await.expect("set org");
    assert_eq!(store.branding(own).await, Ok(Some(org_set.clone())));
    assert_eq!(
        store.effective_branding(Some(org)).await,
        Ok(Some(org_set.clone()))
    );
    assert_eq!(
        store.effective_branding(Some(foreign)).await,
        Ok(Some(installation.clone())),
        "чужая организация не видит набор org"
    );
    assert_eq!(store.branding(BrandingScope::Org(foreign)).await, Ok(None));
    assert_eq!(
        store.effective_branding(None).await,
        Ok(Some(installation.clone()))
    );

    // Замена набора уровня целиком; знак, общий с установкой, упомянут обоими уровнями.
    let org_set = set("Ромашка Плюс", "#2E7D32", "#FFB300", Some(logo_a));
    store
        .set_branding(own, &org_set)
        .await
        .expect("replace org");
    assert_eq!(
        store.effective_branding(Some(org)).await,
        Ok(Some(org_set.clone()))
    );
    let installation_b = set("Почта Холдинга", "#177CAA", "#E4107F", Some(logo_b));
    store
        .set_branding(inst, &installation_b)
        .await
        .expect("replace installation");
    assert_eq!(store.logo_in_use(&logo_a.hash).await, Ok(true), "знак org");
    assert_eq!(store.logo_in_use(&logo_b.hash).await, Ok(true));

    // Сброс организации возвращает прежний набор и брендинг установки.
    assert_eq!(store.reset_branding(own).await, Ok(Some(org_set)));
    assert_eq!(store.branding(own).await, Ok(None));
    assert_eq!(
        store.effective_branding(Some(org)).await,
        Ok(Some(installation_b.clone()))
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
            store.effective_branding(tenant).await,
            Ok(None),
            "{tenant:?}"
        );
    }
}
