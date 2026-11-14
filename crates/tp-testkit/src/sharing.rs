//! Фейк [`SharingAuthorizer`] для тестов протоколов (CTR-84; `tp-dav`, `tp-jmap`, `tp-admin`): права на коллекции и
//! представительства задаёт тест таблицей, правил политики и записей прав в фейке нет — их проверяет набор
//! [`suite::sharing_authorizer`](crate::suite::sharing_authorizer()) на реализации `tp-auth`.
//!
//! ```
//! # futures::executor::block_on(async {
//! use tp_contracts::dav::CollectionId;
//! use tp_contracts::sharing::{CollectionRights, SharingAuthorizer};
//! use tp_contracts::{AccountId, TenantId};
//! use tp_testkit::{FakeSharingAuthorizer, principal};
//!
//! let authz = FakeSharingAuthorizer::default();
//! let bob = principal(TenantId::DEFAULT, "bob@example.org");
//! let (owner, calendar) = (AccountId::new(), CollectionId::new());
//! authz.set_rights(bob.account, owner, calendar, CollectionRights::FREE_BUSY);
//! assert_eq!(authz.rights(&bob, owner, calendar).await, Ok(CollectionRights::FREE_BUSY));
//! # });
//! ```

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tp_contracts::auth::Principal;
use tp_contracts::dav::CollectionId;
use tp_contracts::sharing::{
    CollectionRights, Delegate, Delegation, SharedCollection, SharingAuthorizer,
    validate_identifier,
};
use tp_contracts::{AccountId, Error, Result};

use crate::infra::lock;

#[derive(Debug, Default)]
struct State {
    /// Действующие права по (субъект, владелец, коллекция).
    rights: BTreeMap<(AccountId, AccountId, CollectionId), CollectionRights>,
    /// Представительства субъекта.
    delegations: BTreeMap<AccountId, Vec<Delegation>>,
}

/// Права на коллекции по таблице. Владелец — [`CollectionRights::ALL`] на свои коллекции; прочие — права, заданные
/// [`FakeSharingAuthorizer::set_rights`] (как действующие), иначе [`CollectionRights::NONE`]. Выдача прав — владелец
/// или субъект с `admin`, без ограничений политики; назначение представителя — только владелец. Клоны разделяют
/// состояние.
#[derive(Debug, Clone, Default)]
pub struct FakeSharingAuthorizer {
    state: Arc<Mutex<State>>,
}

impl FakeSharingAuthorizer {
    /// Действующие права `rights` субъекта `subject` на коллекцию `collection` учётной записи `owner`; пустые —
    /// снимают запись.
    pub fn set_rights(
        &self,
        subject: AccountId,
        owner: AccountId,
        collection: CollectionId,
        rights: CollectionRights,
    ) {
        let mut st = lock(&self.state);
        let key = (subject, owner, collection);
        if rights.is_empty() {
            st.rights.remove(&key);
        } else {
            st.rights.insert(key, rights.effective());
        }
    }

    /// Представительства субъекта `delegate` ([`SharingAuthorizer::delegations`]), заменяет прежние.
    pub fn set_delegations(&self, delegate: AccountId, mut list: Vec<Delegation>) {
        list.sort_by_key(|d| d.owner);
        lock(&self.state).delegations.insert(delegate, list);
    }
}

#[async_trait]
impl SharingAuthorizer for FakeSharingAuthorizer {
    async fn rights(
        &self,
        principal: &Principal,
        owner: AccountId,
        collection: CollectionId,
    ) -> Result<CollectionRights> {
        if owner == principal.account {
            return Ok(CollectionRights::ALL);
        }
        Ok(lock(&self.state)
            .rights
            .get(&(principal.account, owner, collection))
            .copied()
            .unwrap_or(CollectionRights::NONE))
    }

    async fn check(
        &self,
        principal: &Principal,
        owner: AccountId,
        collection: CollectionId,
        required: CollectionRights,
    ) -> Result<()> {
        if self
            .rights(principal, owner, collection)
            .await?
            .contains(required)
        {
            Ok(())
        } else {
            Err(Error::Forbidden)
        }
    }

    async fn shared_with(&self, principal: &Principal) -> Result<Vec<SharedCollection>> {
        Ok(lock(&self.state)
            .rights
            .iter()
            .filter(|((subject, owner, _), _)| {
                *subject == principal.account && *owner != principal.account
            })
            .map(|(&(_, owner, collection), &rights)| SharedCollection {
                owner,
                collection,
                rights,
            })
            .collect())
    }

    async fn delegations(&self, principal: &Principal) -> Result<Vec<Delegation>> {
        Ok(lock(&self.state)
            .delegations
            .get(&principal.account)
            .cloned()
            .unwrap_or_default())
    }

    async fn check_grant(
        &self,
        grantor: &Principal,
        owner: AccountId,
        collection: CollectionId,
        identifier: &str,
        _rights: CollectionRights,
    ) -> Result<()> {
        validate_identifier(identifier, false)?;
        if self.rights(grantor, owner, collection).await?.admin {
            Ok(())
        } else {
            Err(Error::Forbidden)
        }
    }

    async fn check_delegate(
        &self,
        principal: &Principal,
        owner: AccountId,
        _delegate: &Delegate,
    ) -> Result<()> {
        if principal.account == owner {
            Ok(())
        } else {
            Err(Error::Forbidden)
        }
    }
}

#[cfg(test)]
mod tests {
    use tp_contracts::TenantId;
    use tp_contracts::sharing::{ANYONE, DelegateLevel};

    use super::*;
    use crate::principal;

    #[tokio::test]
    async fn table() {
        let authz = FakeSharingAuthorizer::default();
        let alice = principal(TenantId::DEFAULT, "alice@example.org");
        let bob = principal(TenantId::DEFAULT, "bob@example.org");
        let (a, b) = (alice.account, bob.account);
        let (c1, c2) = (CollectionId::new(), CollectionId::new());
        let admin = CollectionRights {
            admin: true,
            ..CollectionRights::NONE
        };

        assert_eq!(authz.rights(&alice, a, c1).await, Ok(CollectionRights::ALL));
        assert_eq!(authz.rights(&bob, a, c1).await, Ok(CollectionRights::NONE));
        authz.set_rights(b, a, c2, admin);
        authz.set_rights(b, a, c1, CollectionRights::READ);
        assert_eq!(authz.rights(&bob, a, c2).await, Ok(CollectionRights::ALL));
        assert_eq!(
            authz.check(&bob, a, c1, CollectionRights::WRITE).await,
            Err(Error::Forbidden)
        );
        assert_eq!(
            authz.check(&bob, a, c1, CollectionRights::FREE_BUSY).await,
            Ok(())
        );
        let mut want = vec![
            SharedCollection {
                owner: a,
                collection: c1,
                rights: CollectionRights::READ,
            },
            SharedCollection {
                owner: a,
                collection: c2,
                rights: CollectionRights::ALL,
            },
        ];
        want.sort_by_key(|s| (s.owner, s.collection));
        assert_eq!(authz.shared_with(&bob).await, Ok(want));
        assert_eq!(authz.shared_with(&alice).await, Ok(Vec::new()));

        // Выдача: владелец и admin; недопустимый идентификатор — Invalid.
        let r = CollectionRights::WRITE;
        assert_eq!(authz.check_grant(&bob, a, c2, ANYONE, r).await, Ok(()));
        assert_eq!(
            authz.check_grant(&bob, a, c1, ANYONE, r).await,
            Err(Error::Forbidden)
        );
        assert!(matches!(
            authz.check_grant(&alice, a, c1, "-bob", r).await,
            Err(Error::Invalid(_))
        ));

        // Представители.
        let d = Delegation {
            owner: a,
            level: DelegateLevel::Write,
            schedule_on_behalf: true,
        };
        authz.set_delegations(b, vec![d]);
        assert_eq!(authz.delegations(&bob).await, Ok(vec![d]));
        let delegate = Delegate {
            delegate: b,
            level: DelegateLevel::Read,
            schedule_on_behalf: false,
        };
        assert_eq!(authz.check_delegate(&alice, a, &delegate).await, Ok(()));
        assert_eq!(
            authz.check_delegate(&bob, a, &delegate).await,
            Err(Error::Forbidden)
        );

        // Отзыв.
        authz.set_rights(b, a, c1, CollectionRights::NONE);
        assert_eq!(authz.rights(&bob, a, c1).await, Ok(CollectionRights::NONE));
    }
}
