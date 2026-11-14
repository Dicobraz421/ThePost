//! Права на календари и адресные книги (SRS 4.4, 4.5; CONTRACTS.md#Sharing; CTR-48, CTR-84): [`CollectionAuthorizer`] —
//! реализация [`SharingAuthorizer`], единый слой прав на коллекции ([`Collection`]) для CalDAV, CardDAV, JMAP и Admin
//! API. Хранилища
//! ([`CollectionAcl`], [`DelegateStore`], [`SharingPolicyStore`]) только хранят записи, правила — здесь.
//!
//! - Владелец имеет [`CollectionRights::ALL`] на свои коллекции, записей для этого нет.
//! - Прочие — по записям [`CollectionAcl`]: имя входа, группы субъекта ([`Principal::groups`], уже с вложенными) и
//!   [`ANYONE`]; права записей объединяются, старшее включает младшие ([`CollectionRights::effective`]). Отрицательных
//!   прав нет: идентификатор с `-` в начале не выдаётся ([`SharingAuthorizer::check_grant`] — `Invalid`).
//! - Представитель ([`DelegateStore`]) — [`Delegate::rights`] на каждый календарь владельца; адресные книги
//!   представителю не открываются.
//! - «Только занятость» — право `free_busy` без `read`: субъект видит занятость ([`FreeBusy`]) без подробностей
//!   событий, объекты коллекции ему не читаются.
//! - Политика организации ([`SharingPolicy`]) ограничивает действующие права ([`SharingPolicy::limit`]) — и уже
//!   выданные записи, и права представителей: записи [`ANYONE`] — не больше `max_outside`; при
//!   [`SharingScope::Groups`] субъекту вне групп политики — тоже; [`SharingScope::Nobody`] — общего доступа нет.
//!   Выдача ([`SharingAuthorizer::check_grant`], [`SharingAuthorizer::check_delegate`]) — в пределах политики
//!   ([`SharingPolicy::check`]), отзыв допустим всегда.
//!
//! Участие в группах политики: учётная запись — по раскрытию группы ([`GroupStore::expand`], с вложенными);
//! субъект-группа записи — по имени группы рассылки политики (без учёта регистра). Неизвестная группа политики
//! пропускается.
//!
//! Проверки — набор `tp_testkit::suite::sharing_authorizer` поверх `MemDav` и `MemMetadata`.
//!
//! [`Collection`]: tp_contracts::dav::Collection
//! [`FreeBusy`]: tp_contracts::schedule::FreeBusy

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use tp_contracts::auth::Principal;
use tp_contracts::dav::{CollectionId, CollectionKind, DavStore};
use tp_contracts::groups::GroupStore;
use tp_contracts::meta::{AccountScope, AccountStore};
use tp_contracts::sharing::{
    ANYONE, CollectionAcl, CollectionRights, Delegate, DelegateLevel, DelegateStore, Delegation,
    SharedCollection, SharingAuthorizer, SharingPolicy, SharingPolicyStore, SharingScope,
    collection_rights, validate_identifier,
};
use tp_contracts::{AccountId, Error, GroupId, Result, TenantId};

use crate::authz::identifiers;

/// Хранилища для [`CollectionAuthorizer`].
#[derive(Clone)]
pub struct SharingStores {
    /// Записи прав на коллекции.
    pub acl: Arc<dyn CollectionAcl>,
    /// Представители.
    pub delegates: Arc<dyn DelegateStore>,
    /// Политика организации.
    pub policy: Arc<dyn SharingPolicyStore>,
    /// Коллекции: вид коллекции для прав представителя.
    pub dav: Arc<dyn DavStore>,
    /// Группы рассылки: группы [`SharingScope::Groups`].
    pub groups: Arc<dyn GroupStore>,
    /// Учётные записи: субъект записи по имени входа.
    pub accounts: Arc<dyn AccountStore>,
}

impl fmt::Debug for SharingStores {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharingStores").finish_non_exhaustive()
    }
}

/// Проверка прав на коллекции ([`SharingAuthorizer`]): владелец, записи [`CollectionAcl`], представители, политика
/// организации.
#[derive(Clone)]
pub struct CollectionAuthorizer {
    stores: SharingStores,
}

impl fmt::Debug for CollectionAuthorizer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CollectionAuthorizer")
            .finish_non_exhaustive()
    }
}

/// Политика организации и участие субъекта в её группах — общее для всех коллекций одного запроса.
struct Ctx {
    policy: SharingPolicy,
    in_group: bool,
}

impl CollectionAuthorizer {
    /// Права по хранилищам `stores`.
    pub fn new(stores: SharingStores) -> Self {
        CollectionAuthorizer { stores }
    }
}

#[async_trait]
impl SharingAuthorizer for CollectionAuthorizer {
    async fn rights(
        &self,
        principal: &Principal,
        owner: AccountId,
        collection: CollectionId,
    ) -> Result<CollectionRights> {
        if owner == principal.account {
            return Ok(CollectionRights::ALL);
        }
        let cx = self.ctx(principal).await?;
        self.rights_in(&cx, principal, owner, collection).await
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
        let cx = self.ctx(principal).await?;
        if cx.policy.scope == SharingScope::Nobody {
            return Ok(Vec::new());
        }
        let mut ids = identifiers(principal);
        ids.push(ANYONE.to_owned());
        let found = self
            .stores
            .acl
            .shared_with(scope(principal.tenant, principal.account), &ids)
            .await?;
        let mut out = Vec::with_capacity(found.len());
        for c in found {
            let rights = self
                .rights_in(&cx, principal, c.owner, c.collection)
                .await?;
            if !rights.is_empty() {
                out.push(SharedCollection { rights, ..c });
            }
        }
        Ok(out)
    }

    async fn delegations(&self, principal: &Principal) -> Result<Vec<Delegation>> {
        let cx = self.ctx(principal).await?;
        let found = self
            .stores
            .delegates
            .delegations(scope(principal.tenant, principal.account))
            .await?;
        Ok(found
            .into_iter()
            .filter_map(|d| {
                let rights = cx.policy.limit(
                    Delegate {
                        delegate: principal.account,
                        level: d.level,
                        schedule_on_behalf: d.schedule_on_behalf,
                    }
                    .rights(),
                    false,
                    cx.in_group,
                );
                let level = if rights.write {
                    DelegateLevel::Write
                } else if rights.read {
                    DelegateLevel::Read
                } else {
                    return None;
                };
                Some(Delegation {
                    level,
                    schedule_on_behalf: rights.schedule,
                    ..d
                })
            })
            .collect())
    }

    async fn check_grant(
        &self,
        grantor: &Principal,
        owner: AccountId,
        collection: CollectionId,
        identifier: &str,
        rights: CollectionRights,
    ) -> Result<()> {
        validate_identifier(identifier, false)?;
        if !self.rights(grantor, owner, collection).await?.admin {
            return Err(Error::Forbidden);
        }
        let policy = self.stores.policy.sharing_policy(grantor.tenant).await?;
        let anyone = identifier.eq_ignore_ascii_case(ANYONE);
        let in_group = !anyone
            && self
                .subject_in_policy(&policy, grantor.tenant, identifier)
                .await?;
        policy.check(rights, anyone, in_group)
    }

    async fn check_delegate(
        &self,
        principal: &Principal,
        owner: AccountId,
        delegate: &Delegate,
    ) -> Result<()> {
        if principal.account != owner {
            return Err(Error::Forbidden);
        }
        let policy = self.stores.policy.sharing_policy(principal.tenant).await?;
        let in_group = self
            .member_of(&policy, principal.tenant, delegate.delegate)
            .await?;
        policy.check(delegate.rights(), false, in_group)
    }
}

impl CollectionAuthorizer {
    async fn ctx(&self, principal: &Principal) -> Result<Ctx> {
        let policy = self.stores.policy.sharing_policy(principal.tenant).await?;
        let in_group = self
            .member_of(&policy, principal.tenant, principal.account)
            .await?;
        Ok(Ctx { policy, in_group })
    }

    /// Права не владельца: записи прав и представительство, ограниченные политикой.
    async fn rights_in(
        &self,
        cx: &Ctx,
        principal: &Principal,
        owner: AccountId,
        collection: CollectionId,
    ) -> Result<CollectionRights> {
        if owner == principal.account {
            return Ok(CollectionRights::ALL);
        }
        if cx.policy.scope == SharingScope::Nobody {
            return Ok(CollectionRights::NONE);
        }
        let owner = scope(principal.tenant, owner);
        let entries = match self.stores.acl.collection_acl(owner, collection).await {
            Ok(entries) => entries,
            Err(Error::NotFound) => return Ok(CollectionRights::NONE),
            Err(e) => return Err(e),
        };
        let named = collection_rights(&entries, &identifiers(principal));
        let anyone = collection_rights(&entries, &[ANYONE.to_owned()]);
        let mut rights =
            cx.policy.limit(named, false, cx.in_group) | cx.policy.limit(anyone, true, false);
        if let Some(d) = self.delegate_of(owner, principal.account).await?
            && self.is_calendar(owner, collection).await?
        {
            rights |= cx.policy.limit(d.rights(), false, cx.in_group);
        }
        Ok(rights.effective())
    }

    async fn delegate_of(&self, owner: AccountScope, who: AccountId) -> Result<Option<Delegate>> {
        match self.stores.delegates.delegates(owner).await {
            Ok(list) => Ok(list.into_iter().find(|d| d.delegate == who)),
            Err(Error::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn is_calendar(&self, owner: AccountScope, collection: CollectionId) -> Result<bool> {
        Ok(self
            .stores
            .dav
            .get_collection(owner, collection)
            .await?
            .is_some_and(|c| c.kind == CollectionKind::Calendar))
    }

    /// Учётная запись входит в группу политики [`SharingScope::Groups`].
    async fn member_of(
        &self,
        policy: &SharingPolicy,
        tenant: TenantId,
        account: AccountId,
    ) -> Result<bool> {
        let SharingScope::Groups { groups } = &policy.scope else {
            return Ok(false);
        };
        for g in groups {
            match self.stores.groups.expand(tenant, *g).await {
                Ok(members) if members.contains(&account) => return Ok(true),
                Ok(_) | Err(Error::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(false)
    }

    /// Субъект записи (имя входа или имя группы) входит в группу политики или сам — такая группа.
    async fn subject_in_policy(
        &self,
        policy: &SharingPolicy,
        tenant: TenantId,
        identifier: &str,
    ) -> Result<bool> {
        let SharingScope::Groups { groups } = &policy.scope else {
            return Ok(false);
        };
        if let Some(account) = self.stores.accounts.by_login(tenant, identifier).await? {
            return self.member_of(policy, tenant, account.id).await;
        }
        self.policy_group_named(tenant, groups, identifier).await
    }

    async fn policy_group_named(
        &self,
        tenant: TenantId,
        groups: &[GroupId],
        name: &str,
    ) -> Result<bool> {
        let name = name.to_lowercase();
        for g in groups {
            if let Some(group) = self.stores.groups.get(tenant, *g).await?
                && group.name.to_lowercase() == name
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn scope(tenant: TenantId, account: AccountId) -> AccountScope {
    AccountScope { tenant, account }
}

#[cfg(test)]
mod tests {
    use tp_contracts::DomainId;
    use tp_testkit::suite::{SharingAuthorizerFixture, sharing_authorizer};
    use tp_testkit::{MemDav, MemGroups, MemMetadata, SystemClock};

    use super::*;

    #[tokio::test]
    async fn suite_sharing_authorizer() {
        let meta = MemMetadata::new(Arc::new(SystemClock));
        let (dav, groups) = (MemDav::new(), MemGroups::new());
        let authz = CollectionAuthorizer::new(SharingStores {
            acl: Arc::new(dav.clone()),
            delegates: Arc::new(meta.clone()),
            policy: Arc::new(meta.clone()),
            dav: Arc::new(dav.clone()),
            groups: Arc::new(groups.clone()),
            accounts: Arc::new(meta.clone()),
        });
        let fx = SharingAuthorizerFixture {
            tenant: TenantId::new(),
            domain: DomainId::new(),
            accounts: &meta,
            acl: &dav,
            delegates: &meta,
            policy: &meta,
            dav: &dav,
            groups: &groups,
        };
        sharing_authorizer(&authz, &fx).await;
    }
}
