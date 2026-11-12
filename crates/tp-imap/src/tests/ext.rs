//! NAMESPACE, QUOTA, ACL и общие папки (IMAP-10): примеры RFC 2342, RFC 9208, RFC 4314.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use tp_contracts::acl::AclStore;
use tp_contracts::auth::{Authorizer, Principal, Resource, Rights, SendRight};
use tp_contracts::meta::{
    Account, AccountKind, AccountScope, AccountStore, AccountUpsert, Flag, Flags, MailboxStore,
    MessageQuery, MessageStore, NewMailbox, QuotaUsage, SpecialUse,
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

    fn by_ids<'life0, 'life1, 'async_trait>(
        &'life0 self,
        tenant: TenantId,
        ids: &'life1 [AccountId],
    ) -> Fut<'async_trait, Vec<Account>>
    where
        'life0: 'async_trait,
        'life1: 'async_trait,
        Self: 'async_trait,
    {
        self.0.by_ids(tenant, ids)
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
        self.bob_with_scope().await.0
    }

    /// Вторая учётная запись `bob` и её контекст.
    async fn bob_with_scope(&self) -> (Client, AccountScope) {
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
                    aliases: Vec::new(),
                    kind: AccountKind::User,
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
        let scope = AccountScope {
            tenant: TenantId::DEFAULT,
            account: account.id,
        };
        (self.login(BOB, "pw").await, scope)
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
    // Копирование из общей папки к себе — по праву `r` на источник.
    assert!(b.cmd("F4 COPY 1 INBOX").await.contains("F4 OK [COPYUID"));
    // MOVE из папки — нужны `t` и `e`.
    assert!(b.cmd("F5 MOVE 1 INBOX").await.contains("F5 NO"));
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

/// Письма папки владельца `scope` с флагами.
async fn messages(x: &Ext, scope: AccountScope, mailbox: MailboxId) -> Vec<Flags> {
    MessageStore::list(&x.env.meta, scope, mailbox, MessageQuery::All, 100)
        .await
        .expect("list")
        .into_iter()
        .map(|m| m.flags)
        .collect()
}

async fn share(x: &Ext, mailbox: MailboxId, rights: &str) {
    x.acl
        .set_acl(x.env.scope(), mailbox, BOB, rights.parse().expect("rights"))
        .await
        .expect("acl");
}

#[tokio::test]
async fn shared_store_and_expunge_by_rights() {
    let x = Ext::new(None).await;
    let projects = x.projects().await;
    let flagged: Flags = [Flag::Flagged].into_iter().collect();
    x.env.deliver(projects, PLAIN, flagged).await;
    share(&x, projects, "lrs").await;
    let mut b = x.bob().await;
    let shared = format!("\"Other Users/{LOGIN}/Projects\"");
    // Право `s` — папка для записи, в PERMANENTFLAGS только `\Seen`.
    let r = b.cmd(&format!("S1 SELECT {shared}")).await;
    assert!(r.contains("* OK [PERMANENTFLAGS (\\Seen)]"), "{r}");
    assert!(r.contains("S1 OK [READ-WRITE]"), "{r}");
    // Флаг без права пропускается.
    let r = b.cmd("T1 STORE 1 +FLAGS (\\Seen \\Flagged)").await;
    assert!(r.contains("* 1 FETCH (FLAGS (\\Seen))"), "{r}");
    assert!(r.contains("T1 OK"), "{r}");
    // Ни одного разрешённого флага — NOPERM.
    let r = b.cmd("T2 STORE 1 +FLAGS (\\Deleted)").await;
    assert!(r.contains("T2 NO [NOPERM]"), "{r}");
    // Замена набора не трогает флаги без права (`\Flagged` у письма 2 остаётся).
    let r = b.cmd("T3 STORE 1:2 FLAGS (\\Seen)").await;
    assert!(r.contains("* 2 FETCH (FLAGS (\\Seen \\Flagged))"), "{r}");
    let r = b.cmd("T4 STORE 2 -FLAGS (\\Seen)").await;
    assert!(r.contains("* 2 FETCH (FLAGS (\\Flagged))"), "{r}");
    // EXPUNGE без `e` — NOPERM; CLOSE не удаляет.
    assert!(b.cmd("E1 EXPUNGE").await.contains("E1 NO [NOPERM]"));
    // Права расширены (`t`, `e`) — проверяются при каждой команде, без повторного SELECT.
    share(&x, projects, "lrste").await;
    let r = b.cmd("T5 STORE 1 +FLAGS.SILENT (\\Deleted)").await;
    assert!(r.contains("T5 OK"), "{r}");
    let r = b.cmd("E2 EXPUNGE").await;
    assert!(r.contains("* 1 EXPUNGE\r\nE2 OK"), "{r}");
    assert_eq!(messages(&x, x.env.scope(), projects).await.len(), 1);
    let trash = x.env.mailbox(SpecialUse::Trash);
    assert_eq!(messages(&x, x.env.scope(), trash).await.len(), 1);
    // Права отозваны до `r` — запись отклоняется в уже выбранной папке.
    share(&x, projects, "lr").await;
    assert!(
        b.cmd("T6 STORE 1 +FLAGS (\\Seen)")
            .await
            .contains("T6 NO [NOPERM]")
    );
    assert!(b.cmd("E3 EXPUNGE").await.contains("E3 NO [NOPERM]"));
    // Только `e`: для записи, но STORE запрещён.
    share(&x, projects, "lre").await;
    let r = b.cmd(&format!("S2 SELECT {shared}")).await;
    assert!(r.contains("* OK [PERMANENTFLAGS ()]"), "{r}");
    assert!(r.contains("S2 OK [READ-WRITE]"), "{r}");
    assert!(
        b.cmd("T7 STORE 1 +FLAGS (\\Seen)")
            .await
            .contains("T7 NO [NOPERM]")
    );
}

#[tokio::test]
async fn shared_append_copy_move_by_insert_right() {
    let x = Ext::new(None).await;
    let projects = x.projects().await;
    share(&x, projects, "lr").await;
    let (mut b, bob) = x.bob_with_scope().await;
    let shared = format!("\"Other Users/{LOGIN}/Projects\"");
    let msg = std::str::from_utf8(PLAIN).expect("utf8");
    let append = |tag: &str, flags: &str| {
        format!(
            "{tag} APPEND {shared} ({flags}) {{{}+}}\r\n{msg}",
            PLAIN.len()
        )
    };
    // Без `i` — APPEND и COPY в папку отклоняются.
    let r = b.cmd(&append("A1", "")).await;
    assert!(r.contains("A1 NO [NOPERM]"), "{r}");
    assert_eq!(messages(&x, x.env.scope(), projects).await.len(), 1);
    // С `i` (без `s`, `w`): письмо кладётся владельцу, флаги без права не ставятся.
    share(&x, projects, "lri").await;
    let r = b.cmd(&append("A2", "\\Seen \\Flagged")).await;
    assert!(r.contains("A2 OK [APPENDUID"), "{r}");
    let flags = messages(&x, x.env.scope(), projects).await;
    assert_eq!(flags.len(), 2);
    assert!(flags.iter().all(Flags::is_empty), "{flags:?}");
    // COPY и MOVE из своей папки в общую.
    let inbox = x
        .env
        .meta
        .mailbox_by_role(bob, SpecialUse::Inbox)
        .expect("inbox");
    let own = format!("O1 APPEND INBOX (\\Seen) {{{}+}}\r\n{msg}", PLAIN.len());
    assert!(b.cmd(&own).await.contains("O1 OK"));
    let own = format!("O2 APPEND INBOX () {{{}+}}\r\n{msg}", PLAIN.len());
    assert!(b.cmd(&own).await.contains("O2 OK"));
    assert!(b.cmd("S1 SELECT INBOX").await.contains("S1 OK"));
    let r = b.cmd(&format!("C1 COPY 1 {shared}")).await;
    assert!(r.contains("C1 OK [COPYUID"), "{r}");
    let r = b.cmd(&format!("C2 MOVE 2 {shared}")).await;
    assert!(r.contains("* 2 EXPUNGE"), "{r}");
    assert!(r.contains("C2 OK"), "{r}");
    let flags = messages(&x, x.env.scope(), projects).await;
    assert_eq!(flags.len(), 4);
    assert!(flags.iter().all(Flags::is_empty), "{flags:?}");
    assert_eq!(messages(&x, bob, inbox).await.len(), 1);
    let bob_trash = x
        .env
        .meta
        .mailbox_by_role(bob, SpecialUse::Trash)
        .expect("trash");
    assert_eq!(messages(&x, bob, bob_trash).await.len(), 1);
    // С `s`: `\Seen` у копии сохраняется.
    share(&x, projects, "lris").await;
    assert!(
        b.cmd(&format!("C3 COPY 1 {shared}"))
            .await
            .contains("C3 OK")
    );
    let flags = messages(&x, x.env.scope(), projects).await;
    assert_eq!(flags.iter().filter(|f| f.contains(&Flag::Seen)).count(), 1);
}

#[tokio::test]
async fn shared_access_audited() {
    use tp_contracts::audit::{AuditAction, AuditActorKind};
    let x = Ext::new(None).await;
    let projects = x.projects().await;
    share(&x, projects, "lrstea").await;
    let (mut b, bob) = x.bob_with_scope().await;
    let shared = format!("\"Other Users/{LOGIN}/Projects\"");
    assert!(
        b.cmd(&format!("S1 SELECT {shared}"))
            .await
            .contains("S1 OK")
    );
    // Метаданные — не чтение содержимого.
    assert!(b.cmd("F1 FETCH 1 (FLAGS UID)").await.contains("F1 OK"));
    assert!(b.cmd("F2 FETCH 1 BODY[TEXT]").await.contains("Hello"));
    assert!(
        b.cmd("T1 STORE 1 +FLAGS.SILENT (\\Deleted)")
            .await
            .contains("T1 OK")
    );
    assert!(b.cmd("E1 EXPUNGE").await.contains("E1 OK"));
    let r = b
        .cmd(&format!("A1 SETACL {shared} carol@example.org lr"))
        .await;
    assert!(r.contains("A1 OK"), "{r}");
    let e = x.env.audit.entries();
    let actions: Vec<AuditAction> = e.iter().map(|e| e.action).collect();
    assert_eq!(
        actions,
        [
            AuditAction::Login,
            AuditAction::Read,
            AuditAction::Delete,
            AuditAction::ChangeRights
        ]
    );
    for entry in &e {
        assert_eq!(entry.scope, x.env.scope());
        assert_eq!(entry.actor, BOB);
        assert_eq!(entry.actor_account, Some(bob.account));
        assert_eq!(entry.actor_kind, AuditActorKind::Delegate);
        assert_eq!(entry.object.mailbox, Some(projects));
        assert_eq!(entry.client.as_deref(), Some("imap 192.0.2.7"));
    }
    assert!(e[1].object.message.is_some());
    assert_eq!(e[1].object.message, e[2].object.message);
    assert_eq!(e[3].object.detail.as_deref(), Some("carol@example.org lr"));
    // Чтение владельцем своей папки не записывается.
    x.env.deliver(projects, PLAIN, Default::default()).await;
    let mut o = x.login(LOGIN, PASSWORD).await;
    assert!(o.cmd("S2 SELECT Projects").await.contains("S2 OK"));
    assert!(o.cmd("F3 FETCH 1:* BODY[]").await.contains("F3 OK"));
    assert_eq!(x.env.audit.entries().len(), 4);
}
