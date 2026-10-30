//! Общие наборы тестов контрактов: in-memory реализации testkit и настоящие реализации полос
//! (`tp-meta` — `Membership`, `tp-platform` — `SecretStore`) проходят одни и те же проверки.
//!
//! Время в наборах идёт через [`Wait`]: для testkit — [`ManualClock`] (сдвиг без ожидания), для PostgreSQL
//! и OpenBao — [`RealTime`] (сон), тогда сроки выбирают с запасом на задержки.
//!
//! ```
//! # tokio_test_block_on(async {
//! use std::time::Duration;
//! use tp_testkit::{ManualClock, MemMembership, suite};
//!
//! let clock = ManualClock::default();
//! suite::membership(&MemMembership::new(clock.shared()), Duration::from_secs(30), &clock).await;
//! # });
//! # fn tokio_test_block_on(f: impl std::future::Future<Output = ()>) {
//! #     tokio::runtime::Builder::new_current_thread().enable_all().build().expect("rt").block_on(f)
//! # }
//! ```

use std::time::Duration;

use async_trait::async_trait;
use tp_contracts::Error;
use tp_contracts::TenantId;
use tp_contracts::membership::{Membership, NodeId};
use tp_contracts::secrets::{SecretLease, SecretRef, SecretStore};

use crate::ManualClock;

/// Ход времени для наборов тестов.
#[async_trait]
pub trait Wait: Send + Sync {
    /// Дождаться, пока пройдёт `d` по часам проверяемого хранилища.
    async fn wait(&self, d: Duration);
}

#[async_trait]
impl Wait for ManualClock {
    async fn wait(&self, d: Duration) {
        self.advance(d);
    }
}

/// Настоящее время: `tokio::time::sleep`.
#[derive(Debug, Clone, Copy, Default)]
pub struct RealTime;

#[async_trait]
impl Wait for RealTime {
    async fn wait(&self, d: Duration) {
        tokio::time::sleep(d).await;
    }
}

// ---------- Membership ----------

/// Набор проверок `Membership`. `ttl` — срок записи узла; с [`RealTime`] — с запасом на задержки
/// хранилища (секунды). Набор не требует пустого состава: узлы набора — с уникальным префиксом,
/// чужие записи в `live()` не мешают.
pub async fn membership(m: &dyn Membership, ttl: Duration, wait: &dyn Wait) {
    let prefix = format!("suite-{}", TenantId::new());
    let node = |n: &str| NodeId::new(format!("{prefix}-{n}")).expect("node id");
    let (a, b, c) = (node("a"), node("b"), node("c"));
    let ours = || async {
        let live = m.live().await.expect("live");
        let mut sorted = live.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(live, sorted, "live(): по возрастанию без повторов");
        live.into_iter()
            .filter(|n| n.as_str().starts_with(&prefix))
            .collect::<Vec<_>>()
    };

    assert!(ours().await.is_empty(), "до heartbeat узлов набора нет");

    // Создание записи и повторное продление без повторов в составе.
    m.heartbeat(&c, ttl).await.expect("heartbeat c");
    m.heartbeat(&a, ttl).await.expect("heartbeat a");
    m.heartbeat(&b, ttl).await.expect("heartbeat b");
    m.heartbeat(&a, ttl).await.expect("heartbeat a again");
    assert_eq!(ours().await, vec![a.clone(), b.clone(), c.clone()]);

    // Выход — сразу, без ожидания срока; повторный выход и выход отсутствующего — не ошибка.
    m.leave(&c).await.expect("leave c");
    m.leave(&c).await.expect("leave c again");
    m.leave(&node("never")).await.expect("leave absent");
    assert_eq!(ours().await, vec![a.clone(), b.clone()]);

    // Продление переносит срок: a продлевается каждые ttl/2, b — нет.
    let half = ttl / 2;
    wait.wait(half).await;
    m.heartbeat(&a, ttl).await.expect("renew a");
    wait.wait(half + ttl / 4).await;
    assert_eq!(ours().await, vec![a.clone()], "b истёк, a продлён");

    // Без продления истекает и a; продление после истечения возвращает узел.
    wait.wait(ttl).await;
    assert!(ours().await.is_empty(), "все записи набора истекли");
    m.heartbeat(&b, ttl)
        .await
        .expect("heartbeat b after expiry");
    assert_eq!(ours().await, vec![b.clone()]);
    m.leave(&b).await.expect("leave b");
    assert!(ours().await.is_empty());
}

// ---------- SecretStore ----------

/// Данные хранилища для [`secret_store`]; хранилище заполняет тест до запуска набора.
#[derive(Debug, Clone)]
pub struct SecretFixture {
    /// Статический секрет и его значение.
    pub present: (SecretRef, String),
    /// Поле, которого нет у существующего пути.
    pub missing_field: SecretRef,
    /// Несуществующий путь.
    pub missing_path: SecretRef,
    /// Ссылка, запрещённая политикой, если хранилище умеет политики.
    pub forbidden: Option<SecretRef>,
    /// Динамический секрет с продлеваемой арендой (срок — короткий для [`RealTime`]), если есть.
    pub dynamic: Option<SecretRef>,
}

/// Набор проверок `SecretStore` на подготовленных данных `fixture`.
pub async fn secret_store(store: &dyn SecretStore, fixture: &SecretFixture, wait: &dyn Wait) {
    let (present, value) = &fixture.present;
    let s = store.get(present).await.expect("present secret");
    assert_eq!(s.expose(), value);
    assert!(s.lease().is_none(), "статический секрет — без аренды");
    let shown = format!("{s:?} {s}");
    assert!(!shown.contains(value.as_str()), "значение скрыто: {shown}");

    for missing in [&fixture.missing_field, &fixture.missing_path] {
        assert_eq!(
            store.get(missing).await.map(|_| ()),
            Err(Error::NotFound),
            "{missing}"
        );
    }
    if let Some(forbidden) = &fixture.forbidden {
        assert_eq!(
            store.get(forbidden).await.map(|_| ()),
            Err(Error::Forbidden),
            "{forbidden}"
        );
    }

    let unknown = SecretLease {
        id: format!("suite/unknown/{}", TenantId::new()),
        ttl: Duration::from_secs(60),
        renewable: true,
    };
    assert_eq!(store.renew(&unknown).await, Err(Error::NotFound));

    let Some(dynamic) = &fixture.dynamic else {
        return;
    };
    let s = store.get(dynamic).await.expect("dynamic secret");
    let lease = s.lease().cloned().expect("динамический секрет — с арендой");
    assert!(lease.renewable && !lease.ttl.is_zero(), "{lease:?}");
    assert_eq!(s.lease_ttl(), Some(lease.ttl));

    // Продление до истечения переносит срок от текущего момента.
    wait.wait(lease.ttl / 2).await;
    let renewed = store.renew(&lease).await.expect("renew");
    assert_eq!(renewed.id, lease.id);
    assert!(renewed.renewable && !renewed.ttl.is_zero(), "{renewed:?}");
    wait.wait(renewed.ttl * 3 / 4).await;
    let renewed = store
        .renew(&renewed)
        .await
        .expect("renew after first renew");

    // Истёкшая аренда — `NotFound`, секрет читается заново с новой арендой.
    wait.wait(renewed.ttl + renewed.ttl / 4).await;
    assert_eq!(store.renew(&renewed).await, Err(Error::NotFound));
    let again = store.get(dynamic).await.expect("dynamic again");
    assert!(again.lease().is_some_and(|l| l.id != lease.id));
}
