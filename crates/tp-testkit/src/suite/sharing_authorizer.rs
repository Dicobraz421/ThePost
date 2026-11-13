//! Набор проверок [`SharingAuthorizer`] (CTR-84, CONTRACTS.md#Sharing): владелец, записи прав (имя входа, группы,
//! [`ANYONE`]), «только занятость», выдача прав, представители, политика организации, дом коллекций получателя.
//! Реализация `tp-auth` (`CollectionAuthorizer`) проходит его поверх `MemDav` и `MemMetadata`; поверх `PgMeta` —
//! теми же проверками.

use std::fmt;

use tp_contracts::auth::Principal;
use tp_contracts::dav::{Collection, CollectionId, CollectionKind, DavStore};
use tp_contracts::groups::{DistributionGroup, GroupMember, GroupStore, SenderRestriction};
use tp_contracts::meta::{AccountKind, AccountScope, AccountStore, AccountUpsert};
use tp_contracts::sharing::{
    ANYONE, CollectionAcl, CollectionRights as R, Delegate, DelegateLevel, DelegateStore,
    Delegation, SharedCollection, SharingAuthorizer, SharingPolicy, SharingPolicyStore,
    SharingScope,
};
use tp_contracts::{AccountId, DomainId, Error, GroupId, TenantId};

/// Хранилища, поверх которых построена проверяемая реализация, — набор заполняет их сам. Организация `tenant` —
/// новая (набор создаёт в ней учётные записи `alice@example.org`, `bob@`, `carol@`, `dave@` и меняет её политику),
/// домен `domain` — её.
pub struct SharingAuthorizerFixture<'a> {
    /// Организация набора.
    pub tenant: TenantId,
    /// Домен учётных записей набора.
    pub domain: DomainId,
    /// Учётные записи.
    pub accounts: &'a dyn AccountStore,
    /// Записи прав на коллекции.
    pub acl: &'a dyn CollectionAcl,
    /// Представители.
    pub delegates: &'a dyn DelegateStore,
    /// Политика организации.
    pub policy: &'a dyn SharingPolicyStore,
    /// Коллекции.
    pub dav: &'a dyn DavStore,
    /// Группы рассылки.
    pub groups: &'a dyn GroupStore,
}

impl fmt::Debug for SharingAuthorizerFixture<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SharingAuthorizerFixture")
            .field("tenant", &self.tenant)
            .field("domain", &self.domain)
            .finish_non_exhaustive()
    }
}

const LOGINS: [&str; 4] = [
    "alice@example.org",
    "bob@example.org",
    "carol@example.org",
    "dave@example.org",
];

/// alice (0) — владелец календаря и адресной книги раздела; bob (1), carol (2), dave (3) — субъекты.
struct Env<'a> {
    authz: &'a dyn SharingAuthorizer,
    fx: &'a SharingAuthorizerFixture<'a>,
    ids: Vec<AccountId>,
}

/// Коллекции alice одного раздела набора.
#[derive(Clone, Copy)]
struct Colls {
    calendar: CollectionId,
    book: CollectionId,
}

impl Env<'_> {
    fn p(&self, i: usize, groups: &[&str]) -> Principal {
        Principal {
            tenant: self.fx.tenant,
            account: self.ids[i],
            login: LOGINS[i].into(),
            groups: groups.iter().map(|g| (*g).to_owned()).collect(),
            scopes: None,
        }
    }

    fn alice(&self) -> AccountScope {
        AccountScope {
            tenant: self.fx.tenant,
            account: self.ids[0],
        }
    }

    /// Новый раздел `name`: политика по умолчанию, без представителей alice, новые календарь и адресная книга alice.
    async fn section(&self, name: &str) -> Colls {
        self.set_policy(SharingPolicy::default()).await;
        for id in &self.ids[1..] {
            self.fx
                .delegates
                .remove_delegate(self.alice(), *id)
                .await
                .expect("remove_delegate");
        }
        let calendar = Collection::new(CollectionKind::Calendar, format!("{name}-work"));
        let book = Collection::new(CollectionKind::AddressBook, format!("{name}-team"));
        let colls = Colls {
            calendar: calendar.id,
            book: book.id,
        };
        self.fx
            .dav
            .create_collection(self.alice(), calendar)
            .await
            .expect("календарь");
        self.fx
            .dav
            .create_collection(self.alice(), book)
            .await
            .expect("адресная книга");
        colls
    }

    async fn grant(&self, collection: CollectionId, id: &str, rights: R) {
        self.fx
            .acl
            .set_collection_acl(self.alice(), collection, id, rights)
            .await
            .expect("set_collection_acl");
    }

    async fn rights(&self, p: &Principal, collection: CollectionId) -> R {
        self.authz
            .rights(p, self.ids[0], collection)
            .await
            .expect("rights")
    }

    async fn check_grant(
        &self,
        grantor: &Principal,
        collection: CollectionId,
        id: &str,
        rights: R,
    ) -> tp_contracts::Result<()> {
        self.authz
            .check_grant(grantor, self.ids[0], collection, id, rights)
            .await
    }

    /// Чужие коллекции alice раздела `colls`, доступные `p`.
    async fn shared(&self, p: &Principal, colls: Colls) -> Vec<SharedCollection> {
        self.authz
            .shared_with(p)
            .await
            .expect("shared_with")
            .into_iter()
            .filter(|s| {
                s.owner == self.ids[0] && [colls.calendar, colls.book].contains(&s.collection)
            })
            .collect()
    }

    async fn delegate(&self, i: usize, level: DelegateLevel, schedule_on_behalf: bool) {
        self.fx
            .delegates
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
        self.fx
            .groups
            .put(DistributionGroup {
                id,
                tenant: self.fx.tenant,
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
        self.fx
            .policy
            .set_sharing_policy(self.fx.tenant, policy)
            .await
            .expect("set_sharing_policy");
    }
}

/// Набор проверок [`SharingAuthorizer`] на хранилищах `fx`, поверх которых построена `authz`.
pub async fn sharing_authorizer(authz: &dyn SharingAuthorizer, fx: &SharingAuthorizerFixture<'_>) {
    let mut ids = Vec::new();
    for login in LOGINS {
        let a = fx
            .accounts
            .upsert(
                fx.tenant,
                AccountUpsert {
                    domain: fx.domain,
                    login: login.to_owned(),
                    display_name: login.to_owned(),
                    external_id: None,
                    enabled: true,
                    aliases: Vec::new(),
                    kind: AccountKind::User,
                    hidden_from_gal: false,
                },
            )
            .await
            .expect("upsert");
        ids.push(a.id);
    }
    let env = Env { authz, fx, ids };
    owner_entries_groups(&env).await;
    grants(&env).await;
    delegates(&env).await;
    policy_limits(&env).await;
    shared_with_home(&env).await;
}

/// Владелец, прямые записи, группы, anyone, «только занятость», чужая и несуществующая коллекция.
async fn owner_entries_groups(env: &Env<'_>) {
    let c = env.section("owner_entries_groups").await;
    env.grant(c.calendar, "bob@example.org", R::READ).await;
    env.grant(c.calendar, "team", R::WRITE).await;
    env.grant(c.calendar, ANYONE, R::FREE_BUSY).await;
    env.grant(
        c.book,
        "carol@example.org",
        R {
            admin: true,
            ..R::NONE
        },
    )
    .await;
    let owner = env.ids[0];

    // Владелец — всё без записей, в том числе на коллекцию, которой нет.
    let alice = env.p(0, &[]);
    assert_eq!(env.rights(&alice, c.calendar).await, R::ALL);
    assert_eq!(env.rights(&alice, CollectionId::new()).await, R::ALL);
    assert_eq!(env.authz.check(&alice, owner, c.book, R::ALL).await, Ok(()));

    // Прямая запись: чтение (с занятостью), без записи.
    let bob = env.p(1, &[]);
    assert_eq!(env.rights(&bob, c.calendar).await, R::READ);
    assert_eq!(
        env.authz.check(&bob, owner, c.calendar, R::WRITE).await,
        Err(Error::Forbidden)
    );
    assert_eq!(
        env.authz.check(&bob, owner, c.calendar, R::READ).await,
        Ok(())
    );
    // Через группу — запись; записи объединяются.
    assert_eq!(env.rights(&env.p(1, &["team"]), c.calendar).await, R::WRITE);
    // Только anyone — занятость без подробностей.
    let dave = env.p(3, &[]);
    assert_eq!(env.rights(&dave, c.calendar).await, R::FREE_BUSY);
    assert_eq!(
        env.authz.check(&dave, owner, c.calendar, R::READ).await,
        Err(Error::Forbidden)
    );
    assert_eq!(
        env.authz
            .check(&dave, owner, c.calendar, R::FREE_BUSY)
            .await,
        Ok(())
    );
    // admin включает всё.
    assert_eq!(env.rights(&env.p(2, &[]), c.book).await, R::ALL);
    // Без записей — ничего; неизвестная коллекция — ничего.
    assert_eq!(env.rights(&dave, c.book).await, R::NONE);
    assert_eq!(env.rights(&bob, CollectionId::new()).await, R::NONE);
    // Группа «anyone» и недопустимые в записях имена не добавляют прав.
    assert_eq!(
        env.rights(&env.p(3, &["anyone", "-team", "Domain Users"]), c.book)
            .await,
        R::NONE
    );

    // Отзыв.
    env.grant(c.calendar, "bob@example.org", R::NONE).await;
    assert_eq!(env.rights(&bob, c.calendar).await, R::FREE_BUSY);
}

/// Отрицательных прав нет: идентификатор с `-` не выдаётся; выдавать может владелец или субъект с `admin`.
async fn grants(env: &Env<'_>) {
    let c = env.section("grants").await;
    let (alice, bob, carol) = (env.p(0, &[]), env.p(1, &[]), env.p(2, &[]));
    let cal = c.calendar;
    assert!(matches!(
        env.check_grant(&alice, cal, "-bob@example.org", R::READ)
            .await,
        Err(Error::Invalid(_))
    ));
    assert!(matches!(
        env.check_grant(&alice, cal, "Domain Users", R::READ).await,
        Err(Error::Invalid(_))
    ));
    assert_eq!(
        env.check_grant(&alice, cal, "bob@example.org", R::WRITE)
            .await,
        Ok(())
    );
    // Без admin выдавать нельзя, даже с записью.
    env.grant(cal, "bob@example.org", R::WRITE).await;
    assert_eq!(
        env.check_grant(&bob, cal, "carol@example.org", R::READ)
            .await,
        Err(Error::Forbidden)
    );
    // С admin — можно.
    env.grant(
        cal,
        "carol@example.org",
        R {
            admin: true,
            ..R::NONE
        },
    )
    .await;
    assert_eq!(
        env.check_grant(&carol, cal, "dave@example.org", R::WRITE)
            .await,
        Ok(())
    );
    // Политика по умолчанию: «всем» — не больше чтения; отзыв — всегда.
    assert_eq!(env.check_grant(&alice, cal, ANYONE, R::READ).await, Ok(()));
    assert_eq!(
        env.check_grant(&alice, cal, ANYONE, R::WRITE).await,
        Err(Error::Forbidden)
    );
    assert_eq!(
        env.check_grant(&alice, cal, "Anyone", R::WRITE).await,
        Err(Error::Forbidden)
    );
    assert_eq!(env.check_grant(&alice, cal, ANYONE, R::NONE).await, Ok(()));
}

/// Представитель: права на все календари владельца, не на адресные книги; планирование — по признаку.
async fn delegates(env: &Env<'_>) {
    let c = env.section("delegates").await;
    env.delegate(1, DelegateLevel::Write, true).await;
    env.delegate(2, DelegateLevel::Read, false).await;
    env.grant(c.book, "carol@example.org", R::READ).await;

    let (bob, carol) = (env.p(1, &[]), env.p(2, &[]));
    assert_eq!(
        env.rights(&bob, c.calendar).await,
        R {
            schedule: true,
            ..R::WRITE
        }
    );
    assert_eq!(env.rights(&carol, c.calendar).await, R::READ);
    // Адресная книга — только по записи прав.
    assert_eq!(env.rights(&bob, c.book).await, R::NONE);
    assert_eq!(env.rights(&carol, c.book).await, R::READ);
    // Представительство не путает владельцев: календарь bob недоступен alice через её представителя.
    assert_eq!(
        env.authz
            .rights(&env.p(0, &[]), env.ids[1], CollectionId::new())
            .await,
        Ok(R::NONE)
    );

    let owner = env.ids[0];
    assert_eq!(
        env.authz.delegations(&bob).await,
        Ok(vec![Delegation {
            owner,
            level: DelegateLevel::Write,
            schedule_on_behalf: true,
        }])
    );
    assert_eq!(env.authz.delegations(&env.p(3, &[])).await, Ok(Vec::new()));

    // Назначает только владелец.
    let d = Delegate {
        delegate: env.ids[3],
        level: DelegateLevel::Write,
        schedule_on_behalf: true,
    };
    assert_eq!(
        env.authz.check_delegate(&env.p(0, &[]), owner, &d).await,
        Ok(())
    );
    assert_eq!(
        env.authz.check_delegate(&bob, owner, &d).await,
        Err(Error::Forbidden)
    );

    // Отзыв.
    env.fx
        .delegates
        .remove_delegate(env.alice(), env.ids[1])
        .await
        .expect("remove_delegate");
    assert_eq!(env.rights(&bob, c.calendar).await, R::NONE);
    assert_eq!(env.authz.delegations(&bob).await, Ok(Vec::new()));
}

/// Политика организации: ограничивает выдачу и уже выданные записи, права представителей и список общих.
async fn policy_limits(env: &Env<'_>) {
    let c = env.section("policy_limits").await;
    env.grant(c.calendar, "bob@example.org", R::WRITE).await;
    env.grant(c.calendar, "carol@example.org", R::WRITE).await;
    env.grant(c.calendar, "staff", R::WRITE).await;
    // Записано в обход политики (до её ужесточения): «всем» — запись.
    env.grant(c.book, ANYONE, R::WRITE).await;
    env.delegate(3, DelegateLevel::Write, true).await;

    let (bob, carol, dave) = (env.p(1, &[]), env.p(2, &[]), env.p(3, &[]));
    // По умолчанию — организация: именованным субъектам всё, «всем» — не больше чтения.
    assert_eq!(env.rights(&bob, c.calendar).await, R::WRITE);
    assert_eq!(env.rights(&dave, c.book).await, R::READ);

    // Полные права — только группе «Руководство» (carol); остальным — занятость. Неизвестная группа пропускается.
    let board = env.group("Руководство", &[2]).await;
    env.set_policy(SharingPolicy {
        scope: SharingScope::Groups {
            groups: vec![board, GroupId::new()],
        },
        max_outside: R::FREE_BUSY,
    })
    .await;
    assert_eq!(env.rights(&bob, c.calendar).await, R::FREE_BUSY);
    assert_eq!(env.rights(&carol, c.calendar).await, R::WRITE);
    assert_eq!(
        env.rights(&env.p(1, &["staff"]), c.calendar).await,
        R::FREE_BUSY
    );
    assert_eq!(env.rights(&carol, c.book).await, R::FREE_BUSY);
    // Представитель вне группы — только занятость: не представитель по сути.
    assert_eq!(env.rights(&dave, c.calendar).await, R::FREE_BUSY);
    assert_eq!(env.authz.delegations(&dave).await, Ok(Vec::new()));

    // Выдача: субъекту группы — всё, группе политики по имени — всё, вне группы — не больше max_outside.
    let alice = env.p(0, &[]);
    let cal = c.calendar;
    assert_eq!(
        env.check_grant(&alice, cal, "carol@example.org", R::WRITE)
            .await,
        Ok(())
    );
    assert_eq!(
        env.check_grant(&alice, cal, "руководство", R::WRITE).await,
        Ok(())
    );
    assert_eq!(
        env.check_grant(&alice, cal, "bob@example.org", R::FREE_BUSY)
            .await,
        Ok(())
    );
    assert_eq!(
        env.check_grant(&alice, cal, "bob@example.org", R::READ)
            .await,
        Err(Error::Forbidden)
    );
    assert_eq!(
        env.check_grant(&alice, cal, "staff", R::READ).await,
        Err(Error::Forbidden)
    );
    let d = |i: usize| Delegate {
        delegate: env.ids[i],
        level: DelegateLevel::Read,
        schedule_on_behalf: false,
    };
    let owner = env.ids[0];
    assert_eq!(env.authz.check_delegate(&alice, owner, &d(2)).await, Ok(()));
    assert_eq!(
        env.authz.check_delegate(&alice, owner, &d(1)).await,
        Err(Error::Forbidden)
    );

    // Общий доступ запрещён: записи и представители не действуют, владелец — всё.
    env.set_policy(SharingPolicy {
        scope: SharingScope::Nobody,
        ..SharingPolicy::default()
    })
    .await;
    assert_eq!(env.rights(&carol, c.calendar).await, R::NONE);
    assert_eq!(env.rights(&dave, c.calendar).await, R::NONE);
    assert_eq!(env.rights(&alice, c.calendar).await, R::ALL);
    assert_eq!(env.authz.shared_with(&carol).await, Ok(Vec::new()));
    assert_eq!(env.authz.delegations(&dave).await, Ok(Vec::new()));
    assert_eq!(
        env.check_grant(&alice, cal, "carol@example.org", R::FREE_BUSY)
            .await,
        Err(Error::Forbidden)
    );
    assert_eq!(
        env.check_grant(&alice, cal, "carol@example.org", R::NONE)
            .await,
        Ok(())
    );
}

/// Дом коллекций получателя: чужие коллекции с действующими правами, по (владелец, коллекция).
async fn shared_with_home(env: &Env<'_>) {
    let c = env.section("shared_with_home").await;
    env.grant(c.calendar, "bob@example.org", R::READ).await;
    env.grant(c.book, ANYONE, R::WRITE).await;
    let owner = env.ids[0];
    let mut want = vec![
        SharedCollection {
            owner,
            collection: c.calendar,
            rights: R::READ,
        },
        // «Всем» — запись, урезанная политикой до чтения.
        SharedCollection {
            owner,
            collection: c.book,
            rights: R::READ,
        },
    ];
    want.sort_by_key(|s| (s.owner, s.collection));
    assert_eq!(env.shared(&env.p(1, &[]), c).await, want);
    // Порядок — по (владелец, коллекция).
    let all = env
        .authz
        .shared_with(&env.p(1, &[]))
        .await
        .expect("shared_with");
    assert!(
        all.windows(2)
            .all(|w| (w[0].owner, w[0].collection) < (w[1].owner, w[1].collection))
    );
    // Своё не входит.
    let mine = env
        .authz
        .shared_with(&env.p(0, &[]))
        .await
        .expect("shared_with");
    assert!(mine.iter().all(|s| s.owner != owner));
    // Только «всем».
    assert_eq!(
        env.shared(&env.p(2, &[]), c).await,
        vec![SharedCollection {
            owner,
            collection: c.book,
            rights: R::READ,
        }]
    );
}
