//! Папки учётной записи на PostgreSQL (CONTRACTS.md#Metadata).
//!
//! UIDVALIDITY выдаётся из строки учётной записи: `max(последний + 1, unix-время)` — строго растёт,
//! поэтому пересозданная папка с тем же именем получает новое значение. UIDNEXT и наибольший `ModSeq`
//! папки хранятся в её строке.

use std::time::Duration;

use async_trait::async_trait;
use sqlx::{FromRow, PgConnection};
use tp_contracts::events::CoreEventKind;
use tp_contracts::meta::{
    AccountScope, Mailbox, MailboxStatus, MailboxStore, NewMailbox, SpecialUse,
};
use tp_contracts::{Error, MailboxId, ModSeq, Result, Uid, UidValidity};
use uuid::Uuid;

use crate::PgMeta;
use crate::changelog::{self, Change, Op};
use crate::retention::micros;
use crate::schema::{begin, db_err, to_i64, to_u32, to_u64};

/// Системные папки учётной записи (RFC 6154 и `INBOX`).
pub(crate) const SYSTEM_FOLDERS: [(&str, SpecialUse); 6] = [
    ("INBOX", SpecialUse::Inbox),
    ("Drafts", SpecialUse::Drafts),
    ("Sent", SpecialUse::Sent),
    ("Junk", SpecialUse::Junk),
    ("Trash", SpecialUse::Trash),
    ("Archive", SpecialUse::Archive),
];

/// Системные папки, которых недостаёт: нет папки с той же ролью и нет папки верхнего уровня
/// с тем же именем (без учёта регистра — `INBOX` по RFC 3501, остальные — чтобы не плодить «sent» и «Sent»).
/// `existing` — роль, родитель и имя каждой папки.
pub(crate) fn missing_system<'a>(
    existing: impl IntoIterator<Item = (Option<SpecialUse>, bool, &'a str)> + Clone,
) -> Vec<(&'static str, SpecialUse)> {
    SYSTEM_FOLDERS
        .into_iter()
        .filter(|(name, role)| {
            !existing
                .clone()
                .into_iter()
                .any(|(r, top, n)| r == Some(*role) || (top && n.eq_ignore_ascii_case(name)))
        })
        .collect()
}

fn missing_rows(rows: &[MailboxRow]) -> Result<Vec<(&'static str, SpecialUse)>> {
    let existing = rows
        .iter()
        .map(|r| {
            Ok((
                r.role.as_deref().map(parse_role).transpose()?,
                r.parent_id.is_none(),
                r.name.as_str(),
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(missing_system(existing.iter().copied()))
}

async fn select_all(tx: &mut PgConnection, scope: AccountScope) -> Result<Vec<MailboxRow>> {
    sqlx::query_as(
        "SELECT mailbox_id, parent_id, name, role, uid_validity, subscribed FROM mailboxes
         WHERE tenant_id = $1 AND account_id = $2 ORDER BY created_at, name",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)
}

/// Дополняет учётную запись недостающими системными папками (новая запись, старая запись без части папок).
/// Строка учётной записи блокируется: параллельные вызовы не создают папки дважды.
/// Возвращает число созданных папок; учётной записи нет — 0.
pub(crate) async fn ensure_system(
    tx: &mut PgConnection,
    scope: AccountScope,
    event_partitions: u32,
) -> Result<usize> {
    let locked: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM accounts WHERE tenant_id = $1 AND account_id = $2 FOR UPDATE",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    if locked.is_none() {
        return Ok(0);
    }
    let missing = missing_rows(&select_all(tx, scope).await?)?;
    if missing.is_empty() {
        return Ok(0);
    }
    let ms = changelog::bump_modseq(tx, scope).await?;
    let mut changes = Vec::with_capacity(missing.len());
    let mut events = Vec::with_capacity(missing.len());
    for (name, role) in &missing {
        let data = NewMailbox {
            parent: None,
            name: (*name).to_owned(),
            role: Some(*role),
        };
        let mb = insert(tx, scope, &data, ms).await?;
        changes.push(Change::mailbox(mb.id, Op::Created));
        events.push(changelog::event(
            scope,
            Some(mb.id),
            CoreEventKind::MailboxCreated,
        ));
    }
    changelog::log(tx, scope, ms, &changes).await?;
    changelog::append_events(tx, event_partitions, &events).await?;
    Ok(missing.len())
}

pub(crate) fn role_str(role: SpecialUse) -> &'static str {
    match role {
        SpecialUse::Inbox => "inbox",
        SpecialUse::Archive => "archive",
        SpecialUse::Drafts => "drafts",
        SpecialUse::Junk => "junk",
        SpecialUse::Sent => "sent",
        SpecialUse::Trash => "trash",
    }
}

fn parse_role(s: &str) -> Result<SpecialUse> {
    Ok(match s {
        "inbox" => SpecialUse::Inbox,
        "archive" => SpecialUse::Archive,
        "drafts" => SpecialUse::Drafts,
        "junk" => SpecialUse::Junk,
        "sent" => SpecialUse::Sent,
        "trash" => SpecialUse::Trash,
        other => return Err(Error::internal(format!("роль папки: {other}"))),
    })
}

#[derive(FromRow)]
struct MailboxRow {
    mailbox_id: Uuid,
    parent_id: Option<Uuid>,
    name: String,
    role: Option<String>,
    uid_validity: i64,
    subscribed: bool,
}

impl MailboxRow {
    fn into_mailbox(self) -> Result<Mailbox> {
        Ok(Mailbox {
            id: MailboxId::from_uuid(self.mailbox_id),
            parent: self.parent_id.map(MailboxId::from_uuid),
            name: self.name,
            role: self.role.as_deref().map(parse_role).transpose()?,
            uid_validity: UidValidity(to_u32(self.uid_validity)?),
            subscribed: self.subscribed,
        })
    }
}

fn check_name(name: &str) -> Result<()> {
    if name.is_empty() {
        return Err(Error::invalid("пустое имя папки"));
    }
    Ok(())
}

pub(crate) async fn fetch(
    tx: &mut PgConnection,
    scope: AccountScope,
    id: MailboxId,
    lock: bool,
) -> Result<Option<Mailbox>> {
    let sql = if lock {
        "SELECT mailbox_id, parent_id, name, role, uid_validity, subscribed FROM mailboxes
         WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = $3 FOR UPDATE"
    } else {
        "SELECT mailbox_id, parent_id, name, role, uid_validity, subscribed FROM mailboxes
         WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = $3"
    };
    let row: Option<MailboxRow> = sqlx::query_as(sql)
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
    row.map(MailboxRow::into_mailbox).transpose()
}

/// Папка с назначением `role`.
pub(crate) async fn by_role(
    tx: &mut PgConnection,
    scope: AccountScope,
    role: SpecialUse,
) -> Result<Option<MailboxId>> {
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT mailbox_id FROM mailboxes WHERE tenant_id = $1 AND account_id = $2 AND role = $3",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(role_str(role))
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(id.map(MailboxId::from_uuid))
}

/// Создаёт папку с изменением `modseq` (изменение и событие записывает вызывающий).
pub(crate) async fn insert(
    tx: &mut PgConnection,
    scope: AccountScope,
    data: &NewMailbox,
    modseq: ModSeq,
) -> Result<Mailbox> {
    check_name(&data.name)?;
    let uid_validity: i64 = sqlx::query_scalar(
        "UPDATE accounts SET uid_validity_last = greatest(uid_validity_last + 1, extract(epoch FROM now())::bigint)
         WHERE tenant_id = $1 AND account_id = $2 RETURNING uid_validity_last",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?
    .ok_or(Error::NotFound)?;
    let id = MailboxId::new();
    sqlx::query(
        "INSERT INTO mailboxes (tenant_id, account_id, mailbox_id, parent_id, name, role, uid_validity, highest_modseq)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(id.as_uuid())
    .bind(data.parent.map(|p| *p.as_uuid()))
    .bind(&data.name)
    .bind(data.role.map(role_str))
    .bind(uid_validity)
    .bind(to_i64(modseq.0)?)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(Mailbox {
        id,
        parent: data.parent,
        name: data.name.clone(),
        role: data.role,
        uid_validity: UidValidity(to_u32(uid_validity)?),
        subscribed: true,
    })
}

/// Выделяет `n` UID в папке `to` (её `ModSeq` — `modseq`); возвращает первый.
pub(crate) async fn alloc_uids(
    tx: &mut PgConnection,
    scope: AccountScope,
    to: MailboxId,
    n: usize,
    modseq: ModSeq,
) -> Result<Uid> {
    let n = i64::try_from(n).map_err(|_| Error::invalid("слишком много писем"))?;
    let first: i64 = sqlx::query_scalar(
        "UPDATE mailboxes SET uid_next = uid_next + $4, highest_modseq = $5
         WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = $3 RETURNING uid_next - $4",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(to.as_uuid())
    .bind(n)
    .bind(to_i64(modseq.0)?)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?
    .ok_or(Error::NotFound)?;
    if first + n - 1 > i64::from(u32::MAX) {
        return Err(Error::invalid("исчерпаны UID папки"));
    }
    Ok(Uid(to_u32(first)?))
}

/// Письмо, перенесённое [`relocate`] с тем же идентификатором.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Relocated {
    pub id: Uuid,
    pub from: MailboxId,
    pub from_uid: Uid,
    pub uid: Uid,
}

/// Переносит письма `ids` (в этом порядке) в папку `to` с сохранением идентификаторов и новыми UID;
/// `trash` — перенос в корзину: запомнить исходную папку для восстановления и срок окончательного удаления
/// (политика папки по цепочке, иначе `trash` — срок по умолчанию конфигурации; CTR-07).
/// Записывает изменения `Moved` и изменения папок.
/// Отсутствующие письма пропускаются (в UID папки остаётся пропуск).
pub(crate) async fn relocate(
    tx: &mut PgConnection,
    scope: AccountScope,
    modseq: ModSeq,
    ids: &[Uuid],
    to: MailboxId,
    trash: Option<Duration>,
) -> Result<Vec<Relocated>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let first = alloc_uids(tx, scope, to, ids.len(), modseq).await?;
    let ms = to_i64(modseq.0)?;
    // Подзапрос `old` видит строки до UPDATE: прежние папка и UID.
    let rows: Vec<(Uuid, Uuid, i64, i64)> = sqlx::query_as(
        "WITH src AS (SELECT id, ord FROM unnest($3::uuid[]) WITH ORDINALITY AS s(id, ord)),
              old AS (SELECT m.message_id, m.mailbox_id, m.uid FROM messages m JOIN src ON m.message_id = src.id
                      WHERE m.tenant_id = $1 AND m.account_id = $2)
         UPDATE messages m SET mailbox_id = $4, uid = $5 + src.ord - 1, modseq = $6,
                               trashed_from = CASE WHEN $7 THEN old.mailbox_id END,
                               trashed_at = CASE WHEN $7 THEN now() END, hidden_at = NULL,
                               trash_expires_at = CASE WHEN $7 THEN now() + coalesce(
                                   tp_trash_retention_us(m.tenant_id, m.account_id, old.mailbox_id), $8
                               ) * interval '1 microsecond' END
         FROM src, old
         WHERE m.tenant_id = $1 AND m.account_id = $2 AND m.message_id = src.id AND old.message_id = src.id
         RETURNING m.message_id, old.mailbox_id, old.uid, m.uid",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(ids)
    .bind(to.as_uuid())
    .bind(i64::from(first.0))
    .bind(ms)
    .bind(trash.is_some())
    .bind(trash.map(micros).transpose()?.unwrap_or(0))
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    let moved = rows
        .into_iter()
        .map(|(id, from, from_uid, uid)| {
            Ok(Relocated {
                id,
                from: MailboxId::from_uuid(from),
                from_uid: Uid(to_u32(from_uid)?),
                uid: Uid(to_u32(uid)?),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let mut sources: Vec<Uuid> = moved.iter().map(|m| *m.from.as_uuid()).collect();
    sources.sort_unstable();
    sources.dedup();
    touch(tx, scope, modseq, &sources).await?;
    let mut changes: Vec<Change> = moved
        .iter()
        .map(|m| Change {
            from: Some((m.from, m.from_uid)),
            ..Change::message(m.id, Op::Moved, to, m.uid)
        })
        .collect();
    changes.push(Change::mailbox(to, Op::Updated));
    changes.extend(
        sources
            .iter()
            .map(|s| Change::mailbox(MailboxId::from_uuid(*s), Op::Updated)),
    );
    changelog::log(tx, scope, modseq, &changes).await?;
    Ok(moved)
}

/// Отмечает изменение папок `mailboxes` номером `modseq` (наибольший `ModSeq` папки).
pub(crate) async fn touch(
    tx: &mut PgConnection,
    scope: AccountScope,
    modseq: ModSeq,
    mailboxes: &[Uuid],
) -> Result<()> {
    sqlx::query(
        "UPDATE mailboxes SET highest_modseq = $4
         WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = ANY($3)",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(mailboxes)
    .bind(to_i64(modseq.0)?)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(())
}

#[async_trait]
impl MailboxStore for PgMeta {
    async fn list(&self, scope: AccountScope) -> Result<Vec<Mailbox>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.pool, scope.tenant).await?;
            let mut rows = select_all(&mut tx, scope).await?;
            // Первое открытие ящика без части системных папок (запись заведена в базе, вход без `upsert`).
            let created = !missing_rows(&rows)?.is_empty()
                && ensure_system(&mut tx, scope, self.cfg.event_partitions).await? > 0;
            if created {
                rows = select_all(&mut tx, scope).await?;
            }
            tx.commit().await.map_err(db_err)?;
            if created {
                self.signal_log([scope.account]).await;
            }
            rows.into_iter().map(MailboxRow::into_mailbox).collect()
        })
        .await
    }

    async fn get(&self, scope: AccountScope, id: MailboxId) -> Result<Option<Mailbox>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.pool, scope.tenant).await?;
            let mb = fetch(&mut tx, scope, id, false).await?;
            tx.commit().await.map_err(db_err)?;
            Ok(mb)
        })
        .await
    }

    async fn create(&self, scope: AccountScope, data: NewMailbox) -> Result<Mailbox> {
        self.once(async move {
            check_name(&data.name)?;
            let mut tx = begin(&self.pool, scope.tenant).await?;
            let ms = changelog::bump_modseq(&mut tx, scope).await?;
            if let Some(parent) = data.parent {
                fetch(&mut tx, scope, parent, false)
                    .await?
                    .ok_or(Error::NotFound)?;
            }
            let mb = insert(&mut tx, scope, &data, ms).await?;
            changelog::log(&mut tx, scope, ms, &[Change::mailbox(mb.id, Op::Created)]).await?;
            let ev = changelog::event(scope, Some(mb.id), CoreEventKind::MailboxCreated);
            changelog::append_events(&mut tx, self.cfg.event_partitions, &[ev]).await?;
            tx.commit().await.map_err(db_err)?;
            self.signal_log([scope.account]).await;
            Ok(mb)
        })
        .await
    }

    async fn rename(
        &self,
        scope: AccountScope,
        id: MailboxId,
        parent: Option<MailboxId>,
        name: &str,
    ) -> Result<()> {
        self.once(async move {
        check_name(name)?;
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let ms = changelog::bump_modseq(&mut tx, scope).await?;
        let mb = fetch(&mut tx, scope, id, true)
            .await?
            .ok_or(Error::NotFound)?;
        if mb.role == Some(SpecialUse::Inbox) {
            return Err(Error::Forbidden);
        }
        if let Some(p) = parent {
            fetch(&mut tx, scope, p, false)
                .await?
                .ok_or(Error::NotFound)?;
            // Новый родитель не может быть самой папкой или её потомком.
            let cycle: bool = sqlx::query_scalar(
                "WITH RECURSIVE anc(id, parent) AS (
                     SELECT mailbox_id, parent_id FROM mailboxes
                     WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = $3
                     UNION
                     SELECT m.mailbox_id, m.parent_id FROM mailboxes m JOIN anc ON m.mailbox_id = anc.parent
                     WHERE m.tenant_id = $1 AND m.account_id = $2)
                 SELECT EXISTS (SELECT 1 FROM anc WHERE id = $4)",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(p.as_uuid())
            .bind(id.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            if cycle {
                return Err(Error::invalid("папка не может быть вложена в себя"));
            }
        }
        sqlx::query(
            "UPDATE mailboxes SET parent_id = $4, name = $5, highest_modseq = $6
             WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = $3",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(id.as_uuid())
        .bind(parent.map(|p| *p.as_uuid()))
        .bind(name)
        .bind(to_i64(ms.0)?)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        changelog::log(&mut tx, scope, ms, &[Change::mailbox(id, Op::Updated)]).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(())
}).await
    }

    async fn delete(&self, scope: AccountScope, id: MailboxId, cascade: bool) -> Result<()> {
        self.once(async move {
        let mut tx = begin(&self.pool, scope.tenant).await?;
        // Строка учётной записи заблокирована: иерархия папок не меняется до конца транзакции.
        let ms = changelog::bump_modseq(&mut tx, scope).await?;
        // Папка и все вложенные, предки раньше потомков.
        let subtree: Vec<(Uuid, Option<String>)> = sqlx::query_as(
            "WITH RECURSIVE sub(id, role, depth) AS (
                 SELECT mailbox_id, role, 0 FROM mailboxes
                 WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = $3
                 UNION ALL
                 SELECT m.mailbox_id, m.role, sub.depth + 1 FROM mailboxes m JOIN sub ON m.parent_id = sub.id
                 WHERE m.tenant_id = $1 AND m.account_id = $2)
             SELECT id, role FROM sub ORDER BY depth, id",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(id.as_uuid())
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        if subtree.is_empty() {
            return Err(Error::NotFound);
        }
        if subtree.iter().any(|(_, role)| role.is_some()) {
            return Err(Error::Forbidden);
        }
        // Вложенные без подтверждения: решение принимает пользователь в клиенте.
        if subtree.len() > 1 && !cascade {
            return Err(Error::Conflict);
        }
        let boxes: Vec<Uuid> = subtree.iter().map(|(b, _)| *b).collect();
        // Письма — в корзину (SRS 4.7); восстановление без исходной папки — во «Входящие».
        let ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT message_id FROM messages
             WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = ANY($3) AND hidden_at IS NULL
             ORDER BY array_position($3, mailbox_id), uid",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(&boxes)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        let mut events = Vec::with_capacity(ids.len() + boxes.len());
        if !ids.is_empty() {
            let trash = by_role(&mut tx, scope, SpecialUse::Trash)
                .await?
                .ok_or(Error::NotFound)?;
            for m in relocate(
                &mut tx,
                scope,
                ms,
                &ids,
                trash,
                Some(self.cfg.retention.trash_retention),
            )
            .await?
            {
                let message = tp_contracts::MessageId::from_uuid(m.id);
                events.push(changelog::event(
                    scope,
                    Some(trash),
                    CoreEventKind::Trashed { message },
                ));
            }
        }
        sqlx::query(
            "DELETE FROM mailboxes WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = ANY($3)",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(&boxes)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        // Потомки раньше предков.
        let destroyed: Vec<MailboxId> = boxes
            .iter()
            .rev()
            .map(|b| MailboxId::from_uuid(*b))
            .collect();
        let changes: Vec<Change> = destroyed
            .iter()
            .map(|b| Change::mailbox(*b, Op::Destroyed))
            .collect();
        changelog::log(&mut tx, scope, ms, &changes).await?;
        events.extend(
            destroyed
                .iter()
                .map(|b| changelog::event(scope, Some(*b), CoreEventKind::MailboxDeleted)),
        );
        changelog::append_events(&mut tx, self.cfg.event_partitions, &events).await?;
        tx.commit().await.map_err(db_err)?;
        self.signal_log([scope.account]).await;
        Ok(())
}).await
    }

    async fn set_subscribed(&self, scope: AccountScope, id: MailboxId, on: bool) -> Result<()> {
        self.rewrite(|| async move {
            let mut tx = begin(&self.pool, scope.tenant).await?;
            let ms = changelog::bump_modseq(&mut tx, scope).await?;
            let updated = sqlx::query(
                "UPDATE mailboxes SET subscribed = $4, highest_modseq = $5
             WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = $3",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(id.as_uuid())
            .bind(on)
            .bind(to_i64(ms.0)?)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?
            .rows_affected();
            if updated == 0 {
                return Err(Error::NotFound);
            }
            changelog::log(&mut tx, scope, ms, &[Change::mailbox(id, Op::Updated)]).await?;
            tx.commit().await.map_err(db_err)?;
            Ok(())
        })
        .await
    }

    async fn status(&self, scope: AccountScope, id: MailboxId) -> Result<MailboxStatus> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.pool, scope.tenant).await?;
            let row: Option<(i64, i64, i64, i64, i64, i64)> = sqlx::query_as(
            r"SELECT mb.uid_validity, mb.uid_next, mb.highest_modseq, s.messages, s.unseen, s.size
              FROM mailboxes mb,
                   LATERAL (SELECT count(*) AS messages,
                                   count(*) FILTER (WHERE NOT ('\Seen' = ANY (m.flags))) AS unseen,
                                   coalesce(sum(m.size), 0)::bigint AS size
                            FROM messages m
                            WHERE m.tenant_id = mb.tenant_id AND m.account_id = mb.account_id
                              AND m.mailbox_id = mb.mailbox_id AND m.hidden_at IS NULL) s
              WHERE mb.tenant_id = $1 AND mb.account_id = $2 AND mb.mailbox_id = $3",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(id.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
            tx.commit().await.map_err(db_err)?;
            let (uid_validity, uid_next, highest, messages, unseen, size) =
                row.ok_or(Error::NotFound)?;
            Ok(MailboxStatus {
                messages: to_u32(messages)?,
                unseen: to_u32(unseen)?,
                uid_next: Uid(to_u32(uid_next)?),
                uid_validity: UidValidity(to_u32(uid_validity)?),
                highest_modseq: ModSeq(to_u64(highest)?),
                size: to_u64(size)?,
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roles(v: &[(&str, SpecialUse)]) -> Vec<SpecialUse> {
        v.iter().map(|(_, r)| *r).collect()
    }

    #[test]
    fn missing_system_all_for_empty() {
        let none: [(Option<SpecialUse>, bool, &str); 0] = [];
        let m = missing_system(none);
        assert_eq!(m.len(), 6);
        assert!(m.contains(&("Archive", SpecialUse::Archive)));
        assert!(m.contains(&("INBOX", SpecialUse::Inbox)));
    }

    #[test]
    fn missing_system_skips_same_role_or_name() {
        let existing = [
            (Some(SpecialUse::Inbox), true, "INBOX"),
            // Роль та же, имя другое — не дублировать.
            (Some(SpecialUse::Sent), true, "Отправленные"),
            // Имя то же (без учёта регистра), роли нет — не дублировать.
            (None, true, "junk"),
            // Вложенная папка с тем же именем не мешает.
            (None, false, "Trash"),
        ];
        assert_eq!(
            roles(&missing_system(existing)),
            vec![SpecialUse::Drafts, SpecialUse::Trash, SpecialUse::Archive]
        );
    }

    #[test]
    fn missing_system_none_when_complete() {
        let existing: Vec<_> = SYSTEM_FOLDERS
            .iter()
            .map(|(n, r)| (Some(*r), true, *n))
            .collect();
        assert!(missing_system(existing).is_empty());
    }
}
