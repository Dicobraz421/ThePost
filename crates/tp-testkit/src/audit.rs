//! In-memory журнал аудита ящиков (CONTRACTS.md#MailboxAudit; CTR-35).
//!
//! [`MemMailboxAudit`] применяет политику ящика (ящик → организация → по умолчанию) и хранит записи в порядке
//! записи; [`MemMailboxAudit::entries`] — для проверок в тестах полос.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::audit::{
    AuditCursor, AuditEntry, AuditPage, AuditPolicy, AuditQuery, MailboxAudit,
};
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, Error, Result, TenantId};

use crate::infra::lock;

#[derive(Debug, Default)]
struct State {
    entries: Vec<(u64, AuditEntry)>,
    next_seq: u64,
    policies: HashMap<(TenantId, Option<AccountId>), AuditPolicy>,
    faults: VecDeque<Error>,
}

impl State {
    fn policy(&self, scope: AccountScope) -> AuditPolicy {
        self.policies
            .get(&(scope.tenant, Some(scope.account)))
            .or_else(|| self.policies.get(&(scope.tenant, None)))
            .cloned()
            .unwrap_or_default()
    }
}

/// Журнал аудита ящиков в памяти; клоны разделяют записи.
#[derive(Debug, Clone, Default)]
pub struct MemMailboxAudit {
    state: Arc<Mutex<State>>,
}

impl MemMailboxAudit {
    /// Пустой журнал, политика по умолчанию.
    pub fn new() -> Self {
        Self::default()
    }

    /// Все записанные записи в порядке записи (по всем организациям).
    pub fn entries(&self) -> Vec<AuditEntry> {
        lock(&self.state)
            .entries
            .iter()
            .map(|(_, e)| e.clone())
            .collect()
    }

    /// Следующая операция вернёт `err` (очередь, по одной на операцию).
    pub fn fail_next(&self, err: Error) {
        lock(&self.state).faults.push_back(err);
    }
}

fn fault(st: &mut State) -> Result<()> {
    st.faults.pop_front().map_or(Ok(()), Err)
}

fn position(at: OffsetDateTime, seq: u64) -> (i128, u64) {
    (at.unix_timestamp_nanos(), seq)
}

fn decode(c: &AuditCursor) -> Result<(i128, u64)> {
    let bad = || Error::invalid("invalid audit cursor");
    let (at, seq) = c.0.split_once(':').ok_or_else(bad)?;
    Ok((
        at.parse().map_err(|_| bad())?,
        seq.parse().map_err(|_| bad())?,
    ))
}

#[async_trait]
impl MailboxAudit for MemMailboxAudit {
    async fn record_audit(&self, entry: AuditEntry) -> Result<bool> {
        entry.validate()?;
        let mut st = lock(&self.state);
        fault(&mut st)?;
        if !st
            .policy(entry.scope)
            .records(entry.actor_kind, entry.action)
        {
            return Ok(false);
        }
        let seq = st.next_seq;
        st.next_seq += 1;
        st.entries.push((seq, entry));
        Ok(true)
    }

    async fn audit_policy(&self, scope: AccountScope) -> Result<AuditPolicy> {
        let mut st = lock(&self.state);
        fault(&mut st)?;
        Ok(st.policy(scope))
    }

    async fn set_audit_policy(
        &self,
        tenant: TenantId,
        account: Option<AccountId>,
        policy: Option<AuditPolicy>,
    ) -> Result<()> {
        if let Some(p) = &policy {
            p.validate()?;
        }
        let mut st = lock(&self.state);
        fault(&mut st)?;
        match policy {
            Some(p) => st.policies.insert((tenant, account), p),
            None => st.policies.remove(&(tenant, account)),
        };
        Ok(())
    }

    async fn search_audit(&self, tenant: TenantId, query: &AuditQuery) -> Result<AuditPage> {
        query.validate()?;
        let after = query.after.as_ref().map(decode).transpose()?;
        let mut st = lock(&self.state);
        fault(&mut st)?;
        let mut found: Vec<_> = st
            .entries
            .iter()
            .filter(|(_, e)| e.scope.tenant == tenant && query.matches(e))
            .map(|(seq, e)| (position(e.at, *seq), e))
            .filter(|(pos, _)| after.is_none_or(|a| *pos > a))
            .collect();
        found.sort_by_key(|(pos, _)| *pos);
        let more = found.len() > query.limit;
        found.truncate(query.limit);
        Ok(AuditPage {
            next: more
                .then(|| {
                    found
                        .last()
                        .map(|(p, _)| AuditCursor(format!("{}:{}", p.0, p.1)))
                })
                .flatten(),
            entries: found.into_iter().map(|(_, e)| e.clone()).collect(),
        })
    }

    async fn purge_audit(&self, before: OffsetDateTime) -> Result<()> {
        let mut st = lock(&self.state);
        fault(&mut st)?;
        st.entries.retain(|(_, e)| e.at >= before);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::mailbox_audit;

    #[tokio::test]
    async fn passes_contract_suite() {
        mailbox_audit(&MemMailboxAudit::new(), TenantId::DEFAULT, TenantId::new()).await;
    }

    #[tokio::test]
    async fn purge_and_faults() {
        use tp_contracts::audit::{AuditAction, AuditActorKind, AuditObject};
        let audit = MemMailboxAudit::new();
        let scope = AccountScope {
            tenant: TenantId::DEFAULT,
            account: AccountId::new(),
        };
        let at = |s: i64| OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(s);
        for s in [10, 20] {
            let e = AuditEntry {
                scope,
                at: at(s),
                actor: "admin".into(),
                actor_account: None,
                actor_kind: AuditActorKind::Admin,
                action: AuditAction::Read,
                object: AuditObject::default(),
                client: None,
            };
            assert!(audit.record_audit(e).await.expect("record"));
        }
        audit.purge_audit(at(15)).await.expect("purge");
        let left: Vec<_> = audit.entries().iter().map(|e| e.at).collect();
        assert_eq!(left, [at(20)]);
        audit.fail_next(Error::Unavailable { retry_after: None });
        assert!(audit.audit_policy(scope).await.is_err());
        assert!(audit.audit_policy(scope).await.is_ok());
    }
}
