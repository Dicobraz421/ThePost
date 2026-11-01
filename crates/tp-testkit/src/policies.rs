//! In-memory политики: уровни цепочки и выбор пользователя `trash.purge` (CONTRACTS.md#Tenant,
//! #Trash, Retention, Quota (Ф1); решение 2026-11-01-trash-purge).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tp_contracts::meta::{AccountScope, AccountStore, Policy, PolicyResolver};
use tp_contracts::retention::{TrashPurge, TrashPurgeRule, TrashPurgeSetting};
use tp_contracts::{AccountId, DomainId, Error, MailboxId, Result, TenantId};

use crate::infra::lock;
use crate::meta::MemMetadata;
use crate::suite::TrashPurgeAdmin;

/// Уровень цепочки политик.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyLevel {
    /// Организация.
    Tenant,
    /// Домен.
    Domain(DomainId),
    /// Учётная запись (ящик).
    Account(AccountId),
    /// Папка.
    Mailbox(MailboxId),
}

#[derive(Debug, Default)]
struct PolicyState {
    levels: HashMap<(TenantId, PolicyLevel), Policy>,
    users: HashMap<(TenantId, AccountId), TrashPurge>,
}

/// [`PolicyResolver`] в памяти: политики уровней задаёт тест, домен учётной записи — из [`MemMetadata`].
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

    /// Политика уровня без наложения; не задана — все поля `None`.
    pub fn policy(&self, tenant: TenantId, level: PolicyLevel) -> Policy {
        lock(&self.state)
            .levels
            .get(&(tenant, level))
            .cloned()
            .unwrap_or_default()
    }

    /// Задаёт политику уровня целиком; политика без заданных полей удаляет уровень.
    pub fn set_policy(&self, tenant: TenantId, level: PolicyLevel, policy: Policy) {
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
        level: PolicyLevel,
        rule: Option<TrashPurgeRule>,
    ) {
        let mut policy = self.policy(tenant, level);
        policy.trash_purge = rule;
        self.set_policy(tenant, level, policy);
    }

    async fn domain_of(&self, scope: AccountScope) -> Result<DomainId> {
        Ok(AccountStore::get(&self.meta, scope.tenant, scope.account)
            .await?
            .ok_or(Error::NotFound)?
            .domain)
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
            Some(PolicyLevel::Tenant),
            domain.map(PolicyLevel::Domain),
            account.map(PolicyLevel::Account),
            mailbox.map(PolicyLevel::Mailbox),
        ];
        Ok(levels
            .into_iter()
            .flatten()
            .fold(Policy::default(), |p, level| {
                p.overlay(self.policy(tenant, level))
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
        self.set_trash_purge_rule(tenant, PolicyLevel::Tenant, rule);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::MemTrash;
    use crate::infra::ManualClock;
    use crate::suite::{TrashPurgeFixture, trash_purge};
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
            PolicyLevel::Tenant,
            Policy {
                quota_bytes: Some(10),
                ..Policy::default()
            },
        );
        p.set_trash_purge_rule(s.tenant, PolicyLevel::Tenant, Some(erase));
        p.set_trash_purge_rule(s.tenant, PolicyLevel::Domain(domain), Some(choose));
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
        p.set_trash_purge_rule(s.tenant, PolicyLevel::Account(s.account), Some(hide_only));
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
        p.set_trash_purge_rule(s.tenant, PolicyLevel::Account(s.account), None);
        p.set_trash_purge_rule(s.tenant, PolicyLevel::Domain(domain), None);
        p.set_trash_purge_rule(s.tenant, PolicyLevel::Tenant, None);
        assert_eq!(
            p.policy(s.tenant, PolicyLevel::Domain(domain)),
            Policy::default(),
            "пустой уровень удалён"
        );
        let other = AccountScope {
            tenant: s.tenant,
            account: AccountId::new(),
        };
        assert_eq!(p.trash_purge(other).await, Err(Error::NotFound));
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
