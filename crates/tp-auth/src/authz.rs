//! Авторизация (SRS 4.5; CONTRACTS.md#Auth): [`Authorizer`] поверх [`AclStore`] — ACL папок (RFC 4314),
//! группы, общие ящики, Send As / Send on Behalf. Единый слой для всех протоколов.
//!
//! - Владелец имеет [`Rights::ALL`] на свою учётную запись и её папки (в ACL не хранится).
//! - Прочие — по ACL: записи имени входа, групп субъекта ([`Principal::groups`], с учётом вложенности) и
//!   [`ANYONE`] минус их отрицательные записи; считает хранилище ([`AclStore::acl_rights`]).
//! - Общий ящик — учётная запись, папки которой открыты другим по ACL; права на неё как на
//!   [`Resource::Account`] — объединение прав на её папки, доступные субъекту (с правом `l`).
//! - Отправка от адреса: свой адрес — Send As; адрес другой учётной записи — сильнейшее право, выданное
//!   субъекту или его группе ([`AclStore::send_right_of`]); иначе — запрет.
//!
//! Идентификаторы субъекта, недопустимые в ACL (группа каталога с пробелом в имени, [`ANYONE`] как имя
//! группы), пропускаются: записей ACL на них быть не может, а `anyone` хранилище добавляет само.
//! Адрес отправителя сопоставляется с учётной записью по имени входа (основной адрес,
//! [`AccountStore::by_login`]); псевдонимы — через каталог, когда появится контракт поиска адреса.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use tp_contracts::Result;
use tp_contracts::acl::{ANYONE, AclStore, validate_identifier};
use tp_contracts::auth::{Authorizer, Principal, Resource, Rights, SendRight};
use tp_contracts::meta::{AccountScope, AccountStore};

/// [`Authorizer`] поверх ACL в метаданных ([`AclStore`]) и учётных записей ([`AccountStore`]).
#[derive(Clone)]
pub struct AclAuthorizer {
    acl: Arc<dyn AclStore>,
    accounts: Arc<dyn AccountStore>,
}

impl fmt::Debug for AclAuthorizer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AclAuthorizer").finish_non_exhaustive()
    }
}

impl AclAuthorizer {
    /// Права по ACL `acl`; адреса отправителя — имена входа учётных записей `accounts`.
    pub fn new(acl: Arc<dyn AclStore>, accounts: Arc<dyn AccountStore>) -> Self {
        AclAuthorizer { acl, accounts }
    }
}

/// Идентификаторы субъекта в ACL: имя входа и группы, допустимые в ACL, без [`ANYONE`].
fn identifiers(p: &Principal) -> Vec<String> {
    std::iter::once(&p.login)
        .chain(&p.groups)
        .filter(|id| !id.eq_ignore_ascii_case(ANYONE) && validate_identifier(id, false).is_ok())
        .cloned()
        .collect()
}

fn scope(p: &Principal, account: tp_contracts::AccountId) -> AccountScope {
    AccountScope {
        tenant: p.tenant,
        account,
    }
}

#[async_trait]
impl Authorizer for AclAuthorizer {
    async fn rights(&self, principal: &Principal, resource: &Resource) -> Result<Rights> {
        match *resource {
            Resource::Account(account) if account == principal.account => Ok(Rights::ALL),
            Resource::Mailbox { account, .. } if account == principal.account => Ok(Rights::ALL),
            Resource::Mailbox { account, mailbox } => {
                let ids = identifiers(principal);
                self.acl
                    .acl_rights(scope(principal, account), mailbox, &ids)
                    .await
            }
            Resource::Account(account) => {
                let ids = identifiers(principal);
                Ok(self
                    .acl
                    .shared_mailboxes(principal.tenant, &ids)
                    .await?
                    .into_iter()
                    .filter(|m| m.account == account)
                    .fold(Rights::NONE, |acc, m| acc | m.rights))
            }
        }
    }

    async fn send_right(&self, principal: &Principal, from: &str) -> Result<Option<SendRight>> {
        if from.eq_ignore_ascii_case(&principal.login) {
            return Ok(Some(SendRight::SendAs));
        }
        let Some(owner) = self.accounts.by_login(principal.tenant, from).await? else {
            return Ok(None);
        };
        if owner.id == principal.account {
            return Ok(Some(SendRight::SendAs));
        }
        let ids = identifiers(principal);
        self.acl
            .send_right_of(scope(principal, owner.id), &ids)
            .await
    }
}

#[cfg(test)]
mod tests {
    use tp_contracts::meta::{MailboxStore, NewMailbox};
    use tp_contracts::{AccountId, Error, MailboxId, TenantId};
    use tp_testkit::MemAcl;

    use super::*;
    use crate::testutil::meta_accounts;

    fn r(s: &str) -> Rights {
        s.parse().expect("rights")
    }

    fn who(login: &str, account: AccountId, groups: &[&str]) -> Principal {
        Principal {
            tenant: TenantId::DEFAULT,
            account,
            login: login.into(),
            groups: groups.iter().map(|g| (*g).to_owned()).collect(),
            scopes: None,
        }
    }

    /// Учётные записи alice, bob, carol, dave и общий ящик sales; у каждого папка Shared.
    struct Fx {
        authz: AclAuthorizer,
        acl: MemAcl,
        ids: Vec<AccountId>,
        inbox: Vec<MailboxId>,
    }

    const LOGINS: [&str; 5] = [
        "alice@example.org",
        "bob@example.org",
        "carol@example.org",
        "dave@example.org",
        "sales@example.org",
    ];

    async fn fx() -> Fx {
        let list: Vec<_> = LOGINS.iter().map(|l| (*l, true)).collect();
        let (meta, _, ids) = meta_accounts(&list).await;
        let mut inbox = Vec::new();
        for id in &ids {
            let scope = AccountScope {
                tenant: TenantId::DEFAULT,
                account: *id,
            };
            let m = meta
                .create(
                    scope,
                    NewMailbox {
                        parent: None,
                        name: "Shared".into(),
                        role: None,
                    },
                )
                .await
                .expect("mailbox");
            inbox.push(m.id);
        }
        let acl = MemAcl::new(&meta);
        let authz = AclAuthorizer::new(Arc::new(acl.clone()), Arc::new(meta));
        Fx {
            authz,
            acl,
            ids,
            inbox,
        }
    }

    impl Fx {
        fn owner(&self, i: usize) -> AccountScope {
            AccountScope {
                tenant: TenantId::DEFAULT,
                account: self.ids[i],
            }
        }

        fn mailbox(&self, i: usize) -> Resource {
            Resource::Mailbox {
                account: self.ids[i],
                mailbox: self.inbox[i],
            }
        }

        fn p(&self, i: usize, groups: &[&str]) -> Principal {
            who(LOGINS[i], self.ids[i], groups)
        }
    }

    /// Матрица прав на папку Shared ящика sales (индекс 4) и на сам ящик.
    #[tokio::test]
    async fn rights_matrix() {
        let fx = fx().await;
        let sales = fx.owner(4);
        let set = |id: &'static str, rights: &'static str| {
            let acl = fx.acl.clone();
            let mailbox = fx.inbox[4];
            async move {
                acl.set_acl(sales, mailbox, id, r(rights))
                    .await
                    .expect("set_acl");
            }
        };
        set("alice@example.org", "lrswi").await;
        set("-alice@example.org", "w").await;
        set("team", "lrs").await;
        set(ANYONE, "l").await;
        set("-dave@example.org", "l").await;

        let inbox = fx.mailbox(4);
        let cases = [
            // Владелец — всё, без записей ACL.
            (fx.p(4, &[]), "lrswipkxtea"),
            // Прямая запись минус отрицательная, плюс anyone.
            (fx.p(0, &[]), "lrsi"),
            // Через группу (с учётом вложенности — группы уже развёрнуты в Principal).
            (fx.p(1, &["team"]), "lrs"),
            // Только anyone.
            (fx.p(2, &[]), "l"),
            // Отрицательная запись снимает право anyone.
            (fx.p(3, &[]), ""),
            // Группа с недопустимым в ACL именем и группа «anyone» не мешают и не добавляют прав.
            (fx.p(2, &["Domain Users", "anyone"]), "l"),
        ];
        for (p, want) in &cases {
            assert_eq!(
                fx.authz
                    .rights(p, &inbox)
                    .await
                    .expect("rights")
                    .to_string(),
                *want,
                "{}",
                p.login
            );
            assert_eq!(
                fx.authz
                    .rights(p, &Resource::Account(fx.ids[4]))
                    .await
                    .expect("account rights")
                    .to_string(),
                *want,
                "account of {}",
                p.login
            );
        }

        // Своя папка — всё; чужая без ACL — ничего; неизвестная папка — ничего.
        let alice = fx.p(0, &[]);
        assert_eq!(
            fx.authz.rights(&alice, &fx.mailbox(0)).await,
            Ok(Rights::ALL)
        );
        assert_eq!(
            fx.authz.rights(&alice, &fx.mailbox(1)).await,
            Ok(Rights::NONE)
        );
        let gone = Resource::Mailbox {
            account: fx.ids[1],
            mailbox: MailboxId::new(),
        };
        assert_eq!(fx.authz.rights(&alice, &gone).await, Ok(Rights::NONE));

        // check: нужны все права.
        assert_eq!(fx.authz.check(&alice, &inbox, r("lr")).await, Ok(()));
        assert_eq!(
            fx.authz.check(&alice, &inbox, r("lrw")).await,
            Err(Error::Forbidden)
        );
        assert_eq!(
            fx.authz.check(&alice, &fx.mailbox(0), Rights::ALL).await,
            Ok(())
        );
    }

    /// Send As / Send on Behalf: прямо, через группу, свой адрес, чужой без права, неизвестный адрес.
    #[tokio::test]
    async fn send_rights() {
        let fx = fx().await;
        let sales = fx.owner(4);
        fx.acl
            .set_send_delegation(sales, "alice@example.org", Some(SendRight::SendOnBehalf))
            .await
            .expect("delegate");
        fx.acl
            .set_send_delegation(sales, "team", Some(SendRight::SendAs))
            .await
            .expect("delegate group");
        fx.acl
            .set_send_delegation(sales, ANYONE, Some(SendRight::SendAs))
            .await
            .expect("delegate anyone");

        let sales_addr = "sales@example.org";
        let alice = fx.p(0, &[]);
        let cases = [
            (alice.clone(), sales_addr, Some(SendRight::SendOnBehalf)),
            // Группа даёт более сильное право.
            (fx.p(0, &["team"]), sales_addr, Some(SendRight::SendAs)),
            (
                fx.p(1, &["team"]),
                "Sales@Example.org",
                Some(SendRight::SendAs),
            ),
            // anyone в делегировании не учитывается.
            (fx.p(2, &[]), sales_addr, None),
            // Свой адрес — Send As (регистр не важен).
            (alice.clone(), "ALICE@example.org", Some(SendRight::SendAs)),
            (fx.p(4, &[]), sales_addr, Some(SendRight::SendAs)),
            // Чужой адрес без делегирования, неизвестный адрес.
            (alice.clone(), "bob@example.org", None),
            (alice.clone(), "nobody@example.org", None),
        ];
        for (p, from, want) in cases {
            assert_eq!(
                fx.authz.send_right(&p, from).await,
                Ok(want),
                "{} as {from}",
                p.login
            );
        }

        // Отзыв.
        fx.acl
            .set_send_delegation(sales, "alice@example.org", None)
            .await
            .expect("revoke");
        assert_eq!(fx.authz.send_right(&alice, sales_addr).await, Ok(None));
    }

    #[test]
    fn subject_identifiers() {
        let p = who(
            "u@example.org",
            AccountId::new(),
            &["staff", "Domain Users", "ANYONE", "-x"],
        );
        assert_eq!(
            identifiers(&p),
            vec!["u@example.org".to_owned(), "staff".to_owned()]
        );
    }
}
