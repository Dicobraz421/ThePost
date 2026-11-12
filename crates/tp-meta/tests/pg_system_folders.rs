//! CMP-05, META-42: системные папки при создании учётной записи и при первом входе; недостающая
//! определяется только по роли, при совпадении имени с папкой пользователя — имя с суффиксом (#205).

mod common;

use tp_contracts::meta::{
    AccountKind, AccountScope, AccountStore, AccountUpsert, ChangeLog, MailboxStore, SpecialUse,
};
use tp_contracts::{AccountId, DomainId, TenantId};
use tp_meta::{PgMeta, schema};

const ALL: [SpecialUse; 6] = [
    SpecialUse::Inbox,
    SpecialUse::Archive,
    SpecialUse::Drafts,
    SpecialUse::Junk,
    SpecialUse::Sent,
    SpecialUse::Trash,
];

async fn roles(meta: &PgMeta, scope: AccountScope) -> Vec<SpecialUse> {
    let mut r: Vec<_> = MailboxStore::list(meta, scope)
        .await
        .expect("list")
        .into_iter()
        .filter_map(|m| m.role)
        .collect();
    r.sort();
    r
}

/// Учётная запись, заведённая напрямую в базе (без папок), и, по выбору, папки `folders` (имя, роль).
async fn raw_account(
    pool: &tp_meta::pool::DbPool,
    tenant: TenantId,
    domain: DomainId,
    login: &str,
    folders: &[(&str, Option<&str>)],
) -> AccountScope {
    let account = AccountId::new();
    let mut tx = schema::begin(pool, tenant).await.expect("begin");
    sqlx::query(
        "INSERT INTO accounts (tenant_id, account_id, domain_id, login, display_name)
         VALUES ($1, $2, $3, $4, 'Raw')",
    )
    .bind(tenant.as_uuid())
    .bind(account.as_uuid())
    .bind(domain.as_uuid())
    .bind(login)
    .execute(&mut *tx)
    .await
    .expect("account");
    for (i, (name, role)) in folders.iter().enumerate() {
        sqlx::query(
            "INSERT INTO mailboxes (tenant_id, account_id, mailbox_id, name, role, uid_validity)
             VALUES ($1, $2, gen_random_uuid(), $3, $4, $5)",
        )
        .bind(tenant.as_uuid())
        .bind(account.as_uuid())
        .bind(name)
        .bind(role)
        .bind(i64::try_from(i + 1).expect("i64"))
        .execute(&mut *tx)
        .await
        .expect("mailbox");
    }
    tx.commit().await.expect("commit");
    AccountScope { tenant, account }
}

#[tokio::test]
async fn new_account_gets_six_folders() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    assert_eq!(roles(&meta, scope).await, ALL);
    let names: Vec<_> = MailboxStore::list(&meta, scope)
        .await
        .expect("list")
        .into_iter()
        .map(|m| m.name)
        .collect();
    assert!(names.iter().any(|n| n == "INBOX"));
    assert!(names.iter().any(|n| n == "Archive"));
    assert_eq!(names.len(), 6);
}

#[tokio::test]
async fn raw_account_without_folders_filled_on_first_open() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    let (domain, dname) = common::domain(meta.pool(), tenant).await;
    let scope = raw_account(meta.pool(), tenant, domain, &format!("raw@{dname}"), &[]).await;
    // Первое открытие ящика без `upsert`.
    assert_eq!(roles(&meta, scope).await, ALL);
    // Повторное — без дублей.
    let again = MailboxStore::list(&meta, scope).await.expect("list");
    assert_eq!(again.len(), 6);
    // Папки видны в журнале изменений (JMAP Mailbox/changes).
    let ch = meta
        .mailbox_changes(scope, tp_contracts::ModSeq::ZERO, 100)
        .await
        .expect("changes");
    assert_eq!(ch.created.len(), 6);
}

#[tokio::test]
async fn raw_account_with_inbox_filled_on_login() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    let (domain, dname) = common::domain(meta.pool(), tenant).await;
    let login = format!("old@{dname}");
    // INBOX с ролью, «Отправленные» с ролью под другим именем, «junk» без роли (Junk — «Junk 1»).
    let scope = raw_account(
        meta.pool(),
        tenant,
        domain,
        &login,
        &[
            ("INBOX", Some("inbox")),
            ("Отправленные", Some("sent")),
            ("junk", None),
        ],
    )
    .await;
    // Вход с обновлением записи из каталога.
    let acc = meta
        .upsert(
            tenant,
            AccountUpsert {
                domain,
                login,
                display_name: "Old".into(),
                external_id: None,
                enabled: true,
                aliases: Vec::new(),
                kind: AccountKind::User,
            },
        )
        .await
        .expect("upsert");
    assert_eq!(acc.id, scope.account);
    let folders = MailboxStore::list(&meta, scope).await.expect("list");
    let mut names: Vec<_> = folders.iter().map(|m| m.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        names,
        vec![
            "Archive",
            "Drafts",
            "INBOX",
            "Junk 1",
            "Trash",
            "junk",
            "Отправленные"
        ]
    );
    let mut r: Vec<_> = folders.iter().filter_map(|m| m.role).collect();
    r.sort();
    assert_eq!(
        r,
        vec![
            SpecialUse::Inbox,
            SpecialUse::Archive,
            SpecialUse::Drafts,
            SpecialUse::Junk,
            SpecialUse::Sent,
            SpecialUse::Trash
        ]
    );
    // Повторный вход ничего не добавляет.
    meta.upsert(
        tenant,
        AccountUpsert {
            domain,
            login: acc.login.clone(),
            display_name: "Old".into(),
            external_id: None,
            enabled: true,
            aliases: Vec::new(),
            kind: AccountKind::User,
        },
    )
    .await
    .expect("upsert");
    assert_eq!(
        MailboxStore::list(&meta, scope).await.expect("list").len(),
        7
    );
}

#[tokio::test]
async fn user_folder_with_system_name_kept() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let tenant = common::tenant(meta.pool()).await;
    let (domain, dname) = common::domain(meta.pool(), tenant).await;
    // «Junk» и «Junk 1» без роли — папки пользователя; системная Junk создаётся как «Junk 2».
    let scope = raw_account(
        meta.pool(),
        tenant,
        domain,
        &format!("junk@{dname}"),
        &[("Junk", None), ("Junk 1", None)],
    )
    .await;
    let before = MailboxStore::list(&meta, scope).await.expect("list");
    assert_eq!(roles(&meta, scope).await, ALL);
    let mut junk: Vec<_> = before
        .iter()
        .filter(|m| m.name.starts_with("Junk"))
        .map(|m| (m.name.as_str(), m.role))
        .collect();
    junk.sort_unstable_by_key(|(n, _)| *n);
    assert_eq!(
        junk,
        vec![
            ("Junk", None),
            ("Junk 1", None),
            ("Junk 2", Some(SpecialUse::Junk))
        ]
    );
    // Повторное открытие — без новых папок, папки пользователя не тронуты.
    let again = MailboxStore::list(&meta, scope).await.expect("list");
    assert_eq!(again.len(), 8);
    let ids = |v: &[tp_contracts::meta::Mailbox]| {
        let mut ids: Vec<_> = v.iter().map(|m| m.id).collect();
        ids.sort_unstable();
        ids
    };
    assert_eq!(ids(&before), ids(&again));
}
