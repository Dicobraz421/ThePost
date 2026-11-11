//! Наборы проверок [`SieveStore`] и [`VacationLog`] (CTR-46): in-memory реализации и `PgMeta` (META-49, META-50)
//! проходят одни и те же проверки.

use std::time::Duration;

use time::OffsetDateTime;
use tp_contracts::bus::{EventBus, EventKind};
use tp_contracts::meta::AccountScope;
use tp_contracts::sieve::{ScriptInfo, SieveLimits, SieveStore, Vacation, VacationLog};
use tp_contracts::{AccountId, Error, TenantId};

use super::{Wait, next_signal};

/// Лимиты, с которыми хранилище проходит набор [`sieve`](super::sieve()) (у `PgMeta` — секция `meta` теста).
pub const LIMITS: SieveLimits = SieveLimits {
    max_scripts: 3,
    max_script_size: 1024,
};

/// Учётные записи набора: `owner`, `peer` — без скриптов, одной организации `owner.tenant`; `foreign` — та же учётная
/// запись в другой организации (разделение организаций).
#[derive(Debug, Clone, Copy)]
pub struct SieveFixture {
    /// Учётная запись, в которой работает набор.
    pub owner: AccountScope,
    /// Другая учётная запись той же организации.
    pub peer: AccountScope,
    /// `owner.account` в другой организации.
    pub foreign: AccountScope,
}

impl SieveFixture {
    /// Новые случайные учётные записи (in-memory).
    pub fn new() -> Self {
        let tenant = TenantId::new();
        let owner = AccountScope {
            tenant,
            account: AccountId::new(),
        };
        Self {
            owner,
            peer: AccountScope {
                tenant,
                account: AccountId::new(),
            },
            foreign: AccountScope {
                tenant: TenantId::new(),
                account: owner.account,
            },
        }
    }
}

impl Default for SieveFixture {
    fn default() -> Self {
        Self::new()
    }
}

fn info(name: &str, size: u64, active: bool) -> ScriptInfo {
    ScriptInfo {
        name: name.into(),
        size,
        active,
    }
}

/// Набор проверок [`SieveStore`]: список, замена, активный не больше одного, удаление активного — `Conflict`,
/// переименование (в том числе активного), лимиты числа и размера, скрипт организации, автоответ, состояние,
/// разделение учётных записей и организаций. Хранилище — с лимитами [`LIMITS`].
pub async fn sieve(store: &dyn SieveStore, fx: &SieveFixture) {
    let s = fx.owner;
    assert_eq!(store.limits(), LIMITS, "набор требует лимиты LIMITS");
    assert_eq!(store.state(s).await.expect("state"), 0);
    assert!(store.list(s).await.expect("list").is_empty());
    assert_eq!(store.active(s).await.expect("active"), None);

    let main = "require \"fileinto\";\r\nfileinto \"Работа\";\r\n";
    let s1 = store.put(s, "main", main.into()).await.expect("put");
    let s2 = store.put(s, "Отпуск", "keep;".into()).await.expect("put");
    assert!(s1 > 0 && s2 > s1);
    assert!(matches!(
        store.put(s, "", "keep;".into()).await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        store.put(s, "a\nb", "keep;".into()).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        store
            .put(s, "big", "#".repeat(LIMITS.max_script_size + 1))
            .await,
        Err(Error::QuotaExceeded)
    );
    let s3 = store.put(s, "spare", "stop;".into()).await.expect("put");
    assert_eq!(
        store.put(s, "fourth", "keep;".into()).await,
        Err(Error::QuotaExceeded),
        "скриптов не больше max_scripts"
    );
    // Замена существующего — не новый скрипт.
    let s4 = store
        .put(s, "spare", "discard;".into())
        .await
        .expect("replace");
    assert!(s4 > s3);
    assert_eq!(
        store.get(s, "spare").await.expect("get").as_deref(),
        Some("discard;")
    );
    assert_eq!(store.get(s, "none").await.expect("get"), None);
    assert_eq!(
        store.list(s).await.expect("list"),
        [
            info("main", main.len() as u64, false),
            info("spare", 8, false),
            info("Отпуск", 5, false),
        ],
        "по имени побайтно"
    );

    // Активный не больше одного.
    assert_eq!(store.activate(s, Some("none")).await, Err(Error::NotFound));
    let s5 = store.activate(s, Some("main")).await.expect("activate");
    assert!(s5 > s4);
    let active = store.active(s).await.expect("active").expect("main");
    assert_eq!((active.name.as_str(), active.text.as_str()), ("main", main));
    store.activate(s, Some("spare")).await.expect("activate");
    let flags: Vec<(String, bool)> = store
        .list(s)
        .await
        .expect("list")
        .into_iter()
        .map(|i| (i.name, i.active))
        .collect();
    assert_eq!(
        flags,
        [
            ("main".to_owned(), false),
            ("spare".to_owned(), true),
            ("Отпуск".to_owned(), false)
        ]
    );
    // Замена текста сохраняет активность.
    store
        .put(s, "spare", "keep;".into())
        .await
        .expect("replace");
    assert_eq!(
        store.active(s).await.expect("active").map(|a| a.text),
        Some("keep;".into())
    );

    // Удаление: активный — Conflict.
    assert_eq!(store.delete(s, "spare").await, Err(Error::Conflict));
    assert_eq!(store.delete(s, "none").await, Err(Error::NotFound));
    let s6 = store.delete(s, "main").await.expect("delete");
    assert_eq!(store.get(s, "main").await.expect("get"), None);

    // Переименование.
    assert_eq!(store.rename(s, "none", "x").await, Err(Error::NotFound));
    assert_eq!(
        store.rename(s, "spare", "Отпуск").await,
        Err(Error::Conflict)
    );
    assert!(matches!(
        store.rename(s, "spare", "").await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        store.rename(s, "spare", "spare").await,
        Ok(s6),
        "то же имя — без изменений"
    );
    let s7 = store.rename(s, "spare", "rules").await.expect("rename");
    assert!(s7 > s6);
    let active = store.active(s).await.expect("active").expect("rules");
    assert_eq!(
        (active.name.as_str(), active.text.as_str()),
        ("rules", "keep;"),
        "активность переходит к новому имени"
    );
    assert_eq!(store.get(s, "spare").await.expect("get"), None);
    let s8 = store.activate(s, None).await.expect("deactivate");
    assert!(s8 > s7);
    assert_eq!(store.active(s).await.expect("active"), None);
    assert!(store.list(s).await.expect("list").iter().all(|i| !i.active));
    store.delete(s, "rules").await.expect("delete inactive");

    // Автоответ.
    assert_eq!(
        store.vacation(s).await.expect("vacation"),
        Vacation::default()
    );
    let from = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
    let vacation = Vacation {
        enabled: true,
        from: Some(from),
        to: Some(from + time::Duration::days(14)),
        subject: Some("В отпуске".into()),
        text_body: Some("Вернусь через две недели.".into()),
        html_body: Some("<p>Вернусь через две недели.</p>".into()),
    };
    let s9 = store
        .set_vacation(s, vacation.clone())
        .await
        .expect("set_vacation");
    assert!(s9 > s8);
    assert_eq!(store.vacation(s).await.expect("vacation"), vacation);
    assert!(matches!(
        store
            .set_vacation(
                s,
                Vacation {
                    from: vacation.to,
                    to: vacation.from,
                    ..vacation.clone()
                }
            )
            .await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(store.state(s).await.expect("state"), s9);

    // Скрипт организации.
    assert_eq!(store.org_script(s.tenant).await.expect("org"), None);
    store
        .set_org_script(s.tenant, Some("keep;".into()))
        .await
        .expect("set org");
    assert_eq!(
        store.org_script(s.tenant).await.expect("org").as_deref(),
        Some("keep;")
    );
    assert_eq!(
        store
            .set_org_script(s.tenant, Some("#".repeat(LIMITS.max_script_size + 1)))
            .await,
        Err(Error::QuotaExceeded)
    );
    assert_eq!(
        store.org_script(fx.foreign.tenant).await.expect("org"),
        None
    );
    store
        .set_org_script(s.tenant, None)
        .await
        .expect("remove org");
    assert_eq!(store.org_script(s.tenant).await.expect("org"), None);

    // Разделение учётных записей и организаций.
    store.put(s, "main", main.into()).await.expect("put");
    store.activate(s, Some("main")).await.expect("activate");
    for other in [fx.peer, fx.foreign] {
        assert!(
            store.list(other).await.expect("list").is_empty(),
            "{other:?}"
        );
        assert_eq!(store.get(other, "main").await.expect("get"), None);
        assert_eq!(store.active(other).await.expect("active"), None);
        assert_eq!(store.delete(other, "main").await, Err(Error::NotFound));
        assert_eq!(
            store.activate(other, Some("main")).await,
            Err(Error::NotFound)
        );
        assert_eq!(
            store.vacation(other).await.expect("vacation"),
            Vacation::default()
        );
        assert_eq!(store.state(other).await.expect("state"), 0);
    }
    assert!(store.active(s).await.expect("active").is_some());
}

/// Набор проверок [`VacationLog`] (RFC 5230 `:days`): первый ответ отмечается, повтор в периоде — `true`, после
/// периода — снова ответ; отдельно по отправителю, `handle` и учётной записи. `wait` — часы проверяемой реализации
/// (для PostgreSQL — сон: период выбран в секундах).
pub async fn vacation_log(
    log: &dyn VacationLog,
    owner: AccountScope,
    peer: AccountScope,
    wait: &dyn Wait,
) {
    let period = Duration::from_secs(2);
    let sender = "boss@example.net";
    assert!(
        !log.seen(owner, sender, "", period).await.expect("seen"),
        "первый ответ"
    );
    assert!(
        log.seen(owner, sender, "", period).await.expect("seen"),
        "повтор в периоде"
    );
    assert!(
        !log.seen(owner, "other@example.net", "", period)
            .await
            .expect("seen"),
        "другой отправитель"
    );
    assert!(
        !log.seen(owner, sender, "trip", period).await.expect("seen"),
        "другой :handle"
    );
    assert!(
        !log.seen(peer, sender, "", period).await.expect("seen"),
        "другая учётная запись"
    );
    wait.wait(period + Duration::from_millis(500)).await;
    assert!(
        !log.seen(owner, sender, "", period).await.expect("seen"),
        "период истёк"
    );
    assert!(log.seen(owner, sender, "", period).await.expect("seen"));
    // Длинный период: отметка сохранилась.
    assert!(
        log.seen(owner, sender, "trip", Duration::from_secs(3600))
            .await
            .expect("seen")
    );
}

/// Набор проверок сигналов [`SieveStore`] (CTR-46): `put`, `activate`, `rename`, `delete`, `set_vacation` публикуют
/// `SieveChanged` подписке учётной записи `scope`. `scope` — без скриптов.
pub async fn signals(store: &dyn SieveStore, bus: &dyn EventBus, scope: AccountScope) {
    let mut events = bus.subscribe(scope).await.expect("subscribe");
    store.put(scope, "a", "keep;".into()).await.expect("put");
    store.activate(scope, Some("a")).await.expect("activate");
    store.put(scope, "b", "keep;".into()).await.expect("put");
    store
        .set_org_script(scope.tenant, Some("keep;".into()))
        .await
        .expect("org");
    store.rename(scope, "b", "c").await.expect("rename");
    store.delete(scope, "c").await.expect("delete");
    store
        .set_vacation(
            scope,
            Vacation {
                enabled: true,
                ..Vacation::default()
            },
        )
        .await
        .expect("vacation");
    for _ in 0..6 {
        let e = next_signal(&mut events).await.expect("сигнал");
        assert_eq!(
            (e.tenant, e.account, e.kind),
            (scope.tenant, Some(scope.account), EventKind::SieveChanged)
        );
    }
}
