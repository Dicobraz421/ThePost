//! In-memory политики: уровни цепочки ([`PolicyStore`], CTR-25), разрешение ([`PolicyResolver`]) и выбор
//! пользователя `trash.purge` (CONTRACTS.md#Tenant, #Trash, Retention, Quota (Ф1); решение 2026-11-01-trash-purge).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tp_contracts::meta::{
    AccountScope, AccountStore, Policy, PolicyResolver, PolicyStore, PolicyTarget,
};
use tp_contracts::retention::{TrashPurge, TrashPurgeRule, TrashPurgeSetting};
use tp_contracts::{AccountId, DomainId, Error, MailboxId, Result, TenantId};

use crate::infra::lock;
use crate::meta::MemMetadata;
use crate::suite::TrashPurgeAdmin;

#[derive(Debug, Default)]
struct PolicyState {
    levels: HashMap<(TenantId, PolicyTarget), Policy>,
    users: HashMap<(TenantId, AccountId), TrashPurge>,
}

/// [`PolicyStore`] и [`PolicyResolver`] в памяти; домен учётной записи — из [`MemMetadata`].
/// Клоны разделяют данные.
#[derive(Debug, Clone)]
pub struct MemPolicies {
    meta: MemMetadata,
    state: Arc<Mutex<PolicyState>>,
}

impl MemPolicies {
    /// Политики учётных записей `meta`; ни один уровень не задан.
    pub fn new(meta: &MemMetadata) -> Self {
        Self {
            meta: meta.clone(),
            state: Arc::default(),
        }
    }

    fn level(&self, tenant: TenantId, level: PolicyTarget) -> Policy {
        lock(&self.state)
            .levels
            .get(&(tenant, level))
            .cloned()
            .unwrap_or_default()
    }

    fn put(&self, tenant: TenantId, level: PolicyTarget, policy: Policy) {
        let mut st = lock(&self.state);
        if policy == Policy::default() {
            st.levels.remove(&(tenant, level));
        } else {
            st.levels.insert((tenant, level), policy);
        }
    }

    /// Задаёт правило `trash.purge` уровня, сохраняя остальные поля его политики.
    pub fn set_trash_purge_rule(
        &self,
        tenant: TenantId,
        level: PolicyTarget,
        rule: Option<TrashPurgeRule>,
    ) {
        let mut policy = self.level(tenant, level);
        policy.trash_purge = rule;
        self.put(tenant, level, policy);
    }

    async fn domain_of(&self, scope: AccountScope) -> Result<DomainId> {
        Ok(AccountStore::get(&self.meta, scope.tenant, scope.account)
            .await?
            .ok_or(Error::NotFound)?
            .domain)
    }
}

#[async_trait]
impl PolicyStore for MemPolicies {
    async fn policy(&self, tenant: TenantId, target: PolicyTarget) -> Result<Policy> {
        Ok(self.level(tenant, target))
    }

    async fn set_policy(
        &self,
        tenant: TenantId,
        target: PolicyTarget,
        policy: &Policy,
    ) -> Result<()> {
        self.put(tenant, target, policy.clone());
        Ok(())
    }
}

#[async_trait]
impl PolicyResolver for MemPolicies {
    async fn resolve(
        &self,
        tenant: TenantId,
        domain: Option<DomainId>,
        account: Option<AccountId>,
        mailbox: Option<MailboxId>,
    ) -> Result<Policy> {
        let levels = [
            Some(PolicyTarget::Tenant),
            domain.map(PolicyTarget::Domain),
            account.map(PolicyTarget::Account),
            mailbox.map(PolicyTarget::Mailbox),
        ];
        Ok(levels
            .into_iter()
            .flatten()
            .fold(Policy::default(), |p, level| {
                p.overlay(self.level(tenant, level))
            }))
    }

    async fn trash_purge(&self, scope: AccountScope) -> Result<TrashPurgeSetting> {
        let domain = self.domain_of(scope).await?;
        let rule = self
            .resolve(scope.tenant, Some(domain), Some(scope.account), None)
            .await?
            .trash_purge
            .unwrap_or_default();
        let user = lock(&self.state)
            .users
            .get(&(scope.tenant, scope.account))
            .copied();
        Ok(TrashPurgeSetting { rule, user })
    }

    async fn set_user_trash_purge(
        &self,
        scope: AccountScope,
        mode: Option<TrashPurge>,
    ) -> Result<()> {
        let setting = self.trash_purge(scope).await?;
        let key = (scope.tenant, scope.account);
        match mode {
            None => {
                lock(&self.state).users.remove(&key);
            }
            Some(_) if !setting.rule.user_can_choose => return Err(Error::Forbidden),
            Some(mode) => {
                lock(&self.state).users.insert(key, mode);
            }
        }
        Ok(())
    }
}

#[async_trait]
impl TrashPurgeAdmin for MemPolicies {
    async fn set_tenant_rule(&self, tenant: TenantId, rule: Option<TrashPurgeRule>) {
        self.set_trash_purge_rule(tenant, PolicyTarget::Tenant, rule);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::MemTrash;
    use crate::infra::ManualClock;
    use crate::suite::{PolicyFixture, TrashPurgeFixture, policy_store, trash_purge};
    use crate::{MemMetadata, MemQuota};

    #[tokio::test]
    async fn chain_and_user_choice() {
        let meta = MemMetadata::new(ManualClock::default().shared());
        let s = meta.account("user@example.com").await.expect("account");
        let domain = AccountStore::get(&meta, s.tenant, s.account)
            .await
            .expect("get")
            .expect("account")
            .domain;
        let p = MemPolicies::new(&meta);
        let erase = TrashPurgeRule {
            mode: TrashPurge::Erase,
            user_can_choose: false,
        };
        let choose = TrashPurgeRule {
            mode: TrashPurge::Hide,
            user_can_choose: true,
        };
        p.set_policy(
            s.tenant,
            PolicyTarget::Tenant,
            &Policy {
                quota_bytes: Some(10),
                ..Policy::default()
            },
        )
        .await
        .expect("set");
        p.set_trash_purge_rule(s.tenant, PolicyTarget::Tenant, Some(erase));
        p.set_trash_purge_rule(s.tenant, PolicyTarget::Domain(domain), Some(choose));
        let resolved = p
            .resolve(s.tenant, Some(domain), Some(s.account), None)
            .await
            .expect("resolve");
        assert_eq!(resolved.quota_bytes, Some(10), "остальные поля сохранены");
        assert_eq!(
            resolved.trash_purge,
            Some(choose),
            "домен уточняет организацию"
        );
        p.set_user_trash_purge(s, Some(TrashPurge::Erase))
            .await
            .expect("choose");
        assert_eq!(
            p.trash_purge(s).await.expect("setting").effective(),
            TrashPurge::Erase
        );
        // Ящик без выбора пользователя: значение администратора, выбор хранится, но не действует.
        let hide_only = TrashPurgeRule::default();
        p.set_trash_purge_rule(s.tenant, PolicyTarget::Account(s.account), Some(hide_only));
        let setting = p.trash_purge(s).await.expect("setting");
        assert_eq!(
            (setting.user, setting.effective()),
            (Some(TrashPurge::Erase), TrashPurge::Hide)
        );
        assert_eq!(
            p.set_user_trash_purge(s, Some(TrashPurge::Hide)).await,
            Err(Error::Forbidden)
        );
        assert_eq!(p.set_user_trash_purge(s, None).await, Ok(()));
        p.set_trash_purge_rule(s.tenant, PolicyTarget::Account(s.account), None);
        p.set_trash_purge_rule(s.tenant, PolicyTarget::Domain(domain), None);
        p.set_trash_purge_rule(s.tenant, PolicyTarget::Tenant, None);
        assert_eq!(
            p.policy(s.tenant, PolicyTarget::Domain(domain)).await,
            Ok(Policy::default()),
            "пустой уровень удалён"
        );
        let other = AccountScope {
            tenant: s.tenant,
            account: AccountId::new(),
        };
        assert_eq!(p.trash_purge(other).await, Err(Error::NotFound));
    }

    #[tokio::test]
    async fn suite_policy_store() {
        let meta = MemMetadata::new(ManualClock::default().shared());
        let p = MemPolicies::new(&meta);
        let fx = PolicyFixture {
            tenant: TenantId::new(),
            foreign_tenant: TenantId::new(),
            domain: DomainId::new(),
            account: AccountId::new(),
            mailbox: MailboxId::new(),
        };
        policy_store(&p, &p, &fx).await;
    }

    #[tokio::test]
    async fn suite_trash_purge() {
        let meta = MemMetadata::new(ManualClock::default().shared());
        let scope = meta.account("purge@example.com").await.expect("account");
        let policies = MemPolicies::new(&meta);
        let trash = MemTrash::new(&meta).with_policies(policies.clone());
        let quota = MemQuota::new(&meta);
        trash_purge(
            &trash,
            &meta,
            &meta,
            &quota,
            &policies,
            &policies,
            &TrashPurgeFixture { scope },
        )
        .await;
    }
}
