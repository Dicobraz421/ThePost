//! [`AclStore`] в памяти поверх [`MemMetadata`] (CONTRACTS.md#Auth).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tp_contracts::acl::{
    ANYONE, AclEntry, AclStore, SendDelegation, SharedMailbox, effective_rights,
    validate_identifier,
};
use tp_contracts::auth::{Rights, SendRight};
use tp_contracts::meta::{AccountScope, AccountStore, MailboxStore};
use tp_contracts::{AccountId, Error, MailboxId, Result, TenantId};

use crate::infra::lock;
use crate::meta::MemMetadata;

#[derive(Debug, Default)]
struct AclState {
    acl: BTreeMap<(TenantId, AccountId, MailboxId), BTreeMap<String, Rights>>,
    send: BTreeMap<(TenantId, AccountId), BTreeMap<String, SendRight>>,
}

/// ACL папок и делегирования отправки учётных записей `meta`. Записи удалённой папки не видны
/// (как каскадное удаление в PostgreSQL). Клоны разделяют данные.
#[derive(Debug, Clone)]
pub struct MemAcl {
    meta: MemMetadata,
    state: Arc<Mutex<AclState>>,
}

impl MemAcl {
    /// ACL папок метаданных `meta`.
    pub fn new(meta: &MemMetadata) -> Self {
        Self {
            meta: meta.clone(),
            state: Arc::default(),
        }
    }

    async fn mailbox_exists(&self, owner: AccountScope, mailbox: MailboxId) -> Result<bool> {
        match MailboxStore::get(&self.meta, owner, mailbox).await {
            Ok(m) => Ok(m.is_some()),
            Err(Error::NotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }
}

/// Идентификаторы субъекта с [`ANYONE`] и их отрицательные формы.
fn subject(identifiers: &[String]) -> Result<Vec<String>> {
    let mut ids = Vec::with_capacity(identifiers.len() * 2 + 2);
    for id in identifiers.iter().map(String::as_str).chain([ANYONE]) {
        validate_identifier(id, false)?;
        ids.push(id.to_owned());
        ids.push(format!("-{id}"));
    }
    Ok(ids)
}

fn rights_of(entries: &BTreeMap<String, Rights>, ids: &[String]) -> Rights {
    effective_rights(
        entries
            .iter()
            .filter(|(id, _)| ids.contains(id))
            .map(|(id, r)| (id.as_str(), *r)),
    )
}

#[async_trait]
impl AclStore for MemAcl {
    async fn acl(&self, owner: AccountScope, mailbox: MailboxId) -> Result<Vec<AclEntry>> {
        if !self.mailbox_exists(owner, mailbox).await? {
            return Err(Error::NotFound);
        }
        Ok(lock(&self.state)
            .acl
            .get(&(owner.tenant, owner.account, mailbox))
            .into_iter()
            .flatten()
            .map(|(identifier, rights)| AclEntry {
                identifier: identifier.clone(),
                rights: *rights,
            })
            .collect())
    }

    async fn set_acl(
        &self,
        owner: AccountScope,
        mailbox: MailboxId,
        identifier: &str,
        rights: Rights,
    ) -> Result<()> {
        validate_identifier(identifier, true)?;
        if !self.mailbox_exists(owner, mailbox).await? {
            return Err(Error::NotFound);
        }
        let mut st = lock(&self.state);
        let entries = st
            .acl
            .entry((owner.tenant, owner.account, mailbox))
            .or_default();
        if rights.is_empty() {
            entries.remove(identifier);
        } else {
            entries.insert(identifier.to_owned(), rights);
        }
        Ok(())
    }

    async fn acl_rights(
        &self,
        owner: AccountScope,
        mailbox: MailboxId,
        identifiers: &[String],
    ) -> Result<Rights> {
        let ids = subject(identifiers)?;
        if !self.mailbox_exists(owner, mailbox).await? {
            return Ok(Rights::NONE);
        }
        Ok(lock(&self.state)
            .acl
            .get(&(owner.tenant, owner.account, mailbox))
            .map_or(Rights::NONE, |e| rights_of(e, &ids)))
    }

    async fn shared_mailboxes(
        &self,
        tenant: TenantId,
        identifiers: &[String],
    ) -> Result<Vec<SharedMailbox>> {
        let ids = subject(identifiers)?;
        let candidates: Vec<SharedMailbox> = lock(&self.state)
            .acl
            .iter()
            .filter(|((t, _, _), _)| *t == tenant)
            .map(|(&(_, account, mailbox), e)| SharedMailbox {
                account,
                mailbox,
                rights: rights_of(e, &ids),
            })
            .filter(|s| s.rights.contains(Rights::LOOKUP))
            .collect();
        let mut out = Vec::with_capacity(candidates.len());
        for s in candidates {
            let owner = AccountScope {
                tenant,
                account: s.account,
            };
            if self.mailbox_exists(owner, s.mailbox).await? {
                out.push(s);
            }
        }
        Ok(out)
    }

    async fn send_delegations(&self, owner: AccountScope) -> Result<Vec<SendDelegation>> {
        Ok(lock(&self.state)
            .send
            .get(&(owner.tenant, owner.account))
            .into_iter()
            .flatten()
            .map(|(delegate, right)| SendDelegation {
                delegate: delegate.clone(),
                right: *right,
            })
            .collect())
    }

    async fn set_send_delegation(
        &self,
        owner: AccountScope,
        delegate: &str,
        right: Option<SendRight>,
    ) -> Result<()> {
        validate_identifier(delegate, false)?;
        if AccountStore::get(&self.meta, owner.tenant, owner.account)
            .await?
            .is_none()
        {
            return Err(Error::NotFound);
        }
        let mut st = lock(&self.state);
        let delegations = st.send.entry((owner.tenant, owner.account)).or_default();
        match right {
            Some(r) => {
                delegations.insert(delegate.to_owned(), r);
            }
            None => {
                delegations.remove(delegate);
            }
        }
        Ok(())
    }

    async fn send_right_of(
        &self,
        owner: AccountScope,
        identifiers: &[String],
    ) -> Result<Option<SendRight>> {
        for id in identifiers {
            validate_identifier(id, false)?;
        }
        let st = lock(&self.state);
        let rights: Vec<SendRight> = st
            .send
            .get(&(owner.tenant, owner.account))
            .into_iter()
            .flatten()
            .filter(|(d, _)| identifiers.contains(d))
            .map(|(_, r)| *r)
            .collect();
        Ok(if rights.contains(&SendRight::SendAs) {
            Some(SendRight::SendAs)
        } else {
            rights.first().copied()
        })
    }
}

#[cfg(test)]
mod tests {
    use tp_contracts::meta::AccountUpsert;
    use tp_contracts::{DomainId, TenantId};

    use super::*;
    use crate::suite::{AclFixture, acl_store};

    #[tokio::test]
    async fn passes_contract_suite() {
        let meta = MemMetadata::default();
        let owner = meta.account("owner@example.com").await.expect("owner");
        let peer = meta.account("peer@example.com").await.expect("peer");
        let foreign = meta
            .upsert(
                TenantId::new(),
                AccountUpsert {
                    domain: DomainId::new(),
                    login: "owner@example.com".into(),
                    display_name: "Foreign".into(),
                    external_id: None,
                    enabled: true,
                },
            )
            .await
            .expect("foreign");
        let fx = AclFixture {
            owner,
            peer,
            foreign: AccountScope {
                tenant: foreign.tenant,
                account: foreign.id,
            },
        };
        acl_store(&MemAcl::new(&meta), &meta, &fx).await;
    }

    #[tokio::test]
    async fn clones_share() {
        let meta = MemMetadata::default();
        let owner = meta.account("owner@example.com").await.expect("owner");
        let acl = MemAcl::new(&meta);
        acl.clone()
            .set_send_delegation(owner, "fred", Some(SendRight::SendAs))
            .await
            .expect("set");
        assert_eq!(
            acl.send_right_of(owner, &["fred".into()]).await,
            Ok(Some(SendRight::SendAs))
        );
    }
}
