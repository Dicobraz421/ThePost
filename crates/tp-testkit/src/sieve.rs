//! [`SieveStore`], [`VacationLog`] в памяти и фейк [`DeliveryRules`] для тестов `tp-smtp` (CONTRACTS.md#Sieve, CTR-46).
//!
//! С шиной ([`MemSieve::with_bus`]) после каждого изменения скриптов и автоответа публикуется `SieveChanged`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::bus::EventBus;
use tp_contracts::meta::AccountScope;
use tp_contracts::sieve::{
    ActiveScript, DeliveryRules, RulesInput, RulesOutcome, ScriptInfo, SieveLimits, SieveStore,
    Vacation, VacationLog, sieve_changed, valid_script_name,
};
use tp_contracts::{Error, Result, TenantId};

use crate::infra::{Clock, SystemClock, lock};

#[derive(Debug, Default)]
struct Account {
    scripts: BTreeMap<String, String>,
    active: Option<String>,
    vacation: Vacation,
    state: u64,
}

impl Account {
    fn bump(&mut self) -> u64 {
        self.state += 1;
        self.state
    }
}

#[derive(Debug, Default)]
struct State {
    accounts: HashMap<AccountScope, Account>,
    org: HashMap<TenantId, String>,
}

/// Скрипты Sieve и автоответы в памяти. Клоны разделяют данные.
#[derive(Clone, Default)]
pub struct MemSieve {
    state: Arc<Mutex<State>>,
    limits: SieveLimits,
    bus: Option<Arc<dyn EventBus>>,
}

impl fmt::Debug for MemSieve {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemSieve")
            .field("state", &self.state)
            .field("limits", &self.limits)
            .field("bus", &self.bus.is_some())
            .finish()
    }
}

impl MemSieve {
    /// Пустое хранилище с лимитами по умолчанию.
    pub fn new() -> Self {
        Self::default()
    }

    /// Лимиты `limits`.
    pub fn with_limits(mut self, limits: SieveLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Публиковать `SieveChanged` в `bus` после каждого изменения скриптов и автоответа.
    pub fn with_bus(mut self, bus: Arc<dyn EventBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Изменение учётной записи под блокировкой, затем сигнал.
    async fn change(
        &self,
        scope: AccountScope,
        f: impl FnOnce(&mut Account) -> Result<bool>,
    ) -> Result<u64> {
        let (state, changed) = {
            let mut st = lock(&self.state);
            let acc = st.accounts.entry(scope).or_default();
            let changed = f(acc)?;
            (if changed { acc.bump() } else { acc.state }, changed)
        };
        if changed && let Some(bus) = &self.bus {
            // Сигнал после «фиксации»; доставка at-most-once, ошибка шины не отменяет изменение.
            let _ = bus.publish(sieve_changed(scope)).await;
        }
        Ok(state)
    }
}

#[async_trait]
impl SieveStore for MemSieve {
    fn limits(&self) -> SieveLimits {
        self.limits
    }

    async fn list(&self, scope: AccountScope) -> Result<Vec<ScriptInfo>> {
        Ok(lock(&self.state)
            .accounts
            .get(&scope)
            .map(|a| {
                a.scripts
                    .iter()
                    .map(|(name, text)| ScriptInfo {
                        name: name.clone(),
                        size: text.len() as u64,
                        active: a.active.as_deref() == Some(name),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn get(&self, scope: AccountScope, name: &str) -> Result<Option<String>> {
        Ok(lock(&self.state)
            .accounts
            .get(&scope)
            .and_then(|a| a.scripts.get(name).cloned()))
    }

    async fn active(&self, scope: AccountScope) -> Result<Option<ActiveScript>> {
        Ok(lock(&self.state).accounts.get(&scope).and_then(|a| {
            let name = a.active.clone()?;
            let text = a.scripts.get(&name)?.clone();
            Some(ActiveScript { name, text })
        }))
    }

    async fn put(&self, scope: AccountScope, name: &str, text: String) -> Result<u64> {
        if !valid_script_name(name) {
            return Err(Error::invalid(format!(
                "недопустимое имя скрипта: {name:?}"
            )));
        }
        let limits = self.limits;
        self.change(scope, |a| {
            if text.len() > limits.max_script_size
                || (!a.scripts.contains_key(name) && a.scripts.len() >= limits.max_scripts)
            {
                return Err(Error::QuotaExceeded);
            }
            a.scripts.insert(name.to_owned(), text);
            Ok(true)
        })
        .await
    }

    async fn rename(&self, scope: AccountScope, from: &str, to: &str) -> Result<u64> {
        if !valid_script_name(to) {
            return Err(Error::invalid(format!("недопустимое имя скрипта: {to:?}")));
        }
        self.change(scope, |a| {
            if !a.scripts.contains_key(from) {
                return Err(Error::NotFound);
            }
            if from == to {
                return Ok(false);
            }
            if a.scripts.contains_key(to) {
                return Err(Error::Conflict);
            }
            let text = a.scripts.remove(from).ok_or(Error::NotFound)?;
            a.scripts.insert(to.to_owned(), text);
            if a.active.as_deref() == Some(from) {
                a.active = Some(to.to_owned());
            }
            Ok(true)
        })
        .await
    }

    async fn delete(&self, scope: AccountScope, name: &str) -> Result<u64> {
        self.change(scope, |a| {
            if !a.scripts.contains_key(name) {
                return Err(Error::NotFound);
            }
            if a.active.as_deref() == Some(name) {
                return Err(Error::Conflict);
            }
            a.scripts.remove(name);
            Ok(true)
        })
        .await
    }

    async fn activate(&self, scope: AccountScope, name: Option<&str>) -> Result<u64> {
        self.change(scope, |a| {
            if let Some(n) = name
                && !a.scripts.contains_key(n)
            {
                return Err(Error::NotFound);
            }
            a.active = name.map(str::to_owned);
            Ok(true)
        })
        .await
    }

    async fn org_script(&self, tenant: TenantId) -> Result<Option<String>> {
        Ok(lock(&self.state).org.get(&tenant).cloned())
    }

    async fn set_org_script(&self, tenant: TenantId, text: Option<String>) -> Result<()> {
        let mut st = lock(&self.state);
        match text {
            Some(t) if t.len() > self.limits.max_script_size => Err(Error::QuotaExceeded),
            Some(t) => {
                st.org.insert(tenant, t);
                Ok(())
            }
            None => {
                st.org.remove(&tenant);
                Ok(())
            }
        }
    }

    async fn vacation(&self, scope: AccountScope) -> Result<Vacation> {
        Ok(lock(&self.state)
            .accounts
            .get(&scope)
            .map(|a| a.vacation.clone())
            .unwrap_or_default())
    }

    async fn set_vacation(&self, scope: AccountScope, vacation: Vacation) -> Result<u64> {
        vacation.validate()?;
        self.change(scope, |a| {
            a.vacation = vacation;
            Ok(true)
        })
        .await
    }

    async fn state(&self, scope: AccountScope) -> Result<u64> {
        Ok(lock(&self.state)
            .accounts
            .get(&scope)
            .map_or(0, |a| a.state))
    }
}

/// Учётная запись, отправитель, `:handle`.
type SentKey = (AccountScope, String, String);

/// Журнал автоответов в памяти с часами [`Clock`]. Клоны разделяют данные.
#[derive(Debug, Clone)]
pub struct MemVacationLog {
    sent: Arc<Mutex<HashMap<SentKey, OffsetDateTime>>>,
    clock: Arc<dyn Clock>,
}

impl MemVacationLog {
    /// Пустой журнал с часами `clock`.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            sent: Arc::default(),
            clock,
        }
    }
}

impl Default for MemVacationLog {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock))
    }
}

#[async_trait]
impl VacationLog for MemVacationLog {
    async fn seen(
        &self,
        scope: AccountScope,
        sender: &str,
        handle: &str,
        period: Duration,
    ) -> Result<bool> {
        let now = self.clock.now();
        let mut sent = lock(&self.sent);
        let key = (scope, sender.to_owned(), handle.to_owned());
        if let Some(at) = sent.get(&key)
            && now - *at < period
        {
            return Ok(true);
        }
        sent.insert(key, now);
        Ok(false)
    }
}

/// Вызов [`FakeRules::evaluate`]: получатель и отправитель конверта (без письма).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesCall {
    /// Учётная запись получателя.
    pub scope: AccountScope,
    /// Адрес получателя.
    pub recipient: String,
    /// Отправитель конверта.
    pub mail_from: String,
}

#[derive(Debug, Default)]
struct FakeState {
    outcomes: HashMap<String, RulesOutcome>,
    faults: VecDeque<Error>,
    calls: Vec<RulesCall>,
}

/// Фейк правил доставки для тестов `tp-smtp`: итог по адресу получателя ([`FakeRules::set`]), по умолчанию —
/// implicit keep; [`FakeRules::fail_next`] — ошибка следующего вызова; [`FakeRules::calls`] — журнал вызовов.
#[derive(Debug, Clone, Default)]
pub struct FakeRules {
    state: Arc<Mutex<FakeState>>,
}

impl FakeRules {
    /// Без правил: implicit keep для всех.
    pub fn new() -> Self {
        Self::default()
    }

    /// Итог `outcome` для получателя `recipient` (адрес как в [`RulesInput::recipient`]).
    pub fn set(&self, recipient: impl Into<String>, outcome: RulesOutcome) {
        lock(&self.state).outcomes.insert(recipient.into(), outcome);
    }

    /// Следующий вызов вернёт `err` (очередь, по одной на вызов).
    pub fn fail_next(&self, err: Error) {
        lock(&self.state).faults.push_back(err);
    }

    /// Вызовы по порядку.
    pub fn calls(&self) -> Vec<RulesCall> {
        lock(&self.state).calls.clone()
    }
}

#[async_trait]
impl DeliveryRules for FakeRules {
    async fn evaluate(&self, input: &RulesInput) -> Result<RulesOutcome> {
        let mut st = lock(&self.state);
        st.calls.push(RulesCall {
            scope: input.scope,
            recipient: input.recipient.clone(),
            mail_from: input.mail_from.clone(),
        });
        if let Some(e) = st.faults.pop_front() {
            return Err(e);
        }
        Ok(st
            .outcomes
            .get(&input.recipient)
            .cloned()
            .unwrap_or_else(RulesOutcome::implicit_keep))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use tp_contracts::sieve::{KeepTo, MailboxRef};
    use tp_contracts::{AccountId, MailboxId};

    use crate::{ManualClock, suite};

    fn scope() -> AccountScope {
        AccountScope {
            tenant: TenantId::new(),
            account: AccountId::new(),
        }
    }

    #[tokio::test]
    async fn suite_sieve() {
        let store = MemSieve::new().with_limits(suite::sieve::LIMITS);
        suite::sieve(&store, &suite::sieve::SieveFixture::new()).await;
    }

    #[tokio::test]
    async fn suite_vacation_log() {
        let clock = ManualClock::default();
        let log = MemVacationLog::new(clock.shared());
        suite::sieve::vacation_log(&log, scope(), scope(), &clock).await;
    }

    #[tokio::test]
    async fn suite_sieve_signals() {
        let bus = crate::MemEventBus::default();
        let store = MemSieve::new().with_bus(Arc::new(bus.clone()));
        suite::sieve::signals(&store, &bus, scope()).await;
    }

    #[tokio::test]
    async fn fake_rules() {
        let rules = FakeRules::new();
        let s = scope();
        let input = |rcpt: &str| RulesInput {
            scope: s,
            recipient: rcpt.into(),
            mail_from: "x@example.net".into(),
            message: Bytes::from_static(b"Subject: x\r\n\r\nbody"),
            mailboxes: Vec::new(),
        };
        let folder = RulesOutcome {
            keep: vec![KeepTo {
                mailbox: MailboxRef::Id(MailboxId::new()),
                flags: Default::default(),
            }],
            ..RulesOutcome::default()
        };
        rules.set("anna+news@example.org", folder.clone());
        assert_eq!(
            rules.evaluate(&input("anna+news@example.org")).await,
            Ok(folder)
        );
        assert_eq!(
            rules.evaluate(&input("anna@example.org")).await,
            Ok(RulesOutcome::implicit_keep())
        );
        rules.fail_next(Error::UNAVAILABLE);
        assert_eq!(
            rules.evaluate(&input("anna@example.org")).await,
            Err(Error::UNAVAILABLE)
        );
        let calls = rules.calls();
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[0].recipient, "anna+news@example.org");
        assert_eq!(calls[2].scope, s);
    }
}
