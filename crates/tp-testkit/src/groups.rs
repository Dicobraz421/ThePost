//! In-memory группы рассылки и очередь модерации (CONTRACTS.md#Groups; SRS 4.5).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tp_contracts::groups::{DistributionGroup, GroupMember, GroupStore, HeldMessage};
use tp_contracts::{AccountId, Error, GroupId, HeldId, Result, TenantId};

use crate::infra::lock;

#[derive(Debug, Default)]
struct State {
    groups: BTreeMap<(TenantId, GroupId), DistributionGroup>,
    held: BTreeMap<(TenantId, HeldId), HeldMessage>,
}

/// Группы рассылки в памяти; клоны разделяют данные.
#[derive(Debug, Clone, Default)]
pub struct MemGroups {
    state: Arc<Mutex<State>>,
}

impl MemGroups {
    /// Пустое хранилище.
    pub fn new() -> Self {
        Self::default()
    }
}

fn sorted(mut held: Vec<HeldMessage>) -> Vec<HeldMessage> {
    held.sort_by_key(|h| (h.held_at, h.id));
    held
}

#[async_trait]
impl GroupStore for MemGroups {
    async fn get(&self, tenant: TenantId, id: GroupId) -> Result<Option<DistributionGroup>> {
        Ok(lock(&self.state).groups.get(&(tenant, id)).cloned())
    }

    async fn by_address(
        &self,
        tenant: TenantId,
        address: &str,
    ) -> Result<Option<DistributionGroup>> {
        Ok(lock(&self.state)
            .groups
            .values()
            .find(|g| {
                g.tenant == tenant
                    && g.address
                        .as_deref()
                        .is_some_and(|a| a.eq_ignore_ascii_case(address))
            })
            .cloned())
    }

    async fn by_external_id(
        &self,
        tenant: TenantId,
        external_id: &str,
    ) -> Result<Option<DistributionGroup>> {
        Ok(lock(&self.state)
            .groups
            .values()
            .find(|g| g.tenant == tenant && g.external_id.as_deref() == Some(external_id))
            .cloned())
    }

    async fn list(&self, tenant: TenantId) -> Result<Vec<DistributionGroup>> {
        Ok(lock(&self.state)
            .groups
            .iter()
            .filter(|((t, _), _)| *t == tenant)
            .map(|(_, g)| g.clone())
            .collect())
    }

    async fn put(&self, group: DistributionGroup) -> Result<()> {
        group.validate()?;
        let mut st = lock(&self.state);
        let taken = st.groups.values().any(|g| {
            g.tenant == group.tenant
                && g.id != group.id
                && (g
                    .address
                    .as_deref()
                    .zip(group.address.as_deref())
                    .is_some_and(|(a, b)| a.eq_ignore_ascii_case(b))
                    || (g.external_id.is_some() && g.external_id == group.external_id))
        });
        if taken {
            return Err(Error::Conflict);
        }
        st.groups.insert((group.tenant, group.id), group);
        Ok(())
    }

    async fn remove(&self, tenant: TenantId, id: GroupId) -> Result<Vec<HeldMessage>> {
        let mut st = lock(&self.state);
        st.groups.remove(&(tenant, id));
        let ids: Vec<_> = st
            .held
            .iter()
            .filter(|((t, _), h)| *t == tenant && h.group == id)
            .map(|(k, _)| *k)
            .collect();
        Ok(sorted(
            ids.iter().filter_map(|k| st.held.remove(k)).collect(),
        ))
    }

    async fn expand(&self, tenant: TenantId, id: GroupId) -> Result<Vec<AccountId>> {
        let st = lock(&self.state);
        if !st.groups.contains_key(&(tenant, id)) {
            return Err(Error::NotFound);
        }
        let mut seen = BTreeSet::from([id]);
        let mut queue = VecDeque::from([id]);
        let mut accounts = BTreeSet::new();
        while let Some(g) = queue.pop_front() {
            let Some(group) = st.groups.get(&(tenant, g)) else {
                continue; // неизвестная вложенная группа
            };
            for m in &group.members {
                match *m {
                    GroupMember::Account(a) => {
                        accounts.insert(a);
                    }
                    GroupMember::Group(n) => {
                        if seen.insert(n) {
                            queue.push_back(n);
                        }
                    }
                }
            }
        }
        Ok(accounts.into_iter().collect())
    }

    async fn hold(&self, tenant: TenantId, message: HeldMessage) -> Result<()> {
        let mut st = lock(&self.state);
        if !st.groups.contains_key(&(tenant, message.group)) {
            return Err(Error::NotFound);
        }
        if st.held.contains_key(&(tenant, message.id)) {
            return Err(Error::Conflict);
        }
        st.held.insert((tenant, message.id), message);
        Ok(())
    }

    async fn held(
        &self,
        tenant: TenantId,
        group: Option<GroupId>,
        limit: usize,
    ) -> Result<Vec<HeldMessage>> {
        let st = lock(&self.state);
        let mut found = sorted(
            st.held
                .iter()
                .filter(|((t, _), h)| *t == tenant && group.is_none_or(|g| h.group == g))
                .map(|(_, h)| h.clone())
                .collect(),
        );
        found.truncate(limit);
        Ok(found)
    }

    async fn take_held(&self, tenant: TenantId, id: HeldId) -> Result<HeldMessage> {
        lock(&self.state)
            .held
            .remove(&(tenant, id))
            .ok_or(Error::NotFound)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::{GroupFixture, group_store};

    #[tokio::test]
    async fn passes_contract_suite() {
        group_store(
            &MemGroups::new(),
            &GroupFixture {
                tenant: TenantId::DEFAULT,
                accounts: [(); 4].map(|()| AccountId::new()),
                foreign_tenant: TenantId::new(),
            },
        )
        .await;
    }
}
