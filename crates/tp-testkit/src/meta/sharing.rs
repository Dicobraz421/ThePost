//! Представители, правила бронирования ресурсов и политика общего доступа на [`MemMetadata`] (CTR-48,
//! CONTRACTS.md#Sharing). Записи — у учётной записи: удаление владельца удаляет их, удалённый представитель не
//! виден (как внешние ключи в PostgreSQL); смена вида на пользователя снимает правила бронирования
//! ([`AccountStore::upsert`]).

use async_trait::async_trait;
use tp_contracts::meta::{AccountKind, AccountScope};
use tp_contracts::sharing::{
    BookingPolicy, Delegate, DelegateStore, Delegation, ResourceStore, SharingPolicy,
    SharingPolicyStore,
};
use tp_contracts::{AccountId, Error, Result, TenantId};

#[cfg(doc)]
use tp_contracts::meta::AccountStore;

use super::MemMetadata;
use crate::infra::lock;

#[async_trait]
impl DelegateStore for MemMetadata {
    async fn delegates(&self, owner: AccountScope) -> Result<Vec<Delegate>> {
        let st = lock(&self.state);
        let acc = st
            .accounts
            .get(&(owner.tenant, owner.account))
            .ok_or(Error::NotFound)?;
        Ok(acc
            .delegates
            .values()
            .filter(|d| st.accounts.contains_key(&(owner.tenant, d.delegate)))
            .copied()
            .collect())
    }

    async fn set_delegate(&self, owner: AccountScope, delegate: Delegate) -> Result<()> {
        if delegate.delegate == owner.account {
            return Err(Error::invalid("учётная запись — сама себе представитель"));
        }
        let mut st = lock(&self.state);
        if !st.accounts.contains_key(&(owner.tenant, delegate.delegate)) {
            return Err(Error::NotFound);
        }
        st.accounts
            .get_mut(&(owner.tenant, owner.account))
            .ok_or(Error::NotFound)?
            .delegates
            .insert(delegate.delegate, delegate);
        Ok(())
    }

    async fn remove_delegate(&self, owner: AccountScope, delegate: AccountId) -> Result<()> {
        lock(&self.state)
            .accounts
            .get_mut(&(owner.tenant, owner.account))
            .ok_or(Error::NotFound)?
            .delegates
            .remove(&delegate);
        Ok(())
    }

    async fn delegations(&self, delegate: AccountScope) -> Result<Vec<Delegation>> {
        let st = lock(&self.state);
        if !st
            .accounts
            .contains_key(&(delegate.tenant, delegate.account))
        {
            return Ok(Vec::new());
        }
        let mut out: Vec<Delegation> = st
            .accounts
            .iter()
            .filter(|((tenant, _), _)| *tenant == delegate.tenant)
            .filter_map(|(&(_, owner), acc)| {
                acc.delegates.get(&delegate.account).map(|d| Delegation {
                    owner,
                    level: d.level,
                    schedule_on_behalf: d.schedule_on_behalf,
                })
            })
            .collect();
        out.sort_by_key(|d| d.owner);
        Ok(out)
    }
}

#[async_trait]
impl ResourceStore for MemMetadata {
    async fn booking_policy(&self, scope: AccountScope) -> Result<Option<BookingPolicy>> {
        let st = lock(&self.state);
        let acc = st
            .accounts
            .get(&(scope.tenant, scope.account))
            .ok_or(Error::NotFound)?;
        Ok(acc.booking.clone())
    }

    async fn set_booking_policy(
        &self,
        scope: AccountScope,
        policy: Option<BookingPolicy>,
    ) -> Result<()> {
        if let Some(p) = &policy {
            p.validate()?;
        }
        let mut st = lock(&self.state);
        let acc = st
            .accounts
            .get_mut(&(scope.tenant, scope.account))
            .ok_or(Error::NotFound)?;
        if acc.account.kind != AccountKind::Resource {
            return Err(Error::invalid(
                "правила бронирования — только у учётной записи-ресурса",
            ));
        }
        acc.booking = policy;
        Ok(())
    }

    async fn resources(&self, tenant: TenantId) -> Result<Vec<AccountId>> {
        let mut out: Vec<AccountId> = lock(&self.state)
            .accounts
            .values()
            .filter(|a| a.account.tenant == tenant && a.account.kind == AccountKind::Resource)
            .map(|a| a.account.id)
            .collect();
        out.sort();
        Ok(out)
    }
}

#[async_trait]
impl SharingPolicyStore for MemMetadata {
    async fn sharing_policy(&self, tenant: TenantId) -> Result<SharingPolicy> {
        Ok(lock(&self.state)
            .sharing
            .get(&tenant)
            .cloned()
            .unwrap_or_default())
    }

    async fn set_sharing_policy(&self, tenant: TenantId, policy: SharingPolicy) -> Result<()> {
        lock(&self.state).sharing.insert(tenant, policy);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tp_contracts::meta::{AccountStore, AccountUpsert};
    use tp_contracts::{DomainId, TenantId};

    use super::*;
    use crate::suite::sharing::{
        SharingFixture, delegate_store, resource_store, sharing_policy_store,
    };

    async fn upsert(
        meta: &MemMetadata,
        tenant: TenantId,
        login: &str,
        kind: AccountKind,
    ) -> AccountScope {
        let acc = meta
            .upsert(
                tenant,
                AccountUpsert {
                    domain: DomainId::new(),
                    login: login.into(),
                    display_name: login.into(),
                    external_id: None,
                    enabled: true,
                    aliases: Vec::new(),
                    kind,
                    hidden_from_gal: false,
                    resource_type: None,
                },
            )
            .await
            .expect("upsert");
        AccountScope {
            tenant,
            account: acc.id,
        }
    }

    async fn fixture(meta: &MemMetadata) -> SharingFixture {
        let tenant = TenantId::new();
        SharingFixture {
            owner: upsert(meta, tenant, "owner@example.com", AccountKind::User).await,
            peer: upsert(meta, tenant, "peer@example.com", AccountKind::User).await,
            resource: upsert(meta, tenant, "room@example.com", AccountKind::Resource).await,
            foreign: upsert(
                meta,
                TenantId::new(),
                "owner@example.com",
                AccountKind::User,
            )
            .await,
        }
    }

    #[tokio::test]
    async fn suite_delegates() {
        let meta = MemMetadata::default();
        let fx = fixture(&meta).await;
        delegate_store(&meta, &fx).await;
    }

    #[tokio::test]
    async fn suite_resources() {
        let meta = MemMetadata::default();
        let fx = fixture(&meta).await;
        resource_store(&meta, &meta, &fx).await;
    }

    #[tokio::test]
    async fn suite_sharing_policy() {
        let meta = MemMetadata::default();
        let fx = fixture(&meta).await;
        sharing_policy_store(&meta, fx.owner.tenant, fx.foreign.tenant).await;
    }

    #[tokio::test]
    async fn removed_accounts_drop_delegates() {
        let meta = MemMetadata::default();
        let fx = fixture(&meta).await;
        let d = Delegate {
            delegate: fx.peer.account,
            level: tp_contracts::sharing::DelegateLevel::Read,
            schedule_on_behalf: false,
        };
        meta.set_delegate(fx.owner, d).await.expect("set");
        lock(&meta.state)
            .accounts
            .remove(&(fx.peer.tenant, fx.peer.account));
        assert_eq!(meta.delegates(fx.owner).await, Ok(Vec::new()));
        lock(&meta.state)
            .accounts
            .remove(&(fx.owner.tenant, fx.owner.account));
        assert_eq!(meta.delegates(fx.owner).await, Err(Error::NotFound));
    }
}
