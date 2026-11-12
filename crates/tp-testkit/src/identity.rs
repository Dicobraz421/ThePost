//! [`IdentityStore`] и [`SubmissionStore`] в памяти (CONTRACTS.md#Identity и EmailSubmission, CTR-30, CTR-42).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::identity::{Identity, IdentityStore};
use tp_contracts::meta::AccountScope;
use tp_contracts::queue::SubmitStatus;
use tp_contracts::submission::{EmailSubmission, SubmissionChanges, SubmissionStore, UndoStatus};
use tp_contracts::{Error, QueueId, Result};

use crate::infra::lock;

#[derive(Debug)]
struct Account<T> {
    items: BTreeMap<String, T>,
    state: u64,
}

impl<T> Default for Account<T> {
    fn default() -> Self {
        Self {
            items: BTreeMap::new(),
            state: 0,
        }
    }
}

impl<T> Account<T> {
    fn bump(&mut self) -> u64 {
        self.state += 1;
        self.state
    }
}

/// Адреса отправителя учётных записей в памяти. Клоны разделяют данные.
#[derive(Debug, Clone, Default)]
pub struct MemIdentities {
    state: Arc<Mutex<HashMap<AccountScope, Account<Identity>>>>,
}

impl MemIdentities {
    /// Пустое хранилище.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl IdentityStore for MemIdentities {
    async fn list(&self, scope: AccountScope) -> Result<Vec<Identity>> {
        Ok(lock(&self.state)
            .get(&scope)
            .map(|a| a.items.values().cloned().collect())
            .unwrap_or_default())
    }

    async fn get(&self, scope: AccountScope, id: &str) -> Result<Option<Identity>> {
        Ok(lock(&self.state)
            .get(&scope)
            .and_then(|a| a.items.get(id).cloned()))
    }

    async fn put(&self, scope: AccountScope, identity: Identity) -> Result<u64> {
        identity.validate()?;
        let mut st = lock(&self.state);
        let acc = st.entry(scope).or_default();
        acc.items.insert(identity.id.clone(), identity);
        Ok(acc.bump())
    }

    async fn delete(&self, scope: AccountScope, id: &str) -> Result<u64> {
        let mut st = lock(&self.state);
        let acc = st.get_mut(&scope).ok_or(Error::NotFound)?;
        acc.items.remove(id).ok_or(Error::NotFound)?;
        Ok(acc.bump())
    }

    async fn state(&self, scope: AccountScope) -> Result<u64> {
        Ok(lock(&self.state).get(&scope).map_or(0, |a| a.state))
    }
}

/// Вид изменения отправки в журнале [`MemSubmissions`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Created,
    Updated,
    Destroyed,
}

#[derive(Debug, Default)]
struct Subs {
    acc: Account<EmailSubmission>,
    /// Журнал изменений: состояние после изменения, отправка, вид.
    log: Vec<(u64, String, Op)>,
}

impl Subs {
    fn change(&mut self, id: &str, op: Op) -> u64 {
        let state = self.acc.bump();
        self.log.push((state, id.to_owned(), op));
        state
    }
}

/// Отправки учётных записей в памяти; журнал изменений хранится целиком. Клоны разделяют данные.
#[derive(Debug, Clone, Default)]
pub struct MemSubmissions {
    state: Arc<Mutex<HashMap<AccountScope, Subs>>>,
}

impl MemSubmissions {
    /// Пустое хранилище.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SubmissionStore for MemSubmissions {
    async fn insert(&self, scope: AccountScope, submission: EmailSubmission) -> Result<u64> {
        submission.validate()?;
        if submission.undo_status == UndoStatus::Canceled {
            return Err(Error::invalid("новая отправка уже отменена"));
        }
        let mut st = lock(&self.state);
        let subs = st.entry(scope).or_default();
        if subs.acc.items.contains_key(&submission.id) {
            return Err(Error::Conflict);
        }
        let id = submission.id.clone();
        subs.acc.items.insert(id.clone(), submission);
        Ok(subs.change(&id, Op::Created))
    }

    async fn get(&self, scope: AccountScope, id: &str) -> Result<Option<EmailSubmission>> {
        Ok(lock(&self.state)
            .get(&scope)
            .and_then(|a| a.acc.items.get(id).cloned()))
    }

    async fn list(&self, scope: AccountScope) -> Result<Vec<EmailSubmission>> {
        let mut list: Vec<_> = lock(&self.state)
            .get(&scope)
            .map(|a| a.acc.items.values().cloned().collect())
            .unwrap_or_default();
        list.sort_by(|a, b| (a.send_at, &a.id).cmp(&(b.send_at, &b.id)));
        Ok(list)
    }

    async fn cancel(&self, scope: AccountScope, id: &str) -> Result<EmailSubmission> {
        let mut st = lock(&self.state);
        let subs = st.get_mut(&scope).ok_or(Error::NotFound)?;
        let s = subs.acc.items.get_mut(id).ok_or(Error::NotFound)?;
        match s.undo_status {
            UndoStatus::Canceled => return Ok(s.clone()),
            UndoStatus::Final => return Err(Error::Conflict),
            UndoStatus::Pending => s.undo_status = UndoStatus::Canceled,
        }
        let out = s.clone();
        subs.change(id, Op::Updated);
        Ok(out)
    }

    async fn finalize(
        &self,
        scope: AccountScope,
        id: &str,
        delivery: Vec<(String, SubmitStatus)>,
        queue: Option<QueueId>,
    ) -> Result<u64> {
        let mut st = lock(&self.state);
        let subs = st.get_mut(&scope).ok_or(Error::NotFound)?;
        let s = subs.acc.items.get_mut(id).ok_or(Error::NotFound)?;
        if s.undo_status == UndoStatus::Canceled {
            return Err(Error::Conflict);
        }
        s.undo_status = UndoStatus::Final;
        s.delivery = delivery;
        s.queue = queue;
        Ok(subs.change(id, Op::Updated))
    }

    async fn delete(&self, scope: AccountScope, id: &str) -> Result<u64> {
        let mut st = lock(&self.state);
        let subs = st.get_mut(&scope).ok_or(Error::NotFound)?;
        subs.acc.items.remove(id).ok_or(Error::NotFound)?;
        Ok(subs.change(id, Op::Destroyed))
    }

    async fn due(
        &self,
        now: OffsetDateTime,
        limit: usize,
    ) -> Result<Vec<(AccountScope, EmailSubmission)>> {
        let mut out: Vec<(AccountScope, EmailSubmission)> = lock(&self.state)
            .iter()
            .flat_map(|(scope, subs)| {
                subs.acc
                    .items
                    .values()
                    .filter(|s| s.undo_status == UndoStatus::Pending && s.send_at <= now)
                    .map(|s| (*scope, s.clone()))
            })
            .collect();
        out.sort_by(|a, b| (a.1.send_at, &a.1.id).cmp(&(b.1.send_at, &b.1.id)));
        out.truncate(limit);
        Ok(out)
    }

    async fn state(&self, scope: AccountScope) -> Result<u64> {
        Ok(lock(&self.state).get(&scope).map_or(0, |a| a.acc.state))
    }

    async fn submission_changes(
        &self,
        scope: AccountScope,
        since: u64,
        limit: usize,
    ) -> Result<SubmissionChanges> {
        if limit == 0 {
            return Err(Error::invalid("submission changes limit must be positive"));
        }
        let st = lock(&self.state);
        let Some(subs) = st.get(&scope) else {
            return if since == 0 {
                Ok(SubmissionChanges::default())
            } else {
                Err(Error::Conflict)
            };
        };
        if since > subs.acc.state {
            return Err(Error::Conflict);
        }
        // Первое и последнее изменение каждой отправки после `since`, не более `limit` отправок.
        let mut seen: BTreeMap<&str, (Op, Op)> = BTreeMap::new();
        let mut new_state = since;
        let mut has_more = false;
        for (state, id, op) in subs.log.iter().filter(|(s, _, _)| *s > since) {
            if !seen.contains_key(id.as_str()) && seen.len() == limit {
                has_more = true;
                break;
            }
            seen.entry(id.as_str())
                .and_modify(|(_, last)| *last = *op)
                .or_insert((*op, *op));
            new_state = *state;
        }
        let mut out = SubmissionChanges {
            new_state,
            has_more,
            ..SubmissionChanges::default()
        };
        for (id, (first, last)) in seen {
            match (first, last) {
                (Op::Created, Op::Destroyed) => {}
                (Op::Created, _) => out.created.push(id.to_owned()),
                (_, Op::Destroyed) => out.destroyed.push(id.to_owned()),
                _ => out.updated.push(id.to_owned()),
            }
        }
        Ok(out)
    }

    async fn expire_submissions(&self, before: OffsetDateTime, limit: usize) -> Result<usize> {
        let mut st = lock(&self.state);
        let mut old: Vec<(OffsetDateTime, AccountScope, String)> = st
            .iter()
            .flat_map(|(scope, subs)| {
                subs.acc
                    .items
                    .values()
                    .filter(|s| s.undo_status != UndoStatus::Pending && s.send_at < before)
                    .map(|s| (s.send_at, *scope, s.id.clone()))
            })
            .collect();
        old.sort_by(|a, b| (a.0, &a.2).cmp(&(b.0, &b.2)));
        old.truncate(limit);
        for (_, scope, id) in &old {
            if let Some(subs) = st.get_mut(scope)
                && subs.acc.items.remove(id).is_some()
            {
                subs.change(id, Op::Destroyed);
            }
        }
        Ok(old.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::{identity_store, submission_store};
    use tp_contracts::{AccountId, TenantId};

    fn scope() -> AccountScope {
        AccountScope {
            tenant: TenantId::DEFAULT,
            account: AccountId::new(),
        }
    }

    #[tokio::test]
    async fn passes_contract_suites() {
        identity_store(&MemIdentities::new(), scope(), scope()).await;
        submission_store(&MemSubmissions::new(), scope(), scope()).await;
        crate::suite::submission_query(&MemSubmissions::new(), scope(), scope()).await;
    }
}
