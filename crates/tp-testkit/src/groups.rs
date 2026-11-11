//! In-memory группы рассылки и очередь модерации (CONTRACTS.md#Groups; SRS 4.5).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tp_contracts::groups::{
    DistributionGroup, GroupMember, GroupStore, HeldMessage, Moderation, check_delivery,
    validate_reject_reason,
};
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
            .filter(|((t, _), h)| *t == tenant && h.group == Some(id))
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
        message.validate()?;
        let mut st = lock(&self.state);
        if let Some(g) = message.group
            && !st.groups.contains_key(&(tenant, g))
        {
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
                .filter(|((t, _), h)| *t == tenant && group.is_none_or(|g| h.group == Some(g)))
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

/// Решение модератора, записанное [`MemModeration`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModerationDecision {
    /// Одобрено: письмо и группа доставки (`None` — письмо правила потока, исходным получателям; CTR-43).
    Deliver(HeldId, Option<GroupId>),
    /// Отклонено: письмо и причина.
    Reject(HeldId, String),
}

#[derive(Debug, Default)]
struct ModerationState {
    decisions: Vec<ModerationDecision>,
    fail: Option<Error>,
}

/// Решения модератора в памяти (CTR-31): записывает решения, [`MemModeration::fail_next`] — ошибка следующего.
/// Клоны разделяют данные.
#[derive(Debug, Clone, Default)]
pub struct MemModeration {
    state: Arc<Mutex<ModerationState>>,
}

impl MemModeration {
    /// Без решений.
    pub fn new() -> Self {
        Self::default()
    }

    /// Принятые решения по порядку.
    pub fn decisions(&self) -> Vec<ModerationDecision> {
        lock(&self.state).decisions.clone()
    }

    /// Следующее решение завершится ошибкой `e` (и не будет записано).
    pub fn fail_next(&self, e: Error) {
        lock(&self.state).fail = Some(e);
    }

    fn push(&self, d: ModerationDecision) -> Result<()> {
        let mut st = lock(&self.state);
        if let Some(e) = st.fail.take() {
            return Err(e);
        }
        st.decisions.push(d);
        Ok(())
    }
}

#[async_trait]
impl Moderation for MemModeration {
    async fn deliver(
        &self,
        tenant: TenantId,
        group: Option<&DistributionGroup>,
        message: &HeldMessage,
    ) -> Result<()> {
        check_delivery(tenant, group, message)?;
        self.push(ModerationDecision::Deliver(message.id, group.map(|g| g.id)))
    }

    async fn reject(&self, _tenant: TenantId, message: &HeldMessage, reason: &str) -> Result<()> {
        validate_reject_reason(reason)?;
        self.push(ModerationDecision::Reject(message.id, reason.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::{GroupFixture, ModerationFixture, group_store, moderation};
    use time::OffsetDateTime;
    use tp_contracts::groups::{HoldSource, SenderRestriction};
    use tp_contracts::{BlobHash, BlobKey, Gen};

    #[tokio::test]
    async fn external_id_survives_rename() {
        let meta = crate::MemMetadata::default();
        let fx = crate::suite::ExternalIdFixture {
            tenant: TenantId::DEFAULT,
            domain: tp_contracts::DomainId::new(),
            domain_name: "example.org".into(),
            foreign_tenant: TenantId::new(),
        };
        crate::suite::account_external_id(&meta, &MemGroups::new(), &fx).await;
    }

    #[tokio::test]
    async fn moderation_records_decisions() {
        let tenant = TenantId::DEFAULT;
        let group = DistributionGroup {
            id: GroupId::new(),
            tenant,
            name: "Все".into(),
            address: Some("all@example.org".into()),
            external_id: None,
            members: vec![GroupMember::Account(AccountId::new())],
            senders: SenderRestriction::Anyone,
            moderators: vec![AccountId::new()],
            hidden_from_gal: false,
        };
        let held = || HeldMessage {
            id: HeldId::new(),
            group: Some(group.id),
            source: HoldSource::Group,
            moderators: Vec::new(),
            recipients: Vec::new(),
            sender: "x@example.com".into(),
            sender_account: None,
            message_id: None,
            blob: BlobKey {
                tenant,
                hash: BlobHash([1; 32]),
                generation: Gen::INITIAL,
            },
            size: 10,
            held_at: OffsetDateTime::UNIX_EPOCH,
        };
        let fx = ModerationFixture {
            tenant,
            group: group.clone(),
            approve: held(),
            decline: held(),
        };
        let m = MemModeration::new();
        moderation(&m, &fx).await;
        assert_eq!(
            m.decisions(),
            [
                ModerationDecision::Deliver(fx.approve.id, Some(group.id)),
                ModerationDecision::Reject(fx.decline.id, "off-topic".into()),
            ]
        );
        m.fail_next(Error::UNAVAILABLE);
        assert_eq!(
            m.deliver(tenant, Some(&group), &fx.approve).await,
            Err(Error::UNAVAILABLE)
        );
        assert_eq!(m.decisions().len(), 2);
    }

    /// CTR-43: общий набор очереди модерации для правил потока.
    #[tokio::test]
    async fn rule_moderation_suite() {
        let groups = MemGroups::new();
        let m = MemModeration::new();
        let fx = crate::suite::RuleModerationFixture {
            tenant: TenantId::DEFAULT,
            moderator: AccountId::new(),
            recipients: vec!["a@example.org".into(), "b@example.net".into()],
        };
        let (approved, declined) = crate::suite::rule_moderation(&groups, &m, &fx).await;
        assert_eq!(
            m.decisions(),
            [
                ModerationDecision::Deliver(approved, None),
                ModerationDecision::Reject(declined, "not allowed".into()),
            ]
        );
    }

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
