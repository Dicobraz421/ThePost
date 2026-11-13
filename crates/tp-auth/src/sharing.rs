//! Права на календари и адресные книги (SRS 4.4, 4.5; CONTRACTS.md#Sharing; CTR-48): [`CollectionAuthorizer`] —
//! единый слой прав на коллекции ([`Collection`]) для CalDAV, CardDAV, JMAP и Admin API. Хранилища
//! ([`CollectionAcl`], [`DelegateStore`], [`SharingPolicyStore`]) только хранят записи, правила — здесь.
//!
//! - Владелец имеет [`CollectionRights::ALL`] на свои коллекции, записей для этого нет.
//! - Прочие — по записям [`CollectionAcl`]: имя входа, группы субъекта ([`Principal::groups`], уже с вложенными) и
//!   [`ANYONE`]; права записей объединяются, старшее включает младшие ([`CollectionRights::effective`]). Отрицательных
//!   прав нет: идентификатор с `-` в начале не выдаётся ([`CollectionAuthorizer::check_grant`] — `Invalid`).
//! - Представитель ([`DelegateStore`]) — [`Delegate::rights`] на каждый календарь владельца; адресные книги
//!   представителю не открываются.
//! - «Только занятость» — право `free_busy` без `read`: субъект видит занятость ([`FreeBusy`]) без подробностей
//!   событий, объекты коллекции ему не читаются.
//! - Политика организации ([`SharingPolicy`]) ограничивает действующие права ([`SharingPolicy::limit`]) — и уже
//!   выданные записи, и права представителей: записи [`ANYONE`] — не больше `max_outside`; при
//!   [`SharingScope::Groups`] субъекту вне групп политики — тоже; [`SharingScope::Nobody`] — общего доступа нет.
//!   Выдача ([`CollectionAuthorizer::check_grant`], [`CollectionAuthorizer::check_delegate`]) — в пределах политики
//!   ([`SharingPolicy::check`]), отзыв допустим всегда.
//!
//! Участие в группах политики: учётная запись — по раскрытию группы ([`GroupStore::expand`], с вложенными);
//! субъект-группа записи — по имени группы рассылки политики (без учёта регистра). Неизвестная группа политики
//! пропускается.
//!
//! [`Collection`]: tp_contracts::dav::Collection
//! [`FreeBusy`]: tp_contracts::schedule::FreeBusy

use std::fmt;
use std::sync::Arc;

use tp_contracts::auth::Principal;
use tp_contracts::dav::{CollectionId, CollectionKind, DavStore};
use tp_contracts::groups::GroupStore;
use tp_contracts::meta::{AccountScope, AccountStore};
use tp_contracts::sharing::{
    ANYONE, CollectionAcl, CollectionRights, Delegate, DelegateLevel, DelegateStore, Delegation,
    SharedCollection, SharingPolicy, SharingPolicyStore, SharingScope, collection_rights,
    validate_identifier,
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

/// Проверка прав на коллекции: владелец, записи [`CollectionAcl`], представители, политика организации.
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

    /// Действующие права субъекта на коллекцию `collection` учётной записи `owner` (WebDAV
    /// `current-user-privilege-set`, JMAP `myRights`). Нет коллекции у чужой учётной записи — [`CollectionRights::NONE`].
    pub async fn rights(
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

    /// Есть все права `required` ([`CollectionRights::contains`]), иначе `Error::Forbidden`.
    pub async fn check(
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

    /// Чужие коллекции организации, доступные субъекту по записям прав, с действующими правами (дом коллекций
    /// получателя в CalDAV и CardDAV, список JMAP); по (`owner`, `collection`). Коллекции, права на которые
    /// политика сняла целиком, не входят. Календари владельцев, представителем которых является субъект, —
    /// [`CollectionAuthorizer::delegations`].
    pub async fn shared_with(&self, principal: &Principal) -> Result<Vec<SharedCollection>> {
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

    /// Учётные записи, представителем которых является субъект (CalDAV `calendar-proxy-read-for`,
    /// `calendar-proxy-write-for`), с уровнем в пределах политики: запись, урезанная политикой до чтения, —
    /// [`DelegateLevel::Read`]; снятая целиком — не входит. По `owner`.
    pub async fn delegations(&self, principal: &Principal) -> Result<Vec<Delegation>> {
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

    /// Субъект может выдать права `rights` на коллекцию `collection` учётной записи `owner` субъекту `identifier`
    /// ([`CollectionAcl::set_collection_acl`]): выдаёт владелец или субъект с правом `admin`, в пределах политики
    /// организации; пустые права (отзыв) политика не ограничивает. Недопустимый идентификатор (в том числе
    /// отрицательный) — `Error::Invalid`; нет права или политика не допускает — `Error::Forbidden`.
    pub async fn check_grant(
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

    /// Субъект может назначить представителя `delegate` учётной записи `owner` ([`DelegateStore::set_delegate`]):
    /// назначает только сам владелец, права представителя — в пределах политики организации. Иначе —
    /// `Error::Forbidden`. Отзыв ([`DelegateStore::remove_delegate`]) политика не ограничивает — достаточно быть
    /// владельцем.
    pub async fn check_delegate(
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
    use std::collections::HashMap;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use tp_contracts::dav::Collection;
    use tp_contracts::groups::{DistributionGroup, GroupMember, SenderRestriction};
    use tp_contracts::sharing::CollectionAclEntry;
    use tp_testkit::{MemDav, MemGroups};

    use super::*;
    use crate::testutil::meta_accounts;

    // ---------- Хранилища в памяти (реализаций CTR-48 в tp-testkit пока нет) ----------

    type Key = (AccountScope, CollectionId);

    #[derive(Default)]
    struct Acl(Mutex<HashMap<Key, Vec<CollectionAclEntry>>>);

    #[async_trait]
    impl CollectionAcl for Acl {
        async fn collection_acl(
            &self,
            owner: AccountScope,
            collection: CollectionId,
        ) -> Result<Vec<CollectionAclEntry>> {
            Ok(self
                .0
                .lock()
                .expect("acl")
                .get(&(owner, collection))
                .cloned()
                .unwrap_or_default())
        }

        async fn set_collection_acl(
            &self,
            owner: AccountScope,
            collection: CollectionId,
            identifier: &str,
            rights: CollectionRights,
        ) -> Result<()> {
            validate_identifier(identifier, false)?;
            let mut map = self.0.lock().expect("acl");
            let entries = map.entry((owner, collection)).or_default();
            entries.retain(|e| e.identifier != identifier);
            if !rights.is_empty() {
                entries.push(CollectionAclEntry {
                    identifier: identifier.to_owned(),
                    rights,
                });
            }
            Ok(())
        }

        async fn collection_rights(
            &self,
            owner: AccountScope,
            collection: CollectionId,
            identifiers: &[String],
        ) -> Result<CollectionRights> {
            let entries = self.collection_acl(owner, collection).await?;
            Ok(collection_rights(&entries, identifiers))
        }

        async fn shared_with(
            &self,
            scope: AccountScope,
            identifiers: &[String],
        ) -> Result<Vec<SharedCollection>> {
            let mut out: Vec<_> = self
                .0
                .lock()
                .expect("acl")
                .iter()
                .filter(|((o, _), _)| o.tenant == scope.tenant && o.account != scope.account)
                .map(|((o, c), entries)| SharedCollection {
                    owner: o.account,
                    collection: *c,
                    rights: collection_rights(entries, identifiers),
                })
                .filter(|s| !s.rights.is_empty())
                .collect();
            out.sort_by_key(|s| (s.owner, s.collection));
            Ok(out)
        }
    }

    #[derive(Default)]
    struct Delegates(Mutex<Vec<(AccountScope, Delegate)>>);

    #[async_trait]
    impl DelegateStore for Delegates {
        async fn delegates(&self, owner: AccountScope) -> Result<Vec<Delegate>> {
            Ok(self
                .0
                .lock()
                .expect("delegates")
                .iter()
                .filter(|(o, _)| *o == owner)
                .map(|(_, d)| *d)
                .collect())
        }

        async fn set_delegate(&self, owner: AccountScope, delegate: Delegate) -> Result<()> {
            let mut list = self.0.lock().expect("delegates");
            list.retain(|(o, d)| !(*o == owner && d.delegate == delegate.delegate));
            list.push((owner, delegate));
            Ok(())
        }

        async fn remove_delegate(&self, owner: AccountScope, delegate: AccountId) -> Result<()> {
            self.0
                .lock()
                .expect("delegates")
                .retain(|(o, d)| !(*o == owner && d.delegate == delegate));
            Ok(())
        }

        async fn delegations(&self, delegate: AccountScope) -> Result<Vec<Delegation>> {
            Ok(self
                .0
                .lock()
                .expect("delegates")
                .iter()
                .filter(|(o, d)| o.tenant == delegate.tenant && d.delegate == delegate.account)
                .map(|(o, d)| Delegation {
                    owner: o.account,
                    level: d.level,
                    schedule_on_behalf: d.schedule_on_behalf,
                })
                .collect())
        }
    }

    #[derive(Default)]
    struct Policies(Mutex<HashMap<TenantId, SharingPolicy>>);

    #[async_trait]
    impl SharingPolicyStore for Policies {
        async fn sharing_policy(&self, tenant: TenantId) -> Result<SharingPolicy> {
            Ok(self
                .0
                .lock()
                .expect("policy")
                .get(&tenant)
                .cloned()
                .unwrap_or_default())
        }

        async fn set_sharing_policy(&self, tenant: TenantId, policy: SharingPolicy) -> Result<()> {
            self.0.lock().expect("policy").insert(tenant, policy);
            Ok(())
        }
    }

    // ---------- Окружение ----------

    const LOGINS: [&str; 4] = [
        "alice@example.org",
        "bob@example.org",
        "carol@example.org",
        "dave@example.org",
    ];

    /// alice — владелец календаря `work` и адресной книги `team`; bob, carol, dave — субъекты.
    struct Fx {
        authz: CollectionAuthorizer,
        acl: Arc<Acl>,
        delegates: Arc<Delegates>,
        policy: Arc<Policies>,
        groups: MemGroups,
        ids: Vec<AccountId>,
        calendar: CollectionId,
        book: CollectionId,
    }

    async fn fx() -> Fx {
        let list: Vec<_> = LOGINS.iter().map(|l| (*l, true)).collect();
        let (meta, _, ids) = meta_accounts(&list).await;
        let dav = MemDav::new();
        let alice = scope(TenantId::DEFAULT, ids[0]);
        let calendar = Collection::new(CollectionKind::Calendar, "work");
        let book = Collection::new(CollectionKind::AddressBook, "team");
        let (calendar_id, book_id) = (calendar.id, book.id);
        dav.create_collection(alice, calendar)
            .await
            .expect("calendar");
        dav.create_collection(alice, book).await.expect("book");
        let (acl, delegates, policy) = (
            Arc::new(Acl::default()),
            Arc::new(Delegates::default()),
            Arc::new(Policies::default()),
        );
        let groups = MemGroups::new();
        let authz = CollectionAuthorizer::new(SharingStores {
            acl: acl.clone(),
            delegates: delegates.clone(),
            policy: policy.clone(),
            dav: Arc::new(dav),
            groups: Arc::new(groups.clone()),
            accounts: Arc::new(meta),
        });
        Fx {
            authz,
            acl,
            delegates,
            policy,
            groups,
            ids,
            calendar: calendar_id,
            book: book_id,
        }
    }

    impl Fx {
        fn p(&self, i: usize, groups: &[&str]) -> Principal {
            Principal {
                tenant: TenantId::DEFAULT,
                account: self.ids[i],
                login: LOGINS[i].into(),
                groups: groups.iter().map(|g| (*g).to_owned()).collect(),
                scopes: None,
            }
        }

        fn alice(&self) -> AccountScope {
            scope(TenantId::DEFAULT, self.ids[0])
        }

        async fn grant(&self, collection: CollectionId, id: &str, rights: CollectionRights) {
            self.acl
                .set_collection_acl(self.alice(), collection, id, rights)
                .await
                .expect("set_collection_acl");
        }

        async fn rights(&self, p: &Principal, collection: CollectionId) -> CollectionRights {
            self.authz
                .rights(p, self.ids[0], collection)
                .await
                .expect("rights")
        }

        async fn delegate(&self, i: usize, level: DelegateLevel, schedule_on_behalf: bool) {
            self.delegates
                .set_delegate(
                    self.alice(),
                    Delegate {
                        delegate: self.ids[i],
                        level,
                        schedule_on_behalf,
                    },
                )
                .await
                .expect("set_delegate");
        }

        /// Группа рассылки `name` с участниками — учётными записями с индексами `members`.
        async fn group(&self, name: &str, members: &[usize]) -> GroupId {
            let id = GroupId::new();
            self.groups
                .put(DistributionGroup {
                    id,
                    tenant: TenantId::DEFAULT,
                    name: name.into(),
                    address: None,
                    external_id: None,
                    members: members
                        .iter()
                        .map(|i| GroupMember::Account(self.ids[*i]))
                        .collect(),
                    senders: SenderRestriction::Anyone,
                    moderators: Vec::new(),
                    hidden_from_gal: false,
                })
                .await
                .expect("group");
            id
        }

        async fn set_policy(&self, policy: SharingPolicy) {
            self.policy
                .set_sharing_policy(TenantId::DEFAULT, policy)
                .await
                .expect("policy");
        }
    }

    use CollectionRights as R;

    /// Владелец, прямые записи, группы, anyone, «только занятость», чужая и несуществующая коллекция.
    #[tokio::test]
    async fn owner_entries_groups() {
        let fx = fx().await;
        fx.grant(fx.calendar, "bob@example.org", R::READ).await;
        fx.grant(fx.calendar, "team", R::WRITE).await;
        fx.grant(fx.calendar, ANYONE, R::FREE_BUSY).await;
        fx.grant(
            fx.book,
            "carol@example.org",
            R {
                admin: true,
                ..R::NONE
            },
        )
        .await;

        // Владелец — всё без записей, в том числе на коллекцию, которой нет.
        let alice = fx.p(0, &[]);
        assert_eq!(fx.rights(&alice, fx.calendar).await, R::ALL);
        assert_eq!(fx.rights(&alice, CollectionId::new()).await, R::ALL);

        // Прямая запись: чтение (с занятостью), без записи.
        let bob = fx.p(1, &[]);
        assert_eq!(fx.rights(&bob, fx.calendar).await, R::READ);
        assert_eq!(
            fx.authz.check(&bob, fx.ids[0], fx.calendar, R::WRITE).await,
            Err(Error::Forbidden)
        );
        assert_eq!(
            fx.authz.check(&bob, fx.ids[0], fx.calendar, R::READ).await,
            Ok(())
        );
        // Через группу — запись; записи объединяются.
        assert_eq!(fx.rights(&fx.p(1, &["team"]), fx.calendar).await, R::WRITE);
        // Только anyone — занятость без подробностей.
        let dave = fx.p(3, &[]);
        assert_eq!(fx.rights(&dave, fx.calendar).await, R::FREE_BUSY);
        assert_eq!(
            fx.authz.check(&dave, fx.ids[0], fx.calendar, R::READ).await,
            Err(Error::Forbidden)
        );
        // admin включает всё.
        assert_eq!(fx.rights(&fx.p(2, &[]), fx.book).await, R::ALL);
        // Без записей — ничего; неизвестная коллекция — ничего.
        assert_eq!(fx.rights(&dave, fx.book).await, R::NONE);
        assert_eq!(fx.rights(&bob, CollectionId::new()).await, R::NONE);
        // Группа «anyone» и недопустимые в записях имена не добавляют прав.
        assert_eq!(
            fx.rights(&fx.p(3, &["anyone", "-team", "Domain Users"]), fx.book)
                .await,
            R::NONE
        );

        // Отзыв.
        fx.grant(fx.calendar, "bob@example.org", R::NONE).await;
        assert_eq!(fx.rights(&bob, fx.calendar).await, R::FREE_BUSY);
    }

    /// Отрицательных прав нет: идентификатор с `-` не выдаётся; выдавать может владелец или субъект с `admin`.
    #[tokio::test]
    async fn grants() {
        let fx = fx().await;
        let alice = fx.p(0, &[]);
        let (bob, carol) = (fx.p(1, &[]), fx.p(2, &[]));
        let grant = |p: &Principal, id: &str, rights| {
            let (p, id) = (p.clone(), id.to_owned());
            let (authz, owner, calendar) = (fx.authz.clone(), fx.ids[0], fx.calendar);
            async move { authz.check_grant(&p, owner, calendar, &id, rights).await }
        };
        assert!(matches!(
            grant(&alice, "-bob@example.org", R::READ).await,
            Err(Error::Invalid(_))
        ));
        assert!(matches!(
            grant(&alice, "Domain Users", R::READ).await,
            Err(Error::Invalid(_))
        ));
        assert_eq!(grant(&alice, "bob@example.org", R::WRITE).await, Ok(()));
        // Без admin выдавать нельзя, даже с записью.
        fx.grant(fx.calendar, "bob@example.org", R::WRITE).await;
        assert_eq!(
            grant(&bob, "carol@example.org", R::READ).await,
            Err(Error::Forbidden)
        );
        // С admin — можно.
        fx.grant(
            fx.calendar,
            "carol@example.org",
            R {
                admin: true,
                ..R::NONE
            },
        )
        .await;
        assert_eq!(grant(&carol, "dave@example.org", R::WRITE).await, Ok(()));
        // Политика по умолчанию: «всем» — не больше чтения; отзыв — всегда.
        assert_eq!(grant(&alice, ANYONE, R::READ).await, Ok(()));
        assert_eq!(grant(&alice, ANYONE, R::WRITE).await, Err(Error::Forbidden));
        assert_eq!(
            grant(&alice, "Anyone", R::WRITE).await,
            Err(Error::Forbidden)
        );
        assert_eq!(grant(&alice, ANYONE, R::NONE).await, Ok(()));
    }

    /// Представитель: права на все календари владельца, не на адресные книги; планирование — по признаку.
    #[tokio::test]
    async fn delegates() {
        let fx = fx().await;
        fx.delegate(1, DelegateLevel::Write, true).await;
        fx.delegate(2, DelegateLevel::Read, false).await;
        fx.grant(fx.book, "carol@example.org", R::READ).await;

        let (bob, carol) = (fx.p(1, &[]), fx.p(2, &[]));
        assert_eq!(
            fx.rights(&bob, fx.calendar).await,
            R {
                schedule: true,
                ..R::WRITE
            }
        );
        assert_eq!(fx.rights(&carol, fx.calendar).await, R::READ);
        // Адресная книга — только по записи прав.
        assert_eq!(fx.rights(&bob, fx.book).await, R::NONE);
        assert_eq!(fx.rights(&carol, fx.book).await, R::READ);
        // Представительство не путает владельцев: календарь bob недоступен alice через её представителя.
        assert_eq!(
            fx.authz
                .rights(&fx.p(0, &[]), fx.ids[1], CollectionId::new())
                .await,
            Ok(R::NONE)
        );

        let owner = fx.ids[0];
        assert_eq!(
            fx.authz.delegations(&bob).await,
            Ok(vec![Delegation {
                owner,
                level: DelegateLevel::Write,
                schedule_on_behalf: true,
            }])
        );
        assert_eq!(fx.authz.delegations(&fx.p(3, &[])).await, Ok(Vec::new()));

        // Назначает только владелец.
        let d = Delegate {
            delegate: fx.ids[3],
            level: DelegateLevel::Write,
            schedule_on_behalf: true,
        };
        assert_eq!(
            fx.authz.check_delegate(&fx.p(0, &[]), owner, &d).await,
            Ok(())
        );
        assert_eq!(
            fx.authz.check_delegate(&bob, owner, &d).await,
            Err(Error::Forbidden)
        );

        // Отзыв.
        fx.delegates
            .remove_delegate(fx.alice(), fx.ids[1])
            .await
            .expect("remove");
        assert_eq!(fx.rights(&bob, fx.calendar).await, R::NONE);
    }

    /// Политика организации: ограничивает выдачу и уже выданные записи, права представителей и список общих.
    #[tokio::test]
    async fn policy_limits() {
        let fx = fx().await;
        fx.grant(fx.calendar, "bob@example.org", R::WRITE).await;
        fx.grant(fx.calendar, "carol@example.org", R::WRITE).await;
        fx.grant(fx.calendar, "staff", R::WRITE).await;
        // Записано в обход политики (до её ужесточения): «всем» — запись.
        fx.grant(fx.book, ANYONE, R::WRITE).await;
        fx.delegate(3, DelegateLevel::Write, true).await;

        let (bob, carol, dave) = (fx.p(1, &[]), fx.p(2, &[]), fx.p(3, &[]));
        // По умолчанию — организация: именованным субъектам всё, «всем» — не больше чтения.
        assert_eq!(fx.rights(&bob, fx.calendar).await, R::WRITE);
        assert_eq!(fx.rights(&dave, fx.book).await, R::READ);

        // Полные права — только группе «Руководство» (carol); остальным — занятость.
        let board = fx.group("Руководство", &[2]).await;
        fx.set_policy(SharingPolicy {
            scope: SharingScope::Groups {
                groups: vec![board, GroupId::new()],
            },
            max_outside: R::FREE_BUSY,
        })
        .await;
        assert_eq!(fx.rights(&bob, fx.calendar).await, R::FREE_BUSY);
        assert_eq!(fx.rights(&carol, fx.calendar).await, R::WRITE);
        assert_eq!(
            fx.rights(&fx.p(1, &["staff"]), fx.calendar).await,
            R::FREE_BUSY
        );
        assert_eq!(fx.rights(&carol, fx.book).await, R::FREE_BUSY);
        // Представитель вне группы — только занятость: не представитель по сути.
        assert_eq!(fx.rights(&dave, fx.calendar).await, R::FREE_BUSY);
        assert_eq!(fx.authz.delegations(&dave).await, Ok(Vec::new()));

        // Выдача: субъекту группы — всё, группе политики по имени — всё, вне группы — не больше max_outside.
        let alice = fx.p(0, &[]);
        let grant = |id: &str, rights| {
            let id = id.to_owned();
            let (authz, owner, calendar) = (fx.authz.clone(), fx.ids[0], fx.calendar);
            let alice = alice.clone();
            async move {
                authz
                    .check_grant(&alice, owner, calendar, &id, rights)
                    .await
            }
        };
        assert_eq!(grant("carol@example.org", R::WRITE).await, Ok(()));
        assert_eq!(grant("руководство", R::WRITE).await, Ok(()));
        assert_eq!(grant("bob@example.org", R::FREE_BUSY).await, Ok(()));
        assert_eq!(
            grant("bob@example.org", R::READ).await,
            Err(Error::Forbidden)
        );
        assert_eq!(grant("staff", R::READ).await, Err(Error::Forbidden));
        let d = |i: usize| Delegate {
            delegate: fx.ids[i],
            level: DelegateLevel::Read,
            schedule_on_behalf: false,
        };
        assert_eq!(
            fx.authz.check_delegate(&alice, fx.ids[0], &d(2)).await,
            Ok(())
        );
        assert_eq!(
            fx.authz.check_delegate(&alice, fx.ids[0], &d(1)).await,
            Err(Error::Forbidden)
        );

        // Общий доступ запрещён: записи и представители не действуют, владелец — всё.
        fx.set_policy(SharingPolicy {
            scope: SharingScope::Nobody,
            ..SharingPolicy::default()
        })
        .await;
        assert_eq!(fx.rights(&carol, fx.calendar).await, R::NONE);
        assert_eq!(fx.rights(&dave, fx.calendar).await, R::NONE);
        assert_eq!(fx.rights(&alice, fx.calendar).await, R::ALL);
        assert_eq!(fx.authz.shared_with(&carol).await, Ok(Vec::new()));
        assert_eq!(
            grant("carol@example.org", R::FREE_BUSY).await,
            Err(Error::Forbidden)
        );
        assert_eq!(grant("carol@example.org", R::NONE).await, Ok(()));
    }

    /// Дом коллекций получателя: чужие коллекции с действующими правами, по (владелец, коллекция).
    #[tokio::test]
    async fn shared_with_home() {
        let fx = fx().await;
        fx.grant(fx.calendar, "bob@example.org", R::READ).await;
        fx.grant(fx.book, ANYONE, R::WRITE).await;
        let owner = fx.ids[0];
        let mut want = vec![
            SharedCollection {
                owner,
                collection: fx.calendar,
                rights: R::READ,
            },
            // «Всем» — запись, урезанная политикой до чтения.
            SharedCollection {
                owner,
                collection: fx.book,
                rights: R::READ,
            },
        ];
        want.sort_by_key(|s| (s.owner, s.collection));
        assert_eq!(fx.authz.shared_with(&fx.p(1, &[])).await, Ok(want));
        // Своё не входит.
        assert_eq!(fx.authz.shared_with(&fx.p(0, &[])).await, Ok(Vec::new()));
        // Только «всем».
        assert_eq!(
            fx.authz.shared_with(&fx.p(2, &[])).await,
            Ok(vec![SharedCollection {
                owner,
                collection: fx.book,
                rights: R::READ,
            }])
        );
    }
}
