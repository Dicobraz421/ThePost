//! Синхронизация каталога организации в метаданные (AUTH-10; SRS 5.1, 4.7; решение 2026-11-04-exchange-parity).
//!
//! Источник — любой [`Directory`] (LDAP / Active Directory — [`LdapDirectory`](crate::LdapDirectory)).
//! [`DirectorySync::accounts`] переносит учётные записи в [`AccountStore`], подразделения и оргатрибуты
//! (подразделение, должность, руководитель, старшинство, атрибуты GAL) — в [`OrgStore`];
//! [`DirectorySync::contacts`] — почтовые контакты в [`ContactStore`] (они попадают в GAL и в раскрытие групп).
//!
//! Учётная запись, исчезнувшая из каталога (надгробие или отсутствие в полной выгрузке), не удаляется: ящик
//! отключается ([`AccountState::Disabled`]) до `now + retention` ([`RemovalPolicy`]). Вернувшаяся в каталог —
//! восстанавливается. Если отключить нужно больше `threshold_percent` действующих учётных записей, запуск
//! останавливается без изменений: остановка записывается в [`SyncGate`] (её видит Admin API), в журнал —
//! ошибка; после подтверждения администратором (`approve_sync`) следующий запуск применяет изменения и снимает
//! остановку.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use tp_contracts::addressbook::{ContactStore, MailContact, OrgAttributes, OrgStore, OrgUnit};
use tp_contracts::auth::{
    DirAccount, DirChange, DirContact, DirCursor, DirPage, DirUnit, Directory,
    MASS_REMOVAL_PERCENT, MassRemoval, SyncGate, exceeds_removal_threshold,
};
use tp_contracts::config::duration;
use tp_contracts::meta::{Account, AccountStore, AccountUpsert};
use tp_contracts::transport::TransportStore;
use tp_contracts::{AccountId, ContactId, DomainId, Error, OrgUnitId, Result, TenantId};

/// Политика исчезновения учётных записей из каталога (`auth.ldap.removal`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RemovalPolicy {
    /// Срок хранения отключённого ящика.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retention: Duration,
    /// Порог массового удаления, процентов действующих учётных записей: больше — остановка до подтверждения.
    pub threshold_percent: u64,
}

impl Default for RemovalPolicy {
    fn default() -> Self {
        RemovalPolicy {
            retention: Duration::from_secs(30 * 24 * 3600),
            threshold_percent: MASS_REMOVAL_PERCENT,
        }
    }
}

/// Хранилища, в которые пишет синхронизация.
#[derive(Clone)]
pub struct DirStores {
    /// Учётные записи и состояние ящиков.
    pub accounts: Arc<dyn AccountStore>,
    /// Почтовые контакты.
    pub contacts: Arc<dyn ContactStore>,
    /// Подразделения и оргатрибуты.
    pub org: Arc<dyn OrgStore>,
    /// Остановка при массовом удалении.
    pub gate: Arc<dyn SyncGate>,
    /// Принятые домены (основной домен учётной записи).
    pub transport: Arc<dyn TransportStore>,
}

impl std::fmt::Debug for DirStores {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirStores").finish_non_exhaustive()
    }
}

/// Итог синхронизации учётных записей.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSyncReport {
    /// Позиция для следующего запуска (сохраняет вызывающий); при остановке — прежняя.
    pub cursor: Option<DirCursor>,
    /// Курсор устарел: выполнена полная выгрузка.
    pub full: bool,
    /// Созданные или обновлённые учётные записи.
    pub upserted: usize,
    /// Отключённые ящики.
    pub disabled: usize,
    /// Восстановленные ящики (учётная запись вернулась в каталог).
    pub restored: usize,
    /// Пропущенные записи: домен не принят, некорректные поля (в журнале — предупреждение).
    pub skipped: usize,
    /// Запуск остановлен массовым удалением: ничего не применено, ждёт подтверждения администратора.
    pub held: Option<MassRemoval>,
}

/// Итог синхронизации почтовых контактов.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactSyncReport {
    /// Позиция для следующего запуска (сохраняет вызывающий).
    pub cursor: DirCursor,
    /// Курсор устарел: выполнена полная выгрузка.
    pub full: bool,
    /// Созданные или обновлённые контакты.
    pub upserted: usize,
    /// Удалённые контакты.
    pub removed: usize,
    /// Пропущенные записи: некорректные или с адресом, занятым другим контактом.
    pub skipped: usize,
}

/// Синхронизация каталога организации `tenant` в метаданные.
#[derive(Clone)]
pub struct DirectorySync {
    dir: Arc<dyn Directory>,
    tenant: TenantId,
    batch: usize,
    policy: RemovalPolicy,
    stores: DirStores,
}

impl std::fmt::Debug for DirectorySync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DirectorySync")
            .field("tenant", &self.tenant)
            .field("batch", &self.batch)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

/// Все изменения после курсора.
struct Changes<T> {
    changes: Vec<DirChange<T>>,
    next: DirCursor,
    /// Выгрузка полная: записей, которых в ней нет, в каталоге нет.
    listing: bool,
    /// Курсор устарел.
    full: bool,
}

impl DirectorySync {
    /// Синхронизация каталога `dir` организации `tenant` порциями по `batch` (больше нуля).
    pub fn new(
        dir: Arc<dyn Directory>,
        tenant: TenantId,
        batch: usize,
        policy: RemovalPolicy,
        stores: DirStores,
    ) -> Result<Self> {
        if batch == 0 {
            return Err(Error::invalid("directory sync: batch is zero"));
        }
        Ok(DirectorySync {
            dir,
            tenant,
            batch,
            policy,
            stores,
        })
    }

    /// Изменения после `cursor` до конца; устаревший курсор — полная выгрузка.
    async fn collect<T, F>(&self, cursor: Option<&DirCursor>, fetch: F) -> Result<Changes<T>>
    where
        F: AsyncFn(Option<&DirCursor>, usize) -> Result<DirPage<T>>,
    {
        let (mut page, full) = match fetch(cursor, self.batch).await {
            Err(Error::Conflict) if cursor.is_some() => {
                tracing::warn!("directory sync: stale cursor, full resync");
                (fetch(None, self.batch).await?, true)
            }
            r => (r?, false),
        };
        let mut changes = std::mem::take(&mut page.changes);
        while page.has_more {
            page = fetch(Some(&page.next), self.batch).await?;
            changes.append(&mut page.changes);
        }
        Ok(Changes {
            changes,
            next: page.next,
            listing: cursor.is_none() || full,
            full,
        })
    }

    // ---------- Учётные записи ----------

    /// Учётные записи, подразделения и оргатрибуты после `cursor`; `None` — полная выгрузка: учётные записи
    /// каталога, которых в ней нет, отключаются. Ошибка каталога или хранилища прерывает запуск: применённое
    /// сохранено, курсор прежний.
    pub async fn accounts(&self, cursor: Option<&DirCursor>) -> Result<AccountSyncReport> {
        let t = self.tenant;
        let dir = &self.dir;
        let mut got = self
            .collect(cursor, async |c, n| dir.accounts_since(t, c, n).await)
            .await?;
        got.changes = latest(got.changes, |a| &a.external_id);
        let all = self.all_accounts().await?;
        let disabled: HashSet<AccountId> = self
            .stores
            .accounts
            .disabled_accounts(t)
            .await?
            .into_iter()
            .map(|d| d.account.id)
            .collect();
        let removals = self.removals(&got, &all, &disabled).await?;
        let total = all.iter().filter(|a| !disabled.contains(&a.id)).count();
        let now = OffsetDateTime::now_utc();
        let gate = &self.stores.gate;
        let (removed, total) = (removals.len() as u64, total as u64);
        let held = if exceeds_removal_threshold(removed, total, self.policy.threshold_percent) {
            let hold = gate.hold_sync(t, removed, total, now).await?;
            if !hold.approved {
                tracing::error!(
                    removed,
                    total,
                    percent = self.policy.threshold_percent,
                    "directory sync stopped: mass removal of accounts, confirm through Admin API"
                );
                return Ok(AccountSyncReport {
                    cursor: cursor.cloned(),
                    full: got.full,
                    upserted: 0,
                    disabled: 0,
                    restored: 0,
                    skipped: 0,
                    held: Some(hold),
                });
            }
            true
        } else {
            gate.pending_sync(t).await?.is_some()
        };

        let mut report = AccountSyncReport {
            cursor: Some(got.next.clone()),
            full: got.full,
            upserted: 0,
            disabled: 0,
            restored: 0,
            skipped: 0,
            held: None,
        };
        let units = self.units().await?;
        let domains = self.domains().await?;
        let mut org = Vec::new();
        for change in got.changes {
            let DirChange::Upsert(a) = change else {
                continue;
            };
            match self.upsert(&a, &domains, &disabled).await? {
                Some((account, restored)) => {
                    report.upserted += 1;
                    report.restored += usize::from(restored);
                    org.push((account, a));
                }
                None => report.skipped += 1,
            }
        }
        let until = now + self.policy.retention;
        for id in removals {
            self.stores.accounts.disable_account(t, id, until).await?;
            report.disabled += 1;
        }
        for (account, a) in org {
            if !self.set_org(account, &a, &units).await? {
                report.skipped += 1;
            }
        }
        if held {
            gate.clear_sync(t).await?;
        }
        Ok(report)
    }

    /// Все учётные записи организации.
    async fn all_accounts(&self) -> Result<Vec<Account>> {
        let mut out: Vec<Account> = Vec::new();
        loop {
            let after = out.last().map(|a| a.login.to_lowercase());
            let page = self
                .stores
                .accounts
                .list(self.tenant, "", after.as_deref(), self.batch)
                .await?;
            let last = page.len() < self.batch;
            out.extend(page);
            if last {
                return Ok(out);
            }
        }
    }

    /// Действующие учётные записи к отключению: надгробия и (полная выгрузка) отсутствующие в ней записи
    /// каталога. Пустая выгрузка ничего не отключает: ошибка базы поиска или фильтра не должна отключить всех.
    async fn removals(
        &self,
        got: &Changes<DirAccount>,
        all: &[Account],
        disabled: &HashSet<AccountId>,
    ) -> Result<Vec<AccountId>> {
        let mut out = Vec::new();
        for change in &got.changes {
            if let DirChange::Removed { external_id } = change
                && let Some(a) = self
                    .stores
                    .accounts
                    .by_external_id(self.tenant, external_id)
                    .await?
                && !disabled.contains(&a.id)
            {
                out.push(a.id);
            }
        }
        if got.listing {
            let seen: HashSet<&str> = got
                .changes
                .iter()
                .filter_map(|c| match c {
                    DirChange::Upsert(a) => Some(a.external_id.as_str()),
                    DirChange::Removed { .. } => None,
                })
                .collect();
            if seen.is_empty() {
                if all.iter().any(|a| a.external_id.is_some()) {
                    tracing::warn!(
                        "directory sync: directory returned no accounts, removal skipped"
                    );
                }
            } else {
                out.extend(
                    all.iter()
                        .filter(|a| {
                            a.external_id.as_deref().is_some_and(|x| !seen.contains(x))
                                && !disabled.contains(&a.id)
                        })
                        .map(|a| a.id),
                );
            }
        }
        out.sort_unstable();
        out.dedup();
        Ok(out)
    }

    /// Принятые домены организации по имени (нижний регистр).
    async fn domains(&self) -> Result<HashMap<String, DomainId>> {
        Ok(self
            .stores
            .transport
            .cluster()
            .await?
            .domains
            .into_iter()
            .filter(|d| d.tenant == self.tenant)
            .map(|d| (d.name.to_ascii_lowercase(), d.id))
            .collect())
    }

    /// Создаёт или обновляет учётную запись; отключённый ящик восстанавливается (`true`). Домен основного
    /// адреса (нет адресов — имени входа) не принят организацией — пропуск (`None`).
    async fn upsert(
        &self,
        a: &DirAccount,
        domains: &HashMap<String, DomainId>,
        disabled: &HashSet<AccountId>,
    ) -> Result<Option<(AccountId, bool)>> {
        let t = self.tenant;
        let addr = a.emails.first().unwrap_or(&a.login);
        let domain = addr
            .rsplit_once('@')
            .and_then(|(_, d)| domains.get(&d.to_ascii_lowercase()));
        let Some(&domain) = domain else {
            tracing::warn!(external_id = %a.external_id, "directory account outside accepted domains, skipped");
            return Ok(None);
        };
        let upsert = AccountUpsert {
            domain,
            login: a.login.clone(),
            display_name: a.display_name.clone(),
            external_id: Some(a.external_id.clone()),
            enabled: a.enabled,
            // Адреса каталога: основной и повторы отбрасывает правило `meta::account_aliases` (CTR-53).
            aliases: a.emails.clone(),
            kind: tp_contracts::meta::AccountKind::User,
        };
        let account = match self.stores.accounts.upsert(t, upsert).await {
            Ok(acc) => acc,
            Err(e @ (Error::Invalid(_) | Error::Conflict)) => {
                tracing::warn!(external_id = %a.external_id, error = %e, "directory account skipped");
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let restored = disabled.contains(&account.id);
        if restored {
            self.stores.accounts.restore_account(t, account.id).await?;
        }
        Ok(Some((account.id, restored)))
    }

    /// Оргатрибуты учётной записи из каталога; некорректные — пропуск (`false`).
    async fn set_org(
        &self,
        account: AccountId,
        a: &DirAccount,
        units: &HashMap<String, OrgUnitId>,
    ) -> Result<bool> {
        let t = self.tenant;
        let manager = match &a.org.manager {
            Some(x) => self
                .stores
                .accounts
                .by_external_id(t, x)
                .await?
                .map(|m| m.id)
                .filter(|m| *m != account),
            None => None,
        };
        let org = OrgAttributes {
            department: a
                .org
                .department
                .as_ref()
                .and_then(|d| units.get(d).copied()),
            title: a.org.title.clone(),
            manager,
            seniority: a.org.seniority,
            attributes: a.attributes.clone(),
        };
        match self.stores.org.set_org(t, account, org).await {
            Ok(()) => Ok(true),
            Err(e @ Error::Invalid(_)) => {
                tracing::warn!(external_id = %a.external_id, error = %e, "directory org attributes skipped");
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Подразделения каталога в [`OrgStore`] (полный список): вышестоящие — раньше вложенных, неизвестное
    /// вышестоящее — корень; подразделения каталога, которых нет в списке, удаляются (пустой список ничего не
    /// удаляет). Возвращает `external_id` → идентификатор.
    async fn units(&self) -> Result<HashMap<String, OrgUnitId>> {
        let t = self.tenant;
        let org = &self.stores.org;
        let listed = self.dir.units(t).await?;
        let stored = org.org_units(t).await?;
        let mut ids: HashMap<String, OrgUnitId> = stored
            .iter()
            .filter_map(|u| u.external_id.clone().map(|x| (x, u.id)))
            .collect();
        let known: HashSet<&str> = listed.iter().map(|u| u.external_id.as_str()).collect();
        let mut pending: Vec<&DirUnit> = listed.iter().collect();
        let mut placed: HashSet<String> = HashSet::new();
        while !pending.is_empty() {
            let (ready, rest): (Vec<&DirUnit>, Vec<&DirUnit>) =
                pending.into_iter().partition(|u| {
                    u.parent
                        .as_deref()
                        .is_none_or(|p| placed.contains(p) || !known.contains(p))
                });
            // Цикл вышестоящих: оставшиеся — корни.
            let (ready, rest) = if ready.is_empty() {
                (rest, Vec::new())
            } else {
                (ready, rest)
            };
            for u in ready {
                let parent = u
                    .parent
                    .as_deref()
                    .filter(|p| placed.contains(*p))
                    .and_then(|p| ids.get(p).copied());
                let unit = OrgUnit {
                    id: ids
                        .get(&u.external_id)
                        .copied()
                        .unwrap_or_else(OrgUnitId::new),
                    tenant: t,
                    name: u.name.clone(),
                    parent,
                    external_id: Some(u.external_id.clone()),
                };
                let id = unit.id;
                match org.put_org_unit(unit).await {
                    Ok(()) => {
                        ids.insert(u.external_id.clone(), id);
                        placed.insert(u.external_id.clone());
                    }
                    Err(e @ (Error::Invalid(_) | Error::Conflict)) => {
                        tracing::warn!(external_id = %u.external_id, error = %e, "directory unit skipped");
                    }
                    Err(e) => return Err(e),
                }
            }
            pending = rest;
        }
        if !listed.is_empty() {
            // Вложенные — раньше вышестоящих: удаляем, пока удаляется.
            let mut gone: Vec<&OrgUnit> = stored
                .iter()
                .filter(|u| u.external_id.as_deref().is_some_and(|x| !known.contains(x)))
                .collect();
            while !gone.is_empty() {
                let before = gone.len();
                let mut left = Vec::new();
                for u in gone {
                    match org.remove_org_unit(t, u.id).await {
                        Ok(()) | Err(Error::NotFound) => {}
                        Err(Error::Conflict) => left.push(u),
                        Err(e) => return Err(e),
                    }
                }
                if left.len() == before {
                    tracing::warn!("directory sync: units with local children kept");
                    break;
                }
                gone = left;
            }
            ids.retain(|x, _| known.contains(x.as_str()));
        }
        Ok(ids)
    }

    // ---------- Контакты ----------

    /// Почтовые контакты после `cursor`; `None` — полная выгрузка: контакты каталога, которых в ней нет,
    /// удаляются (пустая выгрузка ничего не удаляет). Группы контакта и скрытие из GAL сохраняются.
    pub async fn contacts(&self, cursor: Option<&DirCursor>) -> Result<ContactSyncReport> {
        let t = self.tenant;
        let dir = &self.dir;
        let store = &self.stores.contacts;
        let mut got = self
            .collect(cursor, async |c, n| dir.contacts_since(t, c, n).await)
            .await?;
        got.changes = latest(got.changes, |c| &c.external_id);
        let mut report = ContactSyncReport {
            cursor: got.next.clone(),
            full: got.full,
            upserted: 0,
            removed: 0,
            skipped: 0,
        };
        let mut seen: HashSet<String> = HashSet::new();
        for change in got.changes {
            match change {
                DirChange::Upsert(c) => {
                    seen.insert(c.external_id.clone());
                    if self.put_contact(c).await? {
                        report.upserted += 1;
                    } else {
                        report.skipped += 1;
                    }
                }
                DirChange::Removed { external_id } => {
                    if let Some(c) = store.contact_by_external_id(t, &external_id).await? {
                        store.remove_contact(t, c.id).await?;
                        report.removed += 1;
                    }
                }
            }
        }
        if got.listing && !seen.is_empty() {
            for c in store.contacts(t).await? {
                if c.external_id.as_ref().is_some_and(|x| !seen.contains(x)) {
                    store.remove_contact(t, c.id).await?;
                    report.removed += 1;
                }
            }
        }
        Ok(report)
    }

    /// Сохраняет контакт каталога; некорректный или с занятым адресом — пропуск (`false`).
    async fn put_contact(&self, c: DirContact) -> Result<bool> {
        let t = self.tenant;
        let store = &self.stores.contacts;
        let existing = store.contact_by_external_id(t, &c.external_id).await?;
        let contact = MailContact {
            id: existing.as_ref().map_or_else(ContactId::new, |e| e.id),
            tenant: t,
            display_name: c.display_name,
            email: c.email,
            external_id: Some(c.external_id.clone()),
            attributes: c.attributes,
            groups: existing
                .as_ref()
                .map(|e| e.groups.clone())
                .unwrap_or_default(),
            hidden_from_gal: existing.as_ref().is_some_and(|e| e.hidden_from_gal),
        };
        match store.put_contact(contact).await {
            Ok(()) => Ok(true),
            Err(e @ (Error::Invalid(_) | Error::Conflict)) => {
                tracing::warn!(external_id = %c.external_id, error = %e, "directory contact skipped");
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }
}

/// Последнее изменение каждой записи (запись могла измениться и удалиться в одной выгрузке), в порядке каталога.
fn latest<T>(changes: Vec<DirChange<T>>, id: impl Fn(&T) -> &String) -> Vec<DirChange<T>> {
    let key = |c: &DirChange<T>| match c {
        DirChange::Upsert(v) => id(v).clone(),
        DirChange::Removed { external_id } => external_id.clone(),
    };
    let last: HashMap<String, usize> = changes
        .iter()
        .enumerate()
        .map(|(i, c)| (key(c), i))
        .collect();
    changes
        .into_iter()
        .enumerate()
        .filter(|(i, c)| last.get(&key(c)) == Some(i))
        .map(|(_, c)| c)
        .collect()
}

#[cfg(test)]
mod tests;
