//! NAMESPACE, QUOTA, ACL и общие папки (IMAP-10): примеры RFC 2342, RFC 9208, RFC 4314.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tp_contracts::acl::AclStore;
use tp_contracts::auth::{Authorizer, Principal, Resource, Rights, SendRight};
use tp_contracts::meta::{
    Account, AccountScope, AccountStore, AccountUpsert, MailboxStore, NewMailbox, QuotaUsage,
    SpecialUse,
};
use tp_contracts::{AccountId, DomainId, MailboxId, Result, TenantId};
use tp_testkit::{MemAcl, MemMetadata};

use super::{Client, Env, LOGIN, PASSWORD};
use crate::ext::Extensions;

const PLAIN: &[u8] = b"From: a@example.org\r\nTo: b@example.org\r\nSubject: hi\r\n\r\nHello\r\n";
const BOB: &str = "bob@example.org";

type Fut<'a, T> = Pin<Box<dyn Future<Output = Result<T>> + Send + 'a>>;

/// Права как у `tp-auth`: владелец — все, прочие — по ACL.
struct TestAuthz(MemAcl);

impl Authorizer for TestAuthz {
    fn rights<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        principal: &'life1 Principal,
        resource: &'life2 Resource,
    ) -> Fut<'async_trait, Rights>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            match *resource {
                Resource::Mailbox { account, mailbox } if account != principal.account => {
                    let owner = AccountScope {
                        tenant: principal.tenant,
                        account,
                    };
                    let ids = vec![principal.login.clone()];
                    self.0.acl_rights(owner, mailbox, &ids).await
                }
                Resource::Account(account) if account != principal.account => Ok(Rights::NONE),
                _ => Ok(Rights::ALL),
            }
        })
    }

    fn send_right<'life0, 'life1, 'life2, 'async_trait>(
        &'life0 self,
        _principal: &'life1 Principal,
        _from: &'life2 str,
    ) -> Fut<'async_trait, Option<SendRight>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        'life2: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async { Ok(None) })
    }
}

/// Учётные записи с лимитом квоты.
struct Limited(MemMetadata, Option<u64>);

impl AccountStore for Limited {
    fn get<'life0, 'async_trait>(
        &'life0 self,
        tenant: TenantId,
        id: AccountId,
    ) -> Fut<'async_trait, Option<Account>>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        AccountStore::get(&self.0, tenant, id)
    }

    fn by_login<'life0, 'life1, 'async_trait>(
        &'life0 self,
        tenant: TenantId,
        login: &'life1 str,
    ) -> Fut<'async_trait, Option<Account>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        self.0.by_login(tenant, login)
    }

    fn upsert<'life0, 'async_trait>(
        &'life0 self,
        tenant: TenantId,
        data: AccountUpsert,
    ) -> Fut<'async_trait, Account>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        self.0.upsert(tenant, data)
    }

    fn quota<'life0, 'async_trait>(
        &'life0 self,
        tenant: TenantId,
        id: AccountId,
    ) -> Fut<'async_trait, QuotaUsage>
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(async move {
            let mut q = self.0.quota(tenant, id).await?;
            q.limit_bytes = self.1;
            Ok(q)
        })
    }
}

struct Ext {
    env: Env,
    acl: MemAcl,
    limit: Option<u64>,
}

impl Ext {
    async fn new(limit: Option<u64>) -> Self {
        let env = Env::new().await;
        let acl = MemAcl::new(&env.meta);
        Self { env, acl, limit }
    }

    fn extensions(&self) -> Extensions {
        Extensions {
            accounts: Some(Arc::new(Limited(self.env.meta.clone(), self.limit))),
            acl: Some(Arc::new(self.acl.clone())),
            authz: Some(Arc::new(TestAuthz(self.acl.clone()))),
        }
    }

    async fn login(&self, login: &str, password: &str) -> Client {
        let mut c = self.env.client_ext(true, self.extensions()).await;
        let r = c.cmd(&format!("L1 LOGIN {login} {password}")).await;
        assert!(r.contains("L1 OK"), "{r}");
        c
    }

    /// Вторая учётная запись `bob`.
    async fn bob(&self) -> Client {
        let account = self
            .env
            .meta
            .upsert(
                TenantId::DEFAULT,
                AccountUpsert {
                    domain: DomainId::new(),
                    login: BOB.into(),
                    display_name: "Bob".into(),
                    external_id: None,
                    enabled: true,
                },
            )
            .await
            .expect("account");
        let p = Principal {
            tenant: TenantId::DEFAULT,
            account: account.id,
            login: BOB.into(),
            groups: Default::default(),
            scopes: None,
        };
        self.env.auth.add_password(BOB, "pw", p);
        self.login(BOB, "pw").await
    }

    /// Папка `Projects` пользователя с одним письмом.
    async fn projects(&self) -> MailboxId {
        let mb = MailboxStore::create(
            &self.env.meta,
            self.env.scope(),
            NewMailbox {
                parent: None,
                name: "Projects".into(),
                role: None,
            },
        )
        .await
        .expect("create");
        self.env.deliver(mb.id, PLAIN, Default::default()).await;
        mb.id
    }
}

#[tokio::test]
async fn without_extensions() {
    let env = Env::new().await;
    let mut c = env.logged_in().await;
    let caps = c.cmd("C1 CAPABILITY").await;
    assert!(caps.contains(" NAMESPACE"), "{caps}");
    assert!(!caps.contains("QUOTA") && !caps.contains(" ACL"), "{caps}");
    assert_eq!(
        c.cmd("N1 NAMESPACE").await,
        "* NAMESPACE ((\"\" \"/\")) NIL NIL\r\nN1 OK NAMESPACE completed\r\n"
    );
    assert!(c.cmd("Q1 GETQUOTAROOT INBOX").await.contains("Q1 BAD"));
    assert!(c.cmd("A1 MYRIGHTS INBOX").await.contains("A1 BAD"));
}

#[tokio::test]
async fn namespace_and_quota() {
    let x = Ext::new(Some(1024 * 1024)).await;
    let inbox = x.env.mailbox(SpecialUse::Inbox);
    x.env.deliver(inbox, PLAIN, Default::default()).await;
    let mut c = x.login(LOGIN, PASSWORD).await;
    let caps = c.cmd("C1 CAPABILITY").await;
    for cap in [" QUOTA ", " QUOTA=RES-STORAGE", " ACL ", " RIGHTS=texk"] {
        assert!(caps.contains(cap), "{cap}: {caps}");
    }
    assert_eq!(
        c.cmd("N1 NAMESPACE").await,
        "* NAMESPACE ((\"\" \"/\")) ((\"Other Users/\" \"/\")) NIL\r\nN1 OK NAMESPACE completed\r\n"
    );
    // RFC 9208, 4.3: корень квоты и использование в КиБ.
    assert_eq!(
        c.cmd("Q1 GETQUOTAROOT INBOX").await,
        "* QUOTAROOT INBOX \"\"\r\n* QUOTA \"\" (STORAGE 1 1024)\r\nQ1 OK GETQUOTAROOT completed\r\n"
    );
    assert_eq!(
        c.cmd("Q2 GETQUOTA \"\"").await,
        "* QUOTA \"\" (STORAGE 1 1024)\r\nQ2 OK GETQUOTA completed\r\n"
    );
    assert!(
        c.cmd("Q3 GETQUOTA other")
            .await
            .contains("Q3 NO [NONEXISTENT]")
    );
    assert!(
        c.cmd("Q4 SETQUOTA \"\" (STORAGE 10)")
            .await
            .contains("Q4 NO [NOPERM]")
    );
    assert!(c.cmd("Q5 GETQUOTAROOT nope").await.contains("Q5 NO"));
}

#[tokio::test]
async fn quota_without_limit() {
    let x = Ext::new(None).await;
    let mut c = x.login(LOGIN, PASSWORD).await;
    assert_eq!(
        c.cmd("Q1 GETQUOTAROOT INBOX").await,
        "* QUOTAROOT INBOX\r\nQ1 OK GETQUOTAROOT completed\r\n"
    );
    assert!(c.cmd("Q2 GETQUOTA \"\"").await.contains("Q2 NO"));
}

#[tokio::test]
async fn acl_of_own_mailbox() {
    let x = Ext::new(None).await;
    let mut c = x.login(LOGIN, PASSWORD).await;
    assert_eq!(
        c.cmd("A1 MYRIGHTS INBOX").await,
        "* MYRIGHTS INBOX lrswipkxtea\r\nA1 OK MYRIGHTS completed\r\n"
    );
    assert!(
        c.cmd(&format!("A2 SETACL INBOX {BOB} lr"))
            .await
            .contains("A2 OK")
    );
    assert_eq!(
        c.cmd("A3 GETACL INBOX").await,
        format!("* ACL INBOX {LOGIN} lrswipkxtea {BOB} lr\r\nA3 OK GETACL completed\r\n")
    );
    // RFC 4314, 3.1: `+`/`-` меняют права, устаревшее `d` — `xte`.
    assert!(
        c.cmd(&format!("A4 SETACL INBOX {BOB} +wd"))
            .await
            .contains("A4 OK")
    );
    assert!(
        c.cmd(&format!("A5 SETACL INBOX {BOB} -r"))
            .await
            .contains("A5 OK")
    );
    assert!(
        c.cmd("A6 GETACL INBOX")
            .await
            .contains(&format!("{BOB} lwxte\r\n"))
    );
    assert!(
        c.cmd(&format!("A7 DELETEACL INBOX {BOB}"))
            .await
            .contains("A7 OK")
    );
    assert_eq!(
        c.cmd("A8 GETACL INBOX").await,
        format!("* ACL INBOX {LOGIN} lrswipkxtea\r\nA8 OK GETACL completed\r\n")
    );
    assert_eq!(
        c.cmd(&format!("A9 LISTRIGHTS INBOX {BOB}")).await,
        format!(
            "* LISTRIGHTS INBOX {BOB} \"\" l r s w i p k x t e a\r\nA9 OK LISTRIGHTS completed\r\n"
        )
    );
    // Права владельца не меняются; неизвестное право — BAD; нет папки — NO.
    assert!(
        c.cmd(&format!("B1 SETACL INBOX {LOGIN} l"))
            .await
            .contains("B1 NO [CANNOT]")
    );
    assert!(
        c.cmd(&format!("B2 SETACL INBOX {BOB} lz"))
            .await
            .contains("B2 BAD")
    );
    assert!(
        c.cmd("B3 MYRIGHTS nope")
            .await
            .contains("B3 NO [NONEXISTENT]")
    );
}

#[tokio::test]
async fn shared_mailbox_read_only() {
    let x = Ext::new(None).await;
    let projects = x.projects().await;
    x.acl
        .set_acl(x.env.scope(), projects, BOB, "lr".parse().expect("rights"))
        .await
        .expect("acl");
    let mut b = x.bob().await;
    let shared = format!("\"Other Users/{LOGIN}/Projects\"");
    let r = b.cmd("L2 LIST \"\" \"*\"").await;
    assert!(
        r.contains("* LIST (\\Noselect \\HasChildren) \"/\" \"Other Users\"\r\n"),
        "{r}"
    );
    assert!(
        r.contains(&format!(
            "* LIST (\\Noselect \\HasChildren) \"/\" \"Other Users/{LOGIN}\"\r\n"
        )),
        "{r}"
    );
    assert!(
        r.contains(&format!("* LIST (\\HasNoChildren) \"/\" {shared}\r\n")),
        "{r}"
    );
    // Папки владельца без ACL не видны.
    assert!(!r.contains(&format!("{LOGIN}/INBOX")), "{r}");
    let r = b.cmd("L3 LIST \"Other Users/\" \"%\"").await;
    assert!(r.contains(&format!("\"Other Users/{LOGIN}\"")), "{r}");
    assert!(!r.contains("Projects"), "{r}");
    assert!(
        b.cmd(&format!("S1 STATUS {shared} (MESSAGES)"))
            .await
            .contains("(MESSAGES 1)")
    );
    let r = b.cmd(&format!("S2 SELECT {shared}")).await;
    assert!(r.contains("* 1 EXISTS"), "{r}");
    assert!(r.contains("S2 OK [READ-ONLY]"), "{r}");
    assert!(b.cmd("F1 FETCH 1 BODY[TEXT]").await.contains("Hello"));
    assert!(b.cmd("F2 SEARCH ALL").await.contains("* SEARCH 1\r\n"));
    assert!(b.cmd("F3 STORE 1 +FLAGS (\\Seen)").await.contains("F3 NO"));
    assert!(b.cmd("F4 COPY 1 INBOX").await.contains("F4 NO [NOPERM]"));
    assert_eq!(
        b.cmd(&format!("A1 MYRIGHTS {shared}")).await,
        format!("* MYRIGHTS {shared} lr\r\nA1 OK MYRIGHTS completed\r\n")
    );
    assert!(
        b.cmd(&format!("A2 GETACL {shared}"))
            .await
            .contains("A2 NO [NOPERM]")
    );
    // Чужая папка без ACL — как несуществующая.
    assert!(
        b.cmd(&format!("A3 SELECT \"Other Users/{LOGIN}/INBOX\""))
            .await
            .contains("A3 NO [NONEXISTENT]")
    );
    // Право `l` без `r`: папка видна, но не выбирается.
    x.acl
        .set_acl(x.env.scope(), projects, BOB, "l".parse().expect("rights"))
        .await
        .expect("acl");
    assert!(
        b.cmd(&format!("A4 EXAMINE {shared}"))
            .await
            .contains("A4 NO [NOPERM]")
    );
}
