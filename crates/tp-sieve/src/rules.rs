//! Правила доставки (SIEVE-07): [`Rules`] — реализация [`DeliveryRules`] (CONTRACTS.md#Sieve).
//!
//! Порядок: скрипт организации, автоответ из настройки [`Vacation`], активный скрипт пользователя. `stop` в скрипте
//! организации останавливает всё: ни автоответ, ни скрипт пользователя не выполняются. Иначе действия скриптов
//! складываются ([`crate::outcome`]). Ошибка скрипта — его действия отменены, письмо остаётся во «Входящих»,
//! запись — в [`RulesOutcome::errors`] и журнал (без содержимого письма). `Err` — только сбой хранилища.
//!
//! Программы кэшируются: учётной записи — по [`SieveStore::state`] (вместе с настройкой автоответа; любое изменение
//! в хранилище меняет состояние и сбрасывает запись), организации — по тексту скрипта. Объём — `sieve.cache`.
//!
//! Защита `redirect` от петли: письмо с числом `Received` больше `sieve.max_received` или уже прошедшее этот узел
//! `sieve.max_node_hops` раз (метка узла — `by <имя узла>` в `Received`) не перенаправляется и остаётся во «Входящих».

use std::collections::HashMap;
use std::fmt;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::meta::AccountScope;
use tp_contracts::sieve::{
    DeliveryRules, RulesInput, RulesOutcome, SieveStore, Vacation, VacationLog,
};
use tp_contracts::{Result, TenantId};

use crate::Limits;
use crate::compile::{Program, compile};
use crate::config::SieveConfig;
use crate::outcome::{Merge, mailbox_paths};
use crate::run::{Context, Hdr, parse_headers, run};
use crate::vacation::{self, Source};

/// Метка узла The Post в `Received`, когда имя узла не задано ([`Rules::with_node`]).
const PRODUCT_MARK: [&str; 2] = ["(The", "Post)"];

const ORG_SCRIPT: &str = "скрипт организации";
const USER_SCRIPT: &str = "скрипт пользователя";

/// Программа или ошибка проверки сохранённого скрипта.
type Compiled = std::result::Result<Arc<Program>, String>;

type Now = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

struct AccountEntry {
    state: u64,
    script: Option<Compiled>,
    vacation: Vacation,
    used: u64,
}

struct OrgEntry {
    text: String,
    program: Compiled,
    used: u64,
}

#[derive(Default)]
struct Cache {
    tick: u64,
    accounts: HashMap<AccountScope, AccountEntry>,
    orgs: HashMap<TenantId, OrgEntry>,
}

/// Правила доставки Sieve.
pub struct Rules {
    store: Arc<dyn SieveStore>,
    log: Arc<dyn VacationLog>,
    config: SieveConfig,
    limits: Limits,
    node: Option<String>,
    now: Now,
    cache: Mutex<Cache>,
    compilations: AtomicU64,
}

impl fmt::Debug for Rules {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Rules")
            .field("config", &self.config)
            .field("node", &self.node)
            .finish_non_exhaustive()
    }
}

impl Rules {
    /// Правила над хранилищем скриптов `store` и журналом автоответов `log` с секцией конфигурации `config`.
    pub fn new(store: Arc<dyn SieveStore>, log: Arc<dyn VacationLog>, config: SieveConfig) -> Self {
        Self {
            store,
            log,
            limits: config.limits(),
            config,
            node: None,
            now: Arc::new(OffsetDateTime::now_utc),
            cache: Mutex::default(),
            compilations: AtomicU64::new(0),
        }
    }

    /// Имя узла, которое `tp-smtp` пишет в `Received` (`by <имя>`), — метка узла для защиты от петли. Без него
    /// своими считаются все `Received` с пометкой `(The Post)`.
    #[must_use]
    pub fn with_node(mut self, hostname: impl Into<String>) -> Self {
        self.node = Some(hostname.into());
        self
    }

    /// Источник текущего времени (UTC) для срока действия автоответа; по умолчанию — системные часы.
    #[must_use]
    pub fn with_clock(mut self, now: impl Fn() -> OffsetDateTime + Send + Sync + 'static) -> Self {
        self.now = Arc::new(now);
        self
    }

    /// Сколько раз компилировались скрипты (промахи кэша).
    #[cfg(test)]
    pub(crate) fn compilations(&self) -> u64 {
        self.compilations.load(Ordering::Relaxed)
    }

    fn cache(&self) -> MutexGuard<'_, Cache> {
        self.cache.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn compile(&self, text: &str) -> Compiled {
        self.compilations.fetch_add(1, Ordering::Relaxed);
        compile(text.as_bytes(), &self.limits)
            .map(Arc::new)
            .map_err(|d| {
                d.first().map_or_else(
                    || "скрипт не прошёл проверку".to_owned(),
                    ToString::to_string,
                )
            })
    }

    /// Программа скрипта организации; кэш сверяется с текстом.
    async fn org(&self, tenant: TenantId) -> Result<Option<Compiled>> {
        let Some(text) = self.store.org_script(tenant).await? else {
            self.cache().orgs.remove(&tenant);
            return Ok(None);
        };
        {
            let mut cache = self.cache();
            cache.tick += 1;
            let tick = cache.tick;
            if let Some(e) = cache.orgs.get_mut(&tenant)
                && e.text == text
            {
                e.used = tick;
                return Ok(Some(e.program.clone()));
            }
        }
        let program = self.compile(&text);
        let mut cache = self.cache();
        let used = cache.tick;
        let cap = self.config.cache.programs;
        insert(
            &mut cache.orgs,
            cap,
            tenant,
            |e| e.used,
            || OrgEntry {
                text,
                program: program.clone(),
                used,
            },
        );
        Ok(Some(program))
    }

    /// Программа активного скрипта и настройка автоответа; кэш — по состоянию хранилища.
    async fn account(&self, scope: AccountScope) -> Result<(Option<Compiled>, Vacation)> {
        let state = self.store.state(scope).await?;
        {
            let mut cache = self.cache();
            cache.tick += 1;
            let tick = cache.tick;
            if let Some(e) = cache.accounts.get_mut(&scope)
                && e.state == state
            {
                e.used = tick;
                return Ok((e.script.clone(), e.vacation.clone()));
            }
        }
        let active = self.store.active(scope).await?;
        let vacation = self.store.vacation(scope).await?;
        let script = active.map(|a| self.compile(&a.text));
        let mut cache = self.cache();
        let used = cache.tick;
        let cap = self.config.cache.programs;
        cache.accounts.remove(&scope);
        insert(
            &mut cache.accounts,
            cap,
            scope,
            |e| e.used,
            || AccountEntry {
                state,
                script: script.clone(),
                vacation: vacation.clone(),
                used,
            },
        );
        Ok((script, vacation))
    }

    /// Причина отменить `redirect`: признаки петли в `Received`.
    fn looped(&self, headers: &[Hdr]) -> Option<String> {
        let received: Vec<&Hdr> = headers
            .iter()
            .filter(|h| h.name.eq_ignore_ascii_case("Received"))
            .collect();
        if received.len() > self.config.max_received {
            return Some(format!(
                "redirect отменён: в письме больше {} заголовков Received (петля пересылки)",
                self.config.max_received
            ));
        }
        let own = received
            .iter()
            .filter(|h| by_node(&h.value, self.node.as_deref()))
            .count();
        if own >= self.config.max_node_hops {
            return Some(format!(
                "redirect отменён: письмо уже проходило этот узел {own} раз (петля пересылки)"
            ));
        }
        None
    }
}

/// `Received` написан этим узлом: `by <node>`; имя узла не задано — `by <любое> (The Post)`.
fn by_node(received: &str, node: Option<&str>) -> bool {
    let mut it = received.split_whitespace();
    while let Some(token) = it.next() {
        if !token.eq_ignore_ascii_case("by") {
            continue;
        }
        let host = it.next();
        return match node {
            Some(node) => host.is_some_and(|h| h.eq_ignore_ascii_case(node)),
            None => {
                host.is_some()
                    && it.next() == Some(PRODUCT_MARK[0])
                    && it.next() == Some(PRODUCT_MARK[1])
            }
        };
    }
    false
}

/// Кладёт запись в кэш на `cap` записей, вытесняя давно не использованную; `cap = 0` — не кладёт.
fn insert<K: Copy + Eq + Hash, V>(
    map: &mut HashMap<K, V>,
    cap: usize,
    key: K,
    used: impl Fn(&V) -> u64,
    value: impl FnOnce() -> V,
) {
    if cap == 0 {
        return;
    }
    while map.len() >= cap && !map.contains_key(&key) {
        let Some(oldest) = map.iter().min_by_key(|(_, v)| used(v)).map(|(k, _)| *k) else {
            break;
        };
        map.remove(&oldest);
    }
    map.insert(key, value());
}

#[async_trait]
impl DeliveryRules for Rules {
    async fn evaluate(&self, input: &RulesInput) -> Result<RulesOutcome> {
        let org = self.org(input.scope.tenant).await?;
        let (script, setting) = self.account(input.scope).await?;
        let now = (self.now)();
        if org.is_none() && script.is_none() && !setting.is_active(now) {
            return Ok(RulesOutcome::implicit_keep());
        }

        let paths = mailbox_paths(&input.mailboxes);
        let ctx = Context {
            message: &input.message,
            envelope_from: &input.mail_from,
            envelope_to: &input.recipient,
            mailboxes: &paths,
        };
        let headers = parse_headers(&input.message);
        let mut merge = Merge::new(&input.mailboxes);
        let mut reply = None;
        let mut stopped = false;
        match &org {
            Some(Ok(program)) => {
                let outcome = run(program, &ctx, &self.limits);
                stopped = outcome.stopped;
                merge.add(ORG_SCRIPT, outcome);
            }
            Some(Err(e)) => merge.fail(ORG_SCRIPT, e),
            None => {}
        }
        if !stopped {
            if setting.is_active(now) {
                let source = Source::Setting(&setting, now);
                reply = vacation::reply(source, input, &headers, &self.config.vacation, &*self.log)
                    .await?;
            }
            match &script {
                Some(Ok(program)) => merge.add(USER_SCRIPT, run(program, &ctx, &self.limits)),
                Some(Err(e)) => merge.fail(USER_SCRIPT, e),
                None => {}
            }
        }
        if merge.redirects()
            && let Some(reason) = self.looped(&headers)
        {
            merge.cancel_redirects(&reason);
        }
        let (mut outcome, action) = merge.finish();
        if reply.is_none()
            && let Some(action) = &action
        {
            let source = Source::Script(action);
            reply =
                vacation::reply(source, input, &headers, &self.config.vacation, &*self.log).await?;
        }
        outcome.vacation = reply;
        for error in &outcome.errors {
            tracing::warn!(
                tenant = %input.scope.tenant,
                account = %input.scope.account,
                error = %error,
                "sieve: ошибка правил доставки"
            );
        }
        Ok(outcome)
    }
}
