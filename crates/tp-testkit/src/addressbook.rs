//! In-memory адресная книга: контакты, оргструктура, динамические группы, остановка синхронизации каталога
//! (CONTRACTS.md#AddressBook, #Directory; CTR-35).

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::addressbook::{
    ContactStore, DynamicGroup, DynamicGroupStore, FilterTarget, MailContact, Members,
    OrgAttributes, OrgMember, OrgStore, OrgUnit, check_unit, sort_members,
};
use tp_contracts::auth::{MassRemoval, SyncGate};
use tp_contracts::{AccountId, ContactId, Error, GroupId, OrgUnitId, Result, TenantId};

use crate::infra::lock;

#[derive(Debug, Default)]
struct Book {
    contacts: BTreeMap<ContactId, MailContact>,
    units: BTreeMap<OrgUnitId, OrgUnit>,
    org: BTreeMap<AccountId, OrgAttributes>,
    dynamic: BTreeMap<GroupId, DynamicGroup>,
}

impl Book {
    /// Подразделение и вышестоящие, начиная со своего (цикл обрывается).
    fn chain(&self, unit: Option<OrgUnitId>) -> Vec<OrgUnitId> {
        let mut out = Vec::new();
        let mut next = unit;
        while let Some(cur) = next {
            if out.contains(&cur) {
                break;
            }
            out.push(cur);
            next = self.units.get(&cur).and_then(|u| u.parent);
        }
        out
    }
}

/// [`ContactStore`], [`OrgStore`] и [`DynamicGroupStore`] в памяти; клоны разделяют данные.
#[derive(Debug, Clone, Default)]
pub struct MemAddressBook {
    books: Arc<Mutex<HashMap<TenantId, Book>>>,
}

impl MemAddressBook {
    /// Пустая адресная книга.
    pub fn new() -> Self {
        Self::default()
    }

    fn with<R>(&self, tenant: TenantId, f: impl FnOnce(&mut Book) -> R) -> R {
        f(lock(&self.books).entry(tenant).or_default())
    }
}

#[async_trait]
impl ContactStore for MemAddressBook {
    async fn contact(&self, tenant: TenantId, id: ContactId) -> Result<Option<MailContact>> {
        Ok(self.with(tenant, |b| b.contacts.get(&id).cloned()))
    }

    async fn contact_by_address(
        &self,
        tenant: TenantId,
        email: &str,
    ) -> Result<Option<MailContact>> {
        Ok(self.with(tenant, |b| {
            b.contacts
                .values()
                .find(|c| c.email.eq_ignore_ascii_case(email))
                .cloned()
        }))
    }

    async fn contact_by_external_id(
        &self,
        tenant: TenantId,
        external_id: &str,
    ) -> Result<Option<MailContact>> {
        Ok(self.with(tenant, |b| {
            b.contacts
                .values()
                .find(|c| c.external_id.as_deref() == Some(external_id))
                .cloned()
        }))
    }

    async fn contacts(&self, tenant: TenantId) -> Result<Vec<MailContact>> {
        Ok(self.with(tenant, |b| b.contacts.values().cloned().collect()))
    }

    async fn put_contact(&self, mut contact: MailContact) -> Result<()> {
        contact.validate()?;
        contact.groups.sort_unstable();
        contact.groups.dedup();
        self.with(contact.tenant, |b| {
            let clash = b.contacts.values().any(|c| {
                c.id != contact.id
                    && (c.email.eq_ignore_ascii_case(&contact.email)
                        || (c.external_id.is_some() && c.external_id == contact.external_id))
            });
            if clash {
                return Err(Error::Conflict);
            }
            b.contacts.insert(contact.id, contact);
            Ok(())
        })
    }

    async fn remove_contact(&self, tenant: TenantId, id: ContactId) -> Result<()> {
        self.with(tenant, |b| {
            b.contacts.remove(&id).map(|_| ()).ok_or(Error::NotFound)
        })
    }

    async fn group_contacts(&self, tenant: TenantId, group: GroupId) -> Result<Vec<MailContact>> {
        Ok(self.with(tenant, |b| {
            b.contacts
                .values()
                .filter(|c| c.groups.contains(&group))
                .cloned()
                .collect()
        }))
    }
}

#[async_trait]
impl OrgStore for MemAddressBook {
    async fn org_units(&self, tenant: TenantId) -> Result<Vec<OrgUnit>> {
        Ok(self.with(tenant, |b| b.units.values().cloned().collect()))
    }

    async fn put_org_unit(&self, unit: OrgUnit) -> Result<()> {
        self.with(unit.tenant, |b| {
            let units: Vec<OrgUnit> = b.units.values().cloned().collect();
            check_unit(&units, &unit)?;
            let clash = b.units.values().any(|u| {
                u.id != unit.id && u.external_id.is_some() && u.external_id == unit.external_id
            });
            if clash {
                return Err(Error::Conflict);
            }
            b.units.insert(unit.id, unit);
            Ok(())
        })
    }

    async fn remove_org_unit(&self, tenant: TenantId, id: OrgUnitId) -> Result<()> {
        self.with(tenant, |b| {
            if !b.units.contains_key(&id) {
                return Err(Error::NotFound);
            }
            if b.units.values().any(|u| u.parent == Some(id)) {
                return Err(Error::Conflict);
            }
            b.units.remove(&id);
            for org in b.org.values_mut() {
                if org.department == Some(id) {
                    org.department = None;
                }
            }
            b.org.retain(|_, o| *o != OrgAttributes::default());
            Ok(())
        })
    }

    async fn org(&self, tenant: TenantId, account: AccountId) -> Result<OrgAttributes> {
        Ok(self.with(tenant, |b| b.org.get(&account).cloned().unwrap_or_default()))
    }

    async fn set_org(
        &self,
        tenant: TenantId,
        account: AccountId,
        org: OrgAttributes,
    ) -> Result<()> {
        org.validate()?;
        if org.manager == Some(account) {
            return Err(Error::invalid(
                "org attributes: account cannot be its own manager",
            ));
        }
        self.with(tenant, |b| {
            if org.department.is_some_and(|d| !b.units.contains_key(&d)) {
                return Err(Error::invalid("org attributes: unknown department"));
            }
            if org == OrgAttributes::default() {
                b.org.remove(&account);
            } else {
                b.org.insert(account, org);
            }
            Ok(())
        })
    }

    async fn org_members(
        &self,
        tenant: TenantId,
        unit: Option<OrgUnitId>,
        subtree: bool,
    ) -> Result<Vec<OrgMember>> {
        self.with(tenant, |b| {
            if unit.is_some_and(|u| !b.units.contains_key(&u)) {
                return Err(Error::NotFound);
            }
            let mut out: Vec<OrgMember> = b
                .org
                .iter()
                .filter(|(_, o)| match unit {
                    None => true,
                    Some(u) if subtree => b.chain(o.department).contains(&u),
                    Some(u) => o.department == Some(u),
                })
                .map(|(a, o)| OrgMember {
                    account: *a,
                    org: o.clone(),
                })
                .collect();
            sort_members(&mut out);
            Ok(out)
        })
    }
}

#[async_trait]
impl DynamicGroupStore for MemAddressBook {
    async fn dynamic(&self, tenant: TenantId, group: GroupId) -> Result<Option<DynamicGroup>> {
        Ok(self.with(tenant, |b| b.dynamic.get(&group).cloned()))
    }

    async fn dynamic_groups(&self, tenant: TenantId) -> Result<Vec<DynamicGroup>> {
        Ok(self.with(tenant, |b| b.dynamic.values().cloned().collect()))
    }

    async fn put_dynamic(&self, group: DynamicGroup) -> Result<()> {
        group.filter.validate()?;
        self.with(group.tenant, |b| {
            b.dynamic.insert(group.group, group);
        });
        Ok(())
    }

    async fn remove_dynamic(&self, tenant: TenantId, group: GroupId) -> Result<()> {
        self.with(tenant, |b| {
            b.dynamic.remove(&group).map(|_| ()).ok_or(Error::NotFound)
        })
    }

    async fn expand_dynamic(&self, tenant: TenantId, group: GroupId) -> Result<Members> {
        self.with(tenant, |b| {
            let g = b.dynamic.get(&group).ok_or(Error::NotFound)?;
            let accounts = b
                .org
                .iter()
                .filter(|(_, o)| {
                    let attrs = o.filter_attributes();
                    let chain = b.chain(o.department);
                    g.filter.matches(FilterTarget {
                        attributes: &attrs,
                        units: &chain,
                    })
                })
                .map(|(a, _)| *a)
                .collect();
            let contacts = if g.contacts {
                b.contacts
                    .values()
                    .filter(|c| {
                        g.filter.matches(FilterTarget {
                            attributes: &c.attributes,
                            units: &[],
                        })
                    })
                    .map(|c| c.id)
                    .collect()
            } else {
                Vec::new()
            };
            Ok(Members { accounts, contacts })
        })
    }
}

/// [`SyncGate`] в памяти; клоны разделяют данные.
#[derive(Debug, Clone, Default)]
pub struct MemSyncGate {
    holds: Arc<Mutex<HashMap<TenantId, MassRemoval>>>,
}

impl MemSyncGate {
    /// Без остановок.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SyncGate for MemSyncGate {
    async fn hold_sync(
        &self,
        tenant: TenantId,
        removed: u64,
        total: u64,
        at: OffsetDateTime,
    ) -> Result<MassRemoval> {
        let mut st = lock(&self.holds);
        let approved = st
            .get(&tenant)
            .is_some_and(|h| h.approved && removed <= h.removed);
        let hold = MassRemoval {
            removed,
            total,
            detected_at: at,
            approved,
        };
        st.insert(tenant, hold.clone());
        Ok(hold)
    }

    async fn pending_sync(&self, tenant: TenantId) -> Result<Option<MassRemoval>> {
        Ok(lock(&self.holds).get(&tenant).cloned())
    }

    async fn approve_sync(&self, tenant: TenantId) -> Result<MassRemoval> {
        let mut st = lock(&self.holds);
        let h = st.get_mut(&tenant).ok_or(Error::NotFound)?;
        h.approved = true;
        Ok(h.clone())
    }

    async fn clear_sync(&self, tenant: TenantId) -> Result<()> {
        lock(&self.holds).remove(&tenant);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite;

    #[tokio::test]
    async fn passes_contract_suites() {
        let t = TenantId::DEFAULT;
        let accounts: Vec<_> = (0..3).map(|_| AccountId::new()).collect();
        suite::contact_store(&MemAddressBook::new(), t, TenantId::new()).await;
        suite::org_store(&MemAddressBook::new(), t, &accounts).await;
        let book = MemAddressBook::new();
        let fx = suite::DynamicFixture {
            tenant: t,
            group: GroupId::new(),
            accounts,
        };
        suite::dynamic_groups(&book, &book, &book, &fx).await;
        suite::sync_gate(&MemSyncGate::new(), t).await;
    }
}
