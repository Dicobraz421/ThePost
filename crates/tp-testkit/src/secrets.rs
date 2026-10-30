//! In-memory хранилище секретов (CONTRACTS.md#Secrets; решение 2026-10-30).
//!
//! [`MemSecretStore`] заменяет OpenBao/Vault в тестах полос: секреты задаёт тест ([`MemSecretStore::set`],
//! [`MemSecretStore::set_dynamic`]), аренды динамических секретов истекают по [`Clock`] и отзываются
//! ([`MemSecretStore::revoke`]), политика запрещает пути ([`MemSecretStore::forbid`]), недоступность —
//! [`MemSecretStore::seal`] и [`MemSecretStore::fail_next`].

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::secrets::{Secret, SecretLease, SecretRef, SecretStore};
use tp_contracts::{Error, Result};

use crate::infra::{Clock, SystemClock, add, lock};

/// Поле секрета: статическое значение или динамическое (каждое чтение выдаёт новую аренду).
#[derive(Clone)]
enum Value {
    Static(String),
    Dynamic {
        value: String,
        ttl: Duration,
        renewable: bool,
    },
}

#[derive(Debug, Clone)]
struct ActiveLease {
    ttl: Duration,
    renewable: bool,
    expires_at: OffsetDateTime,
}

#[derive(Default)]
struct State {
    /// Путь → поле → значение.
    secrets: BTreeMap<String, BTreeMap<String, Value>>,
    forbidden: BTreeSet<String>,
    leases: HashMap<String, ActiveLease>,
    /// Номер последней выданной аренды (идентификаторы не повторяются).
    issued: u64,
    sealed: bool,
    faults: VecDeque<Error>,
    gets: u64,
    renews: u64,
}

impl State {
    /// Ошибка недоступности: запечатано или очередная инъецированная ошибка.
    fn fault(&mut self) -> Result<()> {
        if self.sealed {
            return Err(Error::UNAVAILABLE);
        }
        match self.faults.pop_front() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

/// Хранилище секретов в памяти; клоны разделяют данные. Значения в `Debug` не выводятся.
#[derive(Clone)]
pub struct MemSecretStore {
    state: Arc<Mutex<State>>,
    clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for MemSecretStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let st = lock(&self.state);
        f.debug_struct("MemSecretStore")
            .field("paths", &st.secrets.keys().collect::<Vec<_>>())
            .field("leases", &st.leases.len())
            .field("sealed", &st.sealed)
            .finish()
    }
}

impl MemSecretStore {
    /// Пустое хранилище с часами `clock` (истечение аренд).
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            state: Arc::new(Mutex::new(State::default())),
            clock,
        }
    }

    /// Задаёт статическое поле секрета (замена значения — ротация: следующее `get` вернёт новое).
    pub fn set(&self, secret: &SecretRef, value: impl Into<String>) {
        self.put(secret, Value::Static(value.into()));
    }

    /// Задаёт динамическое поле: каждое `get` выдаёт новую аренду на `ttl` (как database secrets engine).
    pub fn set_dynamic(
        &self,
        secret: &SecretRef,
        value: impl Into<String>,
        ttl: Duration,
        renewable: bool,
    ) {
        self.put(
            secret,
            Value::Dynamic {
                value: value.into(),
                ttl,
                renewable,
            },
        );
    }

    fn put(&self, secret: &SecretRef, value: Value) {
        lock(&self.state)
            .secrets
            .entry(secret.path().to_owned())
            .or_default()
            .insert(secret.field().to_owned(), value);
    }

    /// Удаляет поле секрета (путь без полей удаляется). Выданные аренды остаются до истечения или отзыва.
    pub fn remove(&self, secret: &SecretRef) {
        let mut st = lock(&self.state);
        if let Some(fields) = st.secrets.get_mut(secret.path()) {
            fields.remove(secret.field());
            if fields.is_empty() {
                st.secrets.remove(secret.path());
            }
        }
    }

    /// Запрещает политикой путь `path` и всё под ним (`<path>/…`): `get` — `Forbidden`.
    pub fn forbid(&self, path: &str) {
        lock(&self.state).forbidden.insert(path.to_owned());
    }

    /// Снимает запрет [`MemSecretStore::forbid`].
    pub fn allow(&self, path: &str) {
        lock(&self.state).forbidden.remove(path);
    }

    /// Отзывает аренду: продление — `NotFound`. Неизвестная аренда — без эффекта.
    pub fn revoke(&self, lease_id: &str) {
        lock(&self.state).leases.remove(lease_id);
    }

    /// Аренда выдана, не отозвана и не истекла.
    pub fn lease_active(&self, lease_id: &str) -> bool {
        let now = self.clock.now();
        lock(&self.state)
            .leases
            .get(lease_id)
            .is_some_and(|l| l.expires_at > now)
    }

    /// Запечатывает хранилище: все операции — `Unavailable`, пока не [`MemSecretStore::unseal`].
    /// Аренды при этом продолжают истекать по часам.
    pub fn seal(&self) {
        lock(&self.state).sealed = true;
    }

    /// Распечатывает хранилище.
    pub fn unseal(&self) {
        lock(&self.state).sealed = false;
    }

    /// Следующая операция (`get` или `renew`) вернёт `err` (очередь, по одной ошибке на операцию).
    pub fn fail_next(&self, err: Error) {
        lock(&self.state).faults.push_back(err);
    }

    /// Следующие `n` операций вернут `err`.
    pub fn fail_times(&self, n: usize, err: Error) {
        let mut st = lock(&self.state);
        st.faults.extend(std::iter::repeat_n(err, n));
    }

    /// Снимает инъецированные ошибки и распечатывает хранилище.
    pub fn clear_faults(&self) {
        let mut st = lock(&self.state);
        st.faults.clear();
        st.sealed = false;
    }

    /// Число вызовов `get` (включая неудачные).
    pub fn gets(&self) -> u64 {
        lock(&self.state).gets
    }

    /// Число вызовов `renew` (включая неудачные).
    pub fn renews(&self) -> u64 {
        lock(&self.state).renews
    }
}

impl Default for MemSecretStore {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock))
    }
}

#[async_trait]
impl SecretStore for MemSecretStore {
    async fn get(&self, secret: &SecretRef) -> Result<Secret> {
        let now = self.clock.now();
        let mut st = lock(&self.state);
        st.gets += 1;
        st.fault()?;
        let path = secret.path();
        let denied = st.forbidden.iter().any(|p| {
            path.strip_prefix(p.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        });
        if denied {
            return Err(Error::Forbidden);
        }
        let value = st
            .secrets
            .get(path)
            .and_then(|fields| fields.get(secret.field()))
            .cloned()
            .ok_or(Error::NotFound)?;
        match value {
            Value::Static(v) => Ok(Secret::new(v)),
            Value::Dynamic {
                value,
                ttl,
                renewable,
            } => {
                st.issued += 1;
                let id = format!("{path}/{}", st.issued);
                st.leases.insert(
                    id.clone(),
                    ActiveLease {
                        ttl,
                        renewable,
                        expires_at: add(now, ttl),
                    },
                );
                Ok(Secret::new(value).with_lease(SecretLease { id, ttl, renewable }))
            }
        }
    }

    async fn renew(&self, lease: &SecretLease) -> Result<SecretLease> {
        let now = self.clock.now();
        let mut st = lock(&self.state);
        st.renews += 1;
        st.fault()?;
        let Some(cur) = st.leases.get_mut(&lease.id) else {
            return Err(Error::NotFound);
        };
        if cur.expires_at <= now {
            st.leases.remove(&lease.id);
            return Err(Error::NotFound);
        }
        if !cur.renewable {
            return Err(Error::NotFound);
        }
        cur.expires_at = add(now, cur.ttl);
        Ok(SecretLease {
            id: lease.id.clone(),
            ttl: cur.ttl,
            renewable: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ManualClock;
    use crate::suite::{SecretFixture, secret_store};

    const TTL: Duration = Duration::from_secs(60);

    fn r(s: &str) -> SecretRef {
        s.parse().expect("ref")
    }

    #[tokio::test]
    async fn passes_contract_suite() {
        let clock = ManualClock::default();
        let store = MemSecretStore::new(clock.shared());
        store.set(&r("thepost/db#password"), "pw");
        store.set(&r("thepost/admin#token"), "root");
        store.forbid("thepost/admin");
        store.set_dynamic(&r("database/creds/thepost#password"), "dyn", TTL, true);
        let fixture = SecretFixture {
            present: (r("thepost/db#password"), "pw".into()),
            missing_field: r("thepost/db#nope"),
            missing_path: r("thepost/none#password"),
            forbidden: Some(r("thepost/admin#token")),
            dynamic: Some(r("database/creds/thepost#password")),
        };
        secret_store(&store, &fixture, &clock).await;
    }

    #[tokio::test]
    async fn lease_expires_by_clock_and_renew_extends() {
        let clock = ManualClock::default();
        let store = MemSecretStore::new(clock.shared());
        let dynamic = r("database/creds/thepost#password");
        store.set_dynamic(&dynamic, "dyn", TTL, true);
        let lease = store.get(&dynamic).await.expect("get").lease().cloned();
        let lease = lease.expect("lease");
        assert_eq!(lease.ttl, TTL);

        clock.advance(TTL - Duration::from_secs(1));
        assert!(store.lease_active(&lease.id));
        assert_eq!(store.renew(&lease).await.expect("renew"), lease);
        // Продление считает срок от текущего момента.
        clock.advance(TTL - Duration::from_secs(1));
        assert!(store.lease_active(&lease.id));
        clock.advance(Duration::from_secs(1));
        assert!(!store.lease_active(&lease.id));
        assert_eq!(store.renew(&lease).await, Err(Error::NotFound));

        // Каждое чтение — новая аренда с новым идентификатором.
        let a = store.get(&dynamic).await.expect("get");
        let b = store.get(&dynamic).await.expect("get");
        assert_ne!(a.lease().map(|l| &l.id), b.lease().map(|l| &l.id));
    }

    #[tokio::test]
    async fn revoke_and_not_renewable() {
        let store = MemSecretStore::default();
        let dynamic = r("database/creds/ro#password");
        store.set_dynamic(&dynamic, "dyn", TTL, false);
        let s = store.get(&dynamic).await.expect("get");
        let lease = s.lease().cloned().expect("lease");
        assert!(!lease.renewable);
        assert_eq!(store.renew(&lease).await, Err(Error::NotFound));
        assert!(store.lease_active(&lease.id));
        store.revoke(&lease.id);
        assert!(!store.lease_active(&lease.id));
        store.revoke("unknown");
    }

    #[tokio::test]
    async fn unavailable_injection() {
        let store = MemSecretStore::default();
        let db = r("thepost/db#password");
        store.set(&db, "pw");
        store.set_dynamic(&r("database/creds/thepost#password"), "d", TTL, true);
        let lease = store
            .get(&r("database/creds/thepost#password"))
            .await
            .expect("get")
            .lease()
            .cloned()
            .expect("lease");

        store.seal();
        assert!(matches!(
            store.get(&db).await,
            Err(Error::Unavailable { .. })
        ));
        assert!(matches!(
            store.renew(&lease).await,
            Err(Error::Unavailable { .. })
        ));
        store.unseal();
        assert_eq!(store.get(&db).await.expect("pw").expose(), "pw");

        store.fail_times(2, Error::UNAVAILABLE);
        assert!(matches!(
            store.get(&db).await,
            Err(Error::Unavailable { .. })
        ));
        assert!(matches!(
            store.renew(&lease).await,
            Err(Error::Unavailable { .. })
        ));
        assert!(store.renew(&lease).await.is_ok());
        store.fail_next(Error::UNAVAILABLE);
        store.clear_faults();
        assert!(store.get(&db).await.is_ok());
        assert_eq!((store.gets(), store.renews()), (5, 3));
    }

    #[tokio::test]
    async fn rotation_remove_and_policy() {
        let store = MemSecretStore::default();
        let db = r("thepost/db#password");
        store.set(&db, "old");
        store.set(&r("thepost/db#user"), "thepost");
        store.set(&r("thepost/dbx#password"), "x");
        store.set(&db, "new");
        assert_eq!(store.get(&db).await.expect("get").expose(), "new");

        // Запрет пути действует на него и вложенные пути, но не на пути с тем же началом имени.
        store.forbid("thepost/db");
        assert_eq!(store.get(&db).await.map(|_| ()), Err(Error::Forbidden));
        assert!(store.get(&r("thepost/dbx#password")).await.is_ok());
        store.forbid("thepost");
        assert_eq!(
            store.get(&r("thepost/dbx#password")).await.map(|_| ()),
            Err(Error::Forbidden)
        );
        store.allow("thepost");
        store.allow("thepost/db");

        store.remove(&db);
        assert_eq!(store.get(&db).await.map(|_| ()), Err(Error::NotFound));
        assert_eq!(
            store
                .get(&r("thepost/db#user"))
                .await
                .expect("user")
                .expose(),
            "thepost"
        );
        store.remove(&r("thepost/db#user"));
        store.remove(&r("thepost/none#x"));
        let shown = format!("{store:?}");
        assert!(
            !shown.contains("thepost/db\"") && !shown.contains("new"),
            "{shown}"
        );
        assert!(shown.contains("thepost/dbx"), "{shown}");
    }
}
