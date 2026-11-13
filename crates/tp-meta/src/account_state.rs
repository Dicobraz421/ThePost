//! Состояние ящика учётной записи на PostgreSQL (CONTRACTS.md#Metadata; CTR-35, CTR-50; решения
//! 2026-11-04-exchange-parity «Удалённые ящики», 44): отключение со сроком хранения, восстановление, подключение
//! отключённого ящика к учётной записи каталога, окончательное удаление.
//!
//! Состояние — колонки `accounts.disabled_until` (`NULL` — действует) и `accounts.removing` (CTR-78: окончательное
//! удаление начато, `begin_removal`; восстановление, подключение и перенос срока — `Conflict`, строка каталога
//! блокируется — отметка и восстановление атомарны между собой). Данные ящика — на шарде учётной
//! записи ([`BOX_TABLES`]), каталог — в основной БД. Когда ящик в основной БД, операция — одна транзакция; ящик на
//! другом шарде — сначала транзакция ящика, затем каталога (сеть внутри открытой транзакции не используется):
//! изменение состояния между ними оставляет ящик без папок, их дополняет первое открытие (`MailboxStore::list`).

use std::collections::HashMap;

use sqlx::PgConnection;
use time::OffsetDateTime;
use tp_contracts::events::{CoreEvent, CoreEventKind};
use tp_contracts::meta::{
    Account, AccountScope, AccountState, DisabledAccount, ExistingContent, NewMailbox,
};
use tp_contracts::{AccountId, Error, MailboxId, MessageId, Result, TenantId};
use uuid::Uuid;

use crate::accounts::AccountRow;
use crate::changelog::{self, Change, Op};
use crate::mailboxes;
use crate::retention::{TrashFor, micros};
use crate::schema::{begin, begin_system, db_err, to_i64, to_u32};
use crate::shards::ShardId;
use crate::{BoxDb, PgMeta};

/// Таблицы ящика учётной записи на его шарде (кроме писем и состояния ящика), зависимые раньше главных.
/// Журнал событий ядра не чистится: его позиции читают подписчики, срок хранения — `EventLog::purge`.
const BOX_TABLES: [&str; 15] = [
    "mailbox_acl",
    "send_delegations",
    "index_manifests",
    "dav_tombstones",
    "dav_objects",
    "dav_busy",
    "dav_inbox_tombstones",
    "dav_inbox",
    "dav_collections",
    "sieve_scripts",
    "vacation_log",
    "vacation",
    "changelog",
    "mailboxes",
    "mailbox_state",
];

/// Строка каталога с состоянием.
#[derive(sqlx::FromRow)]
struct StateRow {
    #[sqlx(flatten)]
    account: AccountRow,
    disabled_until: Option<OffsetDateTime>,
    removing: bool,
}

const STATE_COLUMNS: &str = "account_id, tenant_id, domain_id, login, display_name, external_id, enabled, aliases, kind, disabled_until, removing";

fn state((until, removing): (Option<OffsetDateTime>, bool)) -> AccountState {
    match until {
        Some(until) if removing => AccountState::Removing { until },
        Some(until) => AccountState::Disabled { until },
        None => AccountState::Active,
    }
}

/// `remove_disabled` ящика без отметки удаления.
const NOT_REMOVING: &str = "remove_disabled: ящик не удаляется — сначала begin_removal";

fn scope(tenant: TenantId, account: AccountId) -> AccountScope {
    AccountScope { tenant, account }
}

/// Строки каталога `ids` организации с блокировкой до конца транзакции (по возрастанию — без взаимоблокировок).
async fn lock_rows(
    tx: &mut PgConnection,
    tenant: TenantId,
    ids: &[AccountId],
) -> Result<HashMap<AccountId, StateRow>> {
    let ids: Vec<Uuid> = ids.iter().map(|a| *a.as_uuid()).collect();
    let rows: Vec<StateRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {STATE_COLUMNS} FROM accounts WHERE tenant_id = $1 AND account_id = ANY($2)
         ORDER BY account_id FOR UPDATE"
    )))
    .bind(tenant.as_uuid())
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(rows
        .into_iter()
        .map(|r| (AccountId::from_uuid(r.account.account_id), r))
        .collect())
}

/// Есть ли у ящика письма (в том числе в корзине и скрытые).
async fn has_messages(tx: &mut PgConnection, scope: AccountScope) -> Result<bool> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM messages WHERE tenant_id = $1 AND account_id = $2)",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err)
}

/// Удаляет данные ящика без писем ([`BOX_TABLES`]); возвращает его папки (их политики — в каталоге).
async fn drop_box(tx: &mut PgConnection, scope: AccountScope) -> Result<Vec<Uuid>> {
    let folders: Vec<Uuid> = sqlx::query_scalar(
        "SELECT mailbox_id FROM mailboxes WHERE tenant_id = $1 AND account_id = $2",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    for table in BOX_TABLES {
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "DELETE FROM {table} WHERE tenant_id = $1 AND account_id = $2"
        )))
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    }
    Ok(folders)
}

/// Удаляет учётную запись из каталога со строками, ссылающимися на неё без внешнего ключа: списки
/// отправителей, политики уровня учётной записи и её папок `folders`, членство в группах. Оргатрибуты и пароли
/// приложений удаляются внешним ключом.
async fn drop_catalog(tx: &mut PgConnection, scope: AccountScope, folders: &[Uuid]) -> Result<()> {
    let (t, a) = (scope.tenant.as_uuid(), scope.account.as_uuid());
    for sql in [
        "DELETE FROM sender_lists WHERE tenant_id = $1 AND account_id = $2",
        "DELETE FROM group_members WHERE tenant_id = $1 AND account_id = $2",
        "DELETE FROM group_moderators WHERE tenant_id = $1 AND account_id = $2",
        "DELETE FROM group_senders WHERE tenant_id = $1 AND account_id = $2",
        "DELETE FROM policies WHERE tenant_id = $1 AND level = 'account' AND object_id = $2",
    ] {
        sqlx::query(sql)
            .bind(t)
            .bind(a)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
    }
    sqlx::query(
        "DELETE FROM policies WHERE tenant_id = $1 AND level = 'mailbox' AND object_id = ANY($2)",
    )
    .bind(t)
    .bind(folders)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    sqlx::query("DELETE FROM accounts WHERE tenant_id = $1 AND account_id = $2")
        .bind(t)
        .bind(a)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    Ok(())
}

/// Наименьшее свободное у родителя `parent` ящика `scope` имя (без учёта регистра ASCII): `name`, «`name` 1»,
/// «`name` 2»…
async fn free_name(
    tx: &mut PgConnection,
    scope: AccountScope,
    parent: Option<Uuid>,
    name: &str,
) -> Result<String> {
    let taken: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM mailboxes WHERE tenant_id = $1 AND account_id = $2 AND parent_id IS NOT DISTINCT FROM $3",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(parent)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    let free = |n: &str| !taken.iter().any(|t| t.eq_ignore_ascii_case(n));
    if free(name) {
        return Ok(name.to_owned());
    }
    (1..=taken.len())
        .map(|i| format!("{name} {i}"))
        .find(|n| free(n))
        .ok_or_else(|| Error::internal("нет свободного имени папки"))
}

/// Переносит письма ящика `from` в ящик `into` (одна БД) по режиму `mode` ([`ExistingContent`]): папки и письма
/// с теми же `MessageId`, новыми UID и `ModSeq` ящика `into`; изменения и события — у `into`. Письма `from`
/// удаляются (ссылки на объекты BLOB не меняются). `trash` — срок корзины `into` для режима `Delete`.
async fn absorb(
    tx: &mut PgConnection,
    into: AccountScope,
    from: AccountScope,
    mode: ExistingContent,
    trash: Option<&TrashFor>,
) -> Result<Vec<CoreEvent>> {
    changelog::lock_modseq(tx, from).await?;
    let ms = changelog::bump_modseq(tx, into).await?;
    // Папки `from`, предки раньше потомков.
    let folders: Vec<(Uuid, Option<Uuid>, String, Option<String>)> = sqlx::query_as(
        "WITH RECURSIVE sub(id, parent, name, role, depth) AS (
             SELECT mailbox_id, parent_id, name, role, 0 FROM mailboxes
             WHERE tenant_id = $1 AND account_id = $2 AND parent_id IS NULL
             UNION ALL
             SELECT m.mailbox_id, m.parent_id, m.name, m.role, sub.depth + 1 FROM mailboxes m JOIN sub ON m.parent_id = sub.id
             WHERE m.tenant_id = $1 AND m.account_id = $2)
         SELECT id, parent, name, role FROM sub ORDER BY depth, name, id",
    )
    .bind(from.tenant.as_uuid())
    .bind(from.account.as_uuid())
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    let mut changes = Vec::new();
    let mut events = Vec::new();
    // Папка `from` → папка `into`: системная — с той же ролью; прочие (режим `Keep`) — новые с иерархией.
    let mut map: HashMap<Uuid, Uuid> = HashMap::new();
    for (id, parent, name, role) in &folders {
        let target = match role {
            Some(role) => sqlx::query_scalar(
                "SELECT mailbox_id FROM mailboxes WHERE tenant_id = $1 AND account_id = $2 AND role = $3",
            )
            .bind(into.tenant.as_uuid())
            .bind(into.account.as_uuid())
            .bind(role)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?,
            None if mode == ExistingContent::Keep => {
                let parent = match parent {
                    Some(p) => Some(*map.get(p).ok_or_else(|| Error::internal("папка без родителя"))?),
                    None => None,
                };
                let name = free_name(tx, into, parent, name).await?;
                let data = NewMailbox {
                    parent: parent.map(MailboxId::from_uuid),
                    name,
                    role: None,
                };
                let mb = mailboxes::insert(tx, into, &data, ms).await?;
                changes.push(Change::mailbox(mb.id, Op::Created));
                events.push(changelog::event(into, Some(mb.id), CoreEventKind::MailboxCreated));
                Some(*mb.id.as_uuid())
            }
            None => None,
        };
        if let Some(t) = target {
            map.insert(*id, t);
        }
    }
    let trash_box = mailboxes::by_role(tx, into, tp_contracts::meta::SpecialUse::Trash)
        .await?
        .ok_or(Error::NotFound)?;
    let inbox = mailboxes::by_role(tx, into, tp_contracts::meta::SpecialUse::Inbox)
        .await?
        .ok_or(Error::NotFound)?;
    // Письма по папкам и UID; папка назначения — по режиму.
    let msgs: Vec<(Uuid, Uuid)> = sqlx::query_as(
        "SELECT message_id, mailbox_id FROM messages WHERE tenant_id = $1 AND account_id = $2
         ORDER BY mailbox_id, uid",
    )
    .bind(from.tenant.as_uuid())
    .bind(from.account.as_uuid())
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    let target_of = |mb: &Uuid| match mode {
        ExistingContent::Keep => map.get(mb).copied().unwrap_or(*inbox.as_uuid()),
        ExistingContent::Delete => *trash_box.as_uuid(),
    };
    let mut groups: Vec<(Uuid, Vec<Uuid>)> = Vec::new();
    for (id, mb) in &msgs {
        let t = target_of(mb);
        match groups.iter_mut().find(|(g, _)| *g == t) {
            Some((_, v)) => v.push(*id),
            None => groups.push((t, vec![*id])),
        }
    }
    let (mut ids, mut targets, mut uids) = (Vec::new(), Vec::new(), Vec::new());
    for (target, group) in &groups {
        let first =
            mailboxes::alloc_uids(tx, into, MailboxId::from_uuid(*target), group.len(), ms).await?;
        for (i, id) in group.iter().enumerate() {
            let uid = i64::from(first.0) + i64::try_from(i).map_err(|_| Error::internal("UID"))?;
            ids.push(*id);
            targets.push(*target);
            uids.push(uid);
            let message = MessageId::from_uuid(*id);
            let mailbox = MailboxId::from_uuid(*target);
            changes.push(Change::message(
                *id,
                Op::Created,
                mailbox,
                tp_contracts::Uid(to_u32(uid)?),
            ));
            events.push(changelog::event(
                into,
                Some(mailbox),
                CoreEventKind::Delivered { message },
            ));
        }
        changes.push(Change::mailbox(MailboxId::from_uuid(*target), Op::Updated));
    }
    // Исходная папка для восстановления: `Keep` — перенесённая папка, `Delete` — системная с той же ролью.
    let keep = mode == ExistingContent::Keep;
    let mapped: Vec<(Uuid, Uuid)> = folders
        .iter()
        .filter(|(_, _, _, role)| keep || role.is_some())
        .filter_map(|(id, ..)| map.get(id).map(|t| (*id, *t)))
        .collect();
    let default_us = trash.map(|t| micros(t.default)).transpose()?.unwrap_or(0);
    sqlx::query(
        "WITH d AS (SELECT * FROM unnest($3::uuid[], $4::uuid[], $5::int8[]) AS d(id, target, uid)),
              f AS (SELECT * FROM unnest($6::uuid[], $7::uuid[]) AS f(old, new))
         INSERT INTO messages (tenant_id, account_id, message_id, mailbox_id, uid, modseq, size, received_at, flags,
                               skeleton_hash, skeleton_gen, thread_key, meta, trashed_from, trashed_at,
                               trash_expires_at, hidden_at)
         SELECT m.tenant_id, $8, m.message_id, d.target, d.uid, $9, m.size, m.received_at,
                CASE WHEN $10 AND m.trashed_at IS NULL THEN array_remove(m.flags, $12) ELSE m.flags END,
                m.skeleton_hash, m.skeleton_gen, m.thread_key, m.meta,
                (SELECT f.new FROM f WHERE f.old = CASE WHEN m.trashed_at IS NULL AND $10 THEN m.mailbox_id
                                                       ELSE m.trashed_from END),
                CASE WHEN $10 THEN coalesce(m.trashed_at, now()) ELSE m.trashed_at END,
                CASE WHEN $10 AND m.trashed_at IS NULL THEN now() + coalesce(
                         tp_trash_retention_us(m.tenant_id, $8, NULL), $11) * interval '1 microsecond'
                     ELSE m.trash_expires_at END,
                m.hidden_at
         FROM d JOIN messages m ON m.tenant_id = $1 AND m.account_id = $2 AND m.message_id = d.id",
    )
    .bind(from.tenant.as_uuid())
    .bind(from.account.as_uuid())
    .bind(&ids)
    .bind(&targets)
    .bind(&uids)
    .bind(mapped.iter().map(|(o, _)| *o).collect::<Vec<_>>())
    .bind(mapped.iter().map(|(_, n)| *n).collect::<Vec<_>>())
    .bind(into.account.as_uuid())
    .bind(to_i64(ms.0)?)
    .bind(!keep)
    .bind(default_us)
    .bind(r"\Deleted")
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    sqlx::query(
        "INSERT INTO message_parts (tenant_id, account_id, message_id, part, blob_hash, blob_gen, size)
         SELECT tenant_id, $3, message_id, part, blob_hash, blob_gen, size FROM message_parts
         WHERE tenant_id = $1 AND account_id = $2",
    )
    .bind(from.tenant.as_uuid())
    .bind(from.account.as_uuid())
    .bind(into.account.as_uuid())
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    sqlx::query("DELETE FROM messages WHERE tenant_id = $1 AND account_id = $2")
        .bind(from.tenant.as_uuid())
        .bind(from.account.as_uuid())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    changelog::log(tx, into, ms, &changes).await?;
    Ok(events)
}

/// Каталог при подключении: `disabled` получает учётные данные `to` (домен, вход, имя, `external_id`, `enabled`),
/// членство `to` в группах и его оргатрибуты и становится действующим; запись `to` удаляется. Состояния
/// проверяются заново под блокировкой: `disabled` действует — `Invalid`, `to` отключена — `Conflict`.
async fn swap_catalog(
    tx: &mut PgConnection,
    tenant: TenantId,
    disabled: AccountId,
    to: AccountId,
    to_folders: &[Uuid],
) -> Result<Account> {
    let mut rows = lock_rows(tx, tenant, &[disabled, to]).await?;
    let (Some(d), Some(t)) = (rows.remove(&disabled), rows.remove(&to)) else {
        return Err(Error::NotFound);
    };
    if d.disabled_until.is_none() {
        return Err(Error::invalid("attach: ящик не отключён"));
    }
    if d.removing || t.disabled_until.is_some() {
        return Err(Error::Conflict);
    }
    let (tu, du, tou) = (tenant.as_uuid(), disabled.as_uuid(), to.as_uuid());
    for sql in [
        "UPDATE group_members SET account_id = $2 WHERE tenant_id = $1 AND account_id = $3",
        "UPDATE group_moderators SET account_id = $2 WHERE tenant_id = $1 AND account_id = $3",
        "UPDATE group_senders SET account_id = $2 WHERE tenant_id = $1 AND account_id = $3",
        // Оргатрибуты — учётной записи каталога: прежние у `disabled` заменяются, если у `to` они есть.
        "DELETE FROM org_attributes WHERE tenant_id = $1 AND account_id = $2
             AND EXISTS (SELECT 1 FROM org_attributes WHERE tenant_id = $1 AND account_id = $3)",
        "UPDATE org_attributes SET account_id = $2, manager_id = NULLIF(manager_id, $2)
             WHERE tenant_id = $1 AND account_id = $3",
        "UPDATE org_attributes SET manager_id = NULLIF($2, account_id) WHERE tenant_id = $1 AND manager_id = $3",
    ] {
        sqlx::query(sql)
            .bind(tu)
            .bind(du)
            .bind(tou)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
    }
    drop_catalog(tx, scope(tenant, to), to_folders).await?;
    let ident = t.account;
    let row: AccountRow = sqlx::query_as(
        "UPDATE accounts SET domain_id = $3, login = $4, display_name = $5, external_id = $6, enabled = $7,
                             aliases = $8, kind = $9, disabled_until = NULL, updated_at = now()
         WHERE tenant_id = $1 AND account_id = $2
         RETURNING account_id, tenant_id, domain_id, login, display_name, external_id, enabled, aliases, kind",
    )
    .bind(tu)
    .bind(du)
    .bind(ident.domain_id)
    .bind(&ident.login)
    .bind(&ident.display_name)
    .bind(&ident.external_id)
    .bind(ident.enabled)
    .bind(&ident.aliases)
    .bind(&ident.kind)
    .fetch_one(&mut *tx)
    .await
    .map_err(db_err)?;
    Account::try_from(row)
}

impl PgMeta {
    pub(crate) async fn account_state_of(
        &self,
        tenant: TenantId,
        id: AccountId,
    ) -> Result<AccountState> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let row: Option<(Option<OffsetDateTime>, bool)> = sqlx::query_as(
                "SELECT disabled_until, removing FROM accounts WHERE tenant_id = $1 AND account_id = $2",
            )
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            row.map(state).ok_or(Error::NotFound)
        })
        .await
    }

    /// Задаёт срок хранения (`Some` — отключить, `None` — восстановить) под блокировкой строки каталога; запись по
    /// значению — повторяется. Удаляемый ящик — `Conflict`. Возвращает, был ли ящик отключён до записи.
    pub(crate) async fn set_disabled(
        &self,
        tenant: TenantId,
        id: AccountId,
        until: Option<OffsetDateTime>,
    ) -> Result<bool> {
        self.rewrite(|| async move {
            let mut tx = begin(&self.pool, tenant).await?;
            let row: Option<(Option<OffsetDateTime>, bool)> = sqlx::query_as(
                "SELECT disabled_until, removing FROM accounts WHERE tenant_id = $1 AND account_id = $2
                 FOR UPDATE",
            )
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            let (was, removing) = row.ok_or(Error::NotFound)?;
            if removing {
                return Err(Error::Conflict);
            }
            sqlx::query(
                "UPDATE accounts SET disabled_until = $3, updated_at = now()
                 WHERE tenant_id = $1 AND account_id = $2",
            )
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .bind(until)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(was.is_some())
        })
        .await
    }

    /// Отметка «удаляется» (CTR-78) одним `UPDATE` с условием на состояние: отключённый со сроком не позже `now`
    /// или уже удаляемый.
    pub(crate) async fn mark_removing(
        &self,
        tenant: TenantId,
        id: AccountId,
        now: OffsetDateTime,
    ) -> Result<()> {
        self.rewrite(|| async move {
            let mut tx = begin(&self.pool, tenant).await?;
            let n = sqlx::query(
                "UPDATE accounts SET removing = true, updated_at = now()
                 WHERE tenant_id = $1 AND account_id = $2
                   AND (removing OR (disabled_until IS NOT NULL AND disabled_until <= $3))",
            )
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .bind(now)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?
            .rows_affected();
            if n == 1 {
                crate::commits::commit(tx).await?;
                return Ok(());
            }
            let exists: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM accounts WHERE tenant_id = $1 AND account_id = $2)",
            )
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Err(if exists {
                Error::Conflict
            } else {
                Error::NotFound
            })
        })
        .await
    }

    pub(crate) async fn disabled_of(&self, tenant: TenantId) -> Result<Vec<DisabledAccount>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let rows: Vec<StateRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT {STATE_COLUMNS} FROM accounts
                 WHERE tenant_id = $1 AND disabled_until IS NOT NULL ORDER BY disabled_until, account_id"
            )))
            .bind(tenant.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            rows.into_iter()
                .filter_map(|r| {
                    r.disabled_until.map(|until| {
                        Ok(DisabledAccount {
                            account: Account::try_from(r.account)?,
                            until,
                        })
                    })
                })
                .collect()
        })
        .await
    }

    pub(crate) async fn expired(
        &self,
        now: OffsetDateTime,
        limit: usize,
    ) -> Result<Vec<AccountScope>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.idempotent(|| async move {
            let mut tx = begin_system(&self.pool).await?;
            let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(
                "SELECT tenant_id, account_id FROM accounts
                 WHERE disabled_until IS NOT NULL AND disabled_until <= $1
                 ORDER BY disabled_until, account_id LIMIT $2",
            )
            .bind(now)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(rows
                .into_iter()
                .map(|(t, a)| scope(TenantId::from_uuid(t), AccountId::from_uuid(a)))
                .collect())
        })
        .await
    }

    /// Состояние учётной записи в каталоге: `None` — нет записи.
    async fn catalog_state(&self, tenant: TenantId, id: AccountId) -> Result<Option<AccountState>> {
        let mut tx = begin(&self.pool, tenant).await?;
        let row: Option<(Option<OffsetDateTime>, bool)> = sqlx::query_as(
            "SELECT disabled_until, removing FROM accounts WHERE tenant_id = $1 AND account_id = $2",
        )
        .bind(tenant.as_uuid())
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        crate::commits::commit(tx).await?;
        Ok(row.map(state))
    }

    pub(crate) async fn remove_disabled_box(&self, tenant: TenantId, id: AccountId) -> Result<()> {
        self.once(async move {
            match self.catalog_state(tenant, id).await? {
                None => return Err(Error::NotFound),
                Some(AccountState::Removing { .. }) => {}
                Some(_) => return Err(Error::invalid(NOT_REMOVING)),
            }
            let s = scope(tenant, id);
            let db = self.box_db(s).await?;
            let mut tx = begin(&db.pool, tenant).await?;
            if has_messages(&mut tx, s).await? {
                return Err(Error::Conflict);
            }
            let folders = drop_box(&mut tx, s).await?;
            if db.shard != ShardId::MAIN {
                crate::commits::commit(tx).await?;
                tx = begin(&self.pool, tenant).await?;
            }
            let rows = lock_rows(&mut tx, tenant, &[id]).await?;
            match rows.get(&id) {
                None => return Err(Error::NotFound),
                Some(r) if !r.removing => return Err(Error::invalid(NOT_REMOVING)),
                Some(_) => {}
            }
            drop_catalog(&mut tx, s, &folders).await?;
            crate::commits::commit(tx).await?;
            Ok(())
        })
        .await
    }

    pub(crate) async fn attach(
        &self,
        tenant: TenantId,
        disabled: AccountId,
        to: AccountId,
        existing: Option<ExistingContent>,
    ) -> Result<Account> {
        self.once(async move {
            if disabled == to {
                return Err(Error::invalid("attach: ящик подключается к самому себе"));
            }
            let (d_state, t_state) = (
                self.catalog_state(tenant, disabled).await?,
                self.catalog_state(tenant, to).await?,
            );
            let (Some(d_state), Some(t_state)) = (d_state, t_state) else {
                return Err(Error::NotFound);
            };
            match d_state {
                AccountState::Active => return Err(Error::invalid("attach: ящик не отключён")),
                AccountState::Removing { .. } => return Err(Error::Conflict),
                AccountState::Disabled { .. } => {}
            }
            if t_state != AccountState::Active {
                return Err(Error::Conflict);
            }
            let (sd, st) = (scope(tenant, disabled), scope(tenant, to));
            let (dbd, dbt) = (self.box_db(sd).await?, self.box_db(st).await?);
            let mut tx = begin(&dbt.pool, tenant).await?;
            let busy = has_messages(&mut tx, st).await?;
            crate::commits::commit(tx).await?;
            let mode = match (busy, existing) {
                (false, _) => None,
                (true, None) => {
                    return Err(Error::invalid(
                        "attach: в ящике учётной записи есть письма — выберите режим: keep (перенести в \
                         подключаемый ящик) или delete (в корзину подключаемого ящика)",
                    ));
                }
                (true, Some(_)) if dbd.shard != dbt.shard => {
                    return Err(Error::invalid(
                        "attach: ящики на разных шардах — перенос писем между шардами не поддерживается",
                    ));
                }
                (true, Some(mode)) => Some(mode),
            };
            let trash = match mode {
                Some(ExistingContent::Delete) => {
                    Some(self.trash_retention(&dbd, sd, || async { Ok(Vec::new()) }).await?)
                }
                _ => None,
            };
            let all_main = dbd.shard == ShardId::MAIN && dbt.shard == ShardId::MAIN;
            // Ящики вне основной БД — сначала их транзакция: каталог не держим открытым во время работы с шардом.
            let folders = match all_main {
                true => None,
                false => Some(self.move_box_at(&dbd, &dbt, sd, st, mode, trash.clone()).await?),
            };
            let mut tx = begin(&self.pool, tenant).await?;
            let folders = match folders {
                Some(f) => f,
                None => self.move_box(&mut tx, sd, st, mode, trash.as_ref()).await?,
            };
            let account = swap_catalog(&mut tx, tenant, disabled, to, &folders).await?;
            crate::commits::commit(tx).await?;
            if mode.is_some() {
                self.signal_log([disabled]).await;
            }
            Ok(account)
        })
        .await
    }

    /// Письма `st` — в `sd` по режиму, данные ящика `st` удаляются; одна БД ящиков (`tx`). Возвращает папки `st`.
    async fn move_box(
        &self,
        tx: &mut PgConnection,
        sd: AccountScope,
        st: AccountScope,
        mode: Option<ExistingContent>,
        trash: Option<&TrashFor>,
    ) -> Result<Vec<Uuid>> {
        if let Some(mode) = mode {
            mailboxes::ensure_system(tx, sd, self.cfg.event_partitions).await?;
            let events = absorb(tx, sd, st, mode, trash).await?;
            changelog::append_events(tx, self.cfg.event_partitions, &events).await?;
        }
        drop_box(tx, st).await
    }

    /// [`move_box`](Self::move_box) для ящиков вне основной БД: перенос писем — только в одной БД ящиков
    /// (разные шарды — без писем, проверено вызывающим).
    async fn move_box_at(
        &self,
        dbd: &BoxDb,
        dbt: &BoxDb,
        sd: AccountScope,
        st: AccountScope,
        mode: Option<ExistingContent>,
        trash: Option<TrashFor>,
    ) -> Result<Vec<Uuid>> {
        if dbd.shard == dbt.shard {
            let mut tx = begin(&dbt.pool, st.tenant).await?;
            let out = self.move_box(&mut tx, sd, st, mode, trash.as_ref()).await?;
            crate::commits::commit(tx).await?;
            return Ok(out);
        }
        let mut tx = begin(&dbt.pool, st.tenant).await?;
        if has_messages(&mut tx, st).await? {
            return Err(Error::Conflict);
        }
        let folders = drop_box(&mut tx, st).await?;
        crate::commits::commit(tx).await?;
        Ok(folders)
    }
}
