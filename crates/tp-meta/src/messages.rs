//! Письма учётной записи на PostgreSQL (CONTRACTS.md#Metadata).
//!
//! Каждая изменяющая операция — одна транзакция: блокировка строки учётной записи и новый `ModSeq`,
//! изменения строк писем, журнал изменений и события ядра. UID выдаются из `uid_next` строки папки.
//! `to_trash` и `restore` сохраняют `MessageId` (меняются папка и UID), `copy` и `move_to` создают новые.
//! Ссылки на объекты BLOB: `copy` добавляет, `expunge_final` снимает — в той же транзакции.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use sqlx::{FromRow, PgConnection};
use time::OffsetDateTime;
use tp_contracts::events::CoreEventKind;
use tp_contracts::meta::{
    AccountScope, Flag, FlagOp, Flags, FlagsUpdate, Inserted, Message, MessageQuery, MessageStore,
    NewMessage, PartRef, SpecialUse,
};
use tp_contracts::{BlobHash, BlobKey, Error, Gen, MailboxId, MessageId, ModSeq, Result, Uid};
use uuid::Uuid;

use crate::PgMeta;
use crate::changelog::{self, Change, Op};
use crate::mailboxes::{self, alloc_uids, by_role, relocate, touch};
use crate::schema::{begin, db_err, to_i64, to_u32, to_u64};

// ---------- Флаги ----------

fn flag_str(f: &Flag) -> &str {
    match f {
        Flag::Seen => r"\Seen",
        Flag::Answered => r"\Answered",
        Flag::Flagged => r"\Flagged",
        Flag::Deleted => r"\Deleted",
        Flag::Draft => r"\Draft",
        Flag::Keyword(k) => k,
    }
}

fn parse_flag(s: String) -> Flag {
    match s.as_str() {
        r"\Seen" => Flag::Seen,
        r"\Answered" => Flag::Answered,
        r"\Flagged" => Flag::Flagged,
        r"\Deleted" => Flag::Deleted,
        r"\Draft" => Flag::Draft,
        _ => Flag::Keyword(s),
    }
}

/// Ключевое слово: непустое, без пробелов и управляющих символов, не начинается с `\`.
fn check_flags(flags: &Flags) -> Result<()> {
    for f in flags {
        if let Flag::Keyword(k) = f
            && (k.is_empty()
                || k.starts_with('\\')
                || k.chars().any(|c| c.is_whitespace() || c.is_control()))
        {
            return Err(Error::invalid(format!(
                "недопустимое ключевое слово: {k:?}"
            )));
        }
    }
    Ok(())
}

fn flags_vec(flags: &Flags) -> Vec<String> {
    flags.iter().map(|f| flag_str(f).to_owned()).collect()
}

fn apply(op: &FlagOp, old: &Flags) -> Flags {
    match op {
        FlagOp::Add(add) => old.union(add).cloned().collect(),
        FlagOp::Remove(rm) => old.difference(rm).cloned().collect(),
        FlagOp::Replace(new) => new.clone(),
    }
}

// ---------- Чтение ----------

#[derive(FromRow)]
struct MessageRow {
    message_id: Uuid,
    mailbox_id: Uuid,
    uid: i64,
    modseq: i64,
    size: i64,
    received_at: OffsetDateTime,
    flags: Vec<String>,
    skeleton_hash: Vec<u8>,
    skeleton_gen: i64,
    meta: serde_json::Value,
}

#[derive(FromRow)]
struct PartRow {
    message_id: Uuid,
    part: String,
    blob_hash: Vec<u8>,
    blob_gen: i64,
    size: i64,
}

fn blob_key(scope: AccountScope, hash: Vec<u8>, generation: i64) -> Result<BlobKey> {
    let hash: [u8; 32] = hash
        .try_into()
        .map_err(|_| Error::internal("длина хэша объекта письма"))?;
    Ok(BlobKey {
        tenant: scope.tenant,
        hash: BlobHash(hash),
        generation: Gen(to_u32(generation)?),
    })
}

/// Порядок частей MIME: `1` < `1.2` < `2` < `10`.
fn part_order(a: &str, b: &str) -> Ordering {
    let num = |s: &str| {
        s.split('.')
            .map(|x| x.parse::<u32>().unwrap_or(u32::MAX))
            .collect::<Vec<_>>()
    };
    num(a).cmp(&num(b)).then_with(|| a.cmp(b))
}

macro_rules! select_messages {
    ($tail:literal) => {
        concat!(
            "SELECT message_id, mailbox_id, uid, modseq, size, received_at, flags, skeleton_hash, skeleton_gen, meta
             FROM messages WHERE tenant_id = $1 AND account_id = $2 ",
            $tail
        )
    };
}

/// Собирает письма из строк, догружая части.
async fn assemble(
    tx: &mut PgConnection,
    scope: AccountScope,
    rows: Vec<MessageRow>,
) -> Result<Vec<Message>> {
    let ids: Vec<Uuid> = rows.iter().map(|r| r.message_id).collect();
    let parts: Vec<PartRow> = sqlx::query_as(
        "SELECT message_id, part, blob_hash, blob_gen, size FROM message_parts
         WHERE tenant_id = $1 AND account_id = $2 AND message_id = ANY($3)",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(&ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)?;
    let mut by_message: HashMap<Uuid, Vec<PartRef>> = HashMap::new();
    for p in parts {
        by_message.entry(p.message_id).or_default().push(PartRef {
            part: p.part,
            blob: blob_key(scope, p.blob_hash, p.blob_gen)?,
            size: to_u64(p.size)?,
        });
    }
    rows.into_iter()
        .map(|r| {
            let mut parts = by_message.remove(&r.message_id).unwrap_or_default();
            parts.sort_by(|a, b| part_order(&a.part, &b.part));
            Ok(Message {
                id: MessageId::from_uuid(r.message_id),
                mailbox: MailboxId::from_uuid(r.mailbox_id),
                uid: Uid(to_u32(r.uid)?),
                modseq: ModSeq(to_u64(r.modseq)?),
                size: to_u64(r.size)?,
                received_at: r.received_at,
                flags: r.flags.into_iter().map(parse_flag).collect(),
                skeleton: blob_key(scope, r.skeleton_hash, r.skeleton_gen)?,
                parts,
                meta: serde_json::from_value(r.meta).map_err(|e| Error::internal(e.to_string()))?,
            })
        })
        .collect()
}

fn uuids(ids: &[MessageId]) -> Vec<Uuid> {
    ids.iter().map(|i| *i.as_uuid()).collect()
}

/// Существующие письма из `ids` в порядке `ids` (без повторов): идентификатор и папка.
async fn existing(
    tx: &mut PgConnection,
    scope: AccountScope,
    ids: &[Uuid],
) -> Result<Vec<(Uuid, Uuid)>> {
    sqlx::query_as(
        "SELECT m.message_id, m.mailbox_id
         FROM (SELECT id, min(ord) AS ord FROM unnest($3::uuid[]) WITH ORDINALITY AS d(id, ord) GROUP BY id) d
         JOIN messages m ON m.tenant_id = $1 AND m.account_id = $2 AND m.message_id = d.id
         ORDER BY d.ord",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(ids)
    .fetch_all(&mut *tx)
    .await
    .map_err(db_err)
}

/// Ссылки писем `ids` на объекты BLOB изменяются на `delta` (каждое письмо — по ссылке на каждый объект).
async fn adjust_refs(
    tx: &mut PgConnection,
    scope: AccountScope,
    ids: &[Uuid],
    delta: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE blobs b SET refcnt = b.refcnt + c.n * $4,
                            zero_since = CASE WHEN b.refcnt + c.n * $4 = 0 THEN now() ELSE NULL END
         FROM (SELECT hash, gen, count(*) AS n FROM (
                   SELECT m.skeleton_hash AS hash, m.skeleton_gen AS gen
                   FROM unnest($3::uuid[]) AS d(id)
                   JOIN messages m ON m.tenant_id = $1 AND m.account_id = $2 AND m.message_id = d.id
                   UNION ALL
                   SELECT p.blob_hash, p.blob_gen
                   FROM unnest($3::uuid[]) AS d(id)
                   JOIN message_parts p ON p.tenant_id = $1 AND p.account_id = $2 AND p.message_id = d.id) r
               GROUP BY hash, gen) c
         WHERE b.tenant_id = $1 AND b.hash = c.hash AND b.gen = c.gen AND b.state = 'live'",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(ids)
    .bind(delta)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Копирует письма `src` (существующие, по порядку) в папку `to` с новыми идентификаторами и UID.
async fn clone_into(
    tx: &mut PgConnection,
    scope: AccountScope,
    modseq: ModSeq,
    src: &[Uuid],
    to: MailboxId,
) -> Result<Vec<Inserted>> {
    let first = alloc_uids(tx, scope, to, src.len(), modseq).await?;
    let new_ids: Vec<Uuid> = src.iter().map(|_| *MessageId::new().as_uuid()).collect();
    let uids: Vec<i64> = (0..src.len() as i64)
        .map(|i| i64::from(first.0) + i)
        .collect();
    sqlx::query(
        "INSERT INTO messages (tenant_id, account_id, message_id, mailbox_id, uid, modseq, size, received_at,
                               flags, skeleton_hash, skeleton_gen, thread_key, meta)
         SELECT m.tenant_id, m.account_id, d.new_id, $6, d.uid, $7, m.size, m.received_at,
                m.flags, m.skeleton_hash, m.skeleton_gen, m.thread_key, m.meta
         FROM unnest($3::uuid[], $4::uuid[], $5::int8[]) AS d(src, new_id, uid)
         JOIN messages m ON m.tenant_id = $1 AND m.account_id = $2 AND m.message_id = d.src",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(src)
    .bind(&new_ids)
    .bind(&uids)
    .bind(to.as_uuid())
    .bind(to_i64(modseq.0)?)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    sqlx::query(
        "INSERT INTO message_parts (tenant_id, account_id, message_id, part, blob_hash, blob_gen, size)
         SELECT p.tenant_id, p.account_id, d.new_id, p.part, p.blob_hash, p.blob_gen, p.size
         FROM unnest($3::uuid[], $4::uuid[]) AS d(src, new_id)
         JOIN message_parts p ON p.tenant_id = $1 AND p.account_id = $2 AND p.message_id = d.src",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(src)
    .bind(&new_ids)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    let inserted: Vec<Inserted> = new_ids
        .iter()
        .zip(&uids)
        .map(|(id, uid)| {
            Ok(Inserted {
                id: MessageId::from_uuid(*id),
                uid: Uid(to_u32(*uid)?),
                modseq,
            })
        })
        .collect::<Result<_>>()?;
    let mut changes: Vec<Change> = inserted
        .iter()
        .map(|i| Change::message(*i.id.as_uuid(), Op::Created, to, i.uid))
        .collect();
    changes.push(Change::mailbox(to, Op::Updated));
    changelog::log(tx, scope, modseq, &changes).await?;
    Ok(inserted)
}

/// Все `ids` должны существовать; возвращает их (без повторов, по порядку) с папками.
async fn require_all(
    tx: &mut PgConnection,
    scope: AccountScope,
    ids: &[MessageId],
) -> Result<Vec<(Uuid, Uuid)>> {
    let wanted: HashSet<&MessageId> = ids.iter().collect();
    let found = existing(tx, scope, &uuids(ids)).await?;
    if found.len() != wanted.len() {
        return Err(Error::NotFound);
    }
    Ok(found)
}

#[async_trait]
impl MessageStore for PgMeta {
    async fn insert(
        &self,
        scope: AccountScope,
        mailbox: MailboxId,
        msg: NewMessage,
    ) -> Result<Inserted> {
        check_flags(&msg.flags)?;
        if msg.skeleton.tenant != scope.tenant
            || msg.parts.iter().any(|p| p.blob.tenant != scope.tenant)
        {
            return Err(Error::invalid("объект письма другой организации"));
        }
        let meta = serde_json::to_value(&msg.meta).map_err(|e| Error::internal(e.to_string()))?;
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let ms = changelog::bump_modseq(&mut tx, scope).await?;
        let uid = alloc_uids(&mut tx, scope, mailbox, 1, ms).await?;
        let id = MessageId::new();
        sqlx::query(
            "INSERT INTO messages (tenant_id, account_id, message_id, mailbox_id, uid, modseq, size, received_at,
                                   flags, skeleton_hash, skeleton_gen, thread_key, meta)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(id.as_uuid())
        .bind(mailbox.as_uuid())
        .bind(i64::from(uid.0))
        .bind(to_i64(ms.0)?)
        .bind(to_i64(msg.size)?)
        .bind(msg.received_at)
        .bind(flags_vec(&msg.flags))
        .bind(msg.skeleton.hash.as_bytes().as_slice())
        .bind(i64::from(msg.skeleton.generation.0))
        .bind(&msg.meta.thread_key)
        .bind(meta)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        if !msg.parts.is_empty() {
            let parts: Vec<&str> = msg.parts.iter().map(|p| p.part.as_str()).collect();
            let hashes: Vec<Vec<u8>> = msg
                .parts
                .iter()
                .map(|p| p.blob.hash.as_bytes().to_vec())
                .collect();
            let gens: Vec<i64> = msg
                .parts
                .iter()
                .map(|p| i64::from(p.blob.generation.0))
                .collect();
            let sizes = msg
                .parts
                .iter()
                .map(|p| to_i64(p.size))
                .collect::<Result<Vec<_>>>()?;
            sqlx::query(
                "INSERT INTO message_parts (tenant_id, account_id, message_id, part, blob_hash, blob_gen, size)
                 SELECT $1, $2, $3, p.* FROM unnest($4::text[], $5::bytea[], $6::int8[], $7::int8[]) AS p",
            )
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(id.as_uuid())
            .bind(parts)
            .bind(hashes)
            .bind(gens)
            .bind(sizes)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        let changes = [
            Change::message(*id.as_uuid(), Op::Created, mailbox, uid),
            Change::mailbox(mailbox, Op::Updated),
        ];
        changelog::log(&mut tx, scope, ms, &changes).await?;
        let ev = changelog::event(
            scope,
            Some(mailbox),
            CoreEventKind::Delivered { message: id },
        );
        changelog::append_events(&mut tx, self.cfg.event_partitions, &[ev]).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(Inserted {
            id,
            uid,
            modseq: ms,
        })
    }

    async fn get(&self, scope: AccountScope, id: MessageId) -> Result<Option<Message>> {
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let rows: Vec<MessageRow> = sqlx::query_as(select_messages!("AND message_id = $3"))
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .bind(id.as_uuid())
            .fetch_all(&mut *tx)
            .await
            .map_err(db_err)?;
        let msg = assemble(&mut tx, scope, rows).await?.pop();
        tx.commit().await.map_err(db_err)?;
        Ok(msg)
    }

    async fn list(
        &self,
        scope: AccountScope,
        mailbox: MailboxId,
        query: MessageQuery,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let base = |sql| {
            sqlx::query_as::<_, MessageRow>(sql)
                .bind(scope.tenant.as_uuid())
                .bind(scope.account.as_uuid())
                .bind(mailbox.as_uuid())
                .bind(limit)
        };
        let q = match &query {
            MessageQuery::All => base(select_messages!("AND mailbox_id = $3 ORDER BY uid LIMIT $4")),
            MessageQuery::Uids(ranges) => base(select_messages!(
                "AND mailbox_id = $3 AND EXISTS (SELECT 1 FROM unnest($5::int8[], $6::int8[]) AS r(lo, hi)
                                                 WHERE uid BETWEEN r.lo AND r.hi)
                 ORDER BY uid LIMIT $4"
            ))
            .bind(ranges.iter().map(|r| i64::from(r.start().0)).collect::<Vec<_>>())
            .bind(ranges.iter().map(|r| i64::from(r.end().0)).collect::<Vec<_>>()),
            MessageQuery::Ids(ids) => base(select_messages!(
                "AND mailbox_id = $3 AND message_id = ANY($5) ORDER BY uid LIMIT $4"
            ))
            .bind(uuids(ids)),
            MessageQuery::ChangedSince(ms) => base(select_messages!(
                "AND mailbox_id = $3 AND modseq > $5 ORDER BY uid LIMIT $4"
            ))
            .bind(to_i64(ms.0)?),
        };
        let rows = q.fetch_all(&mut *tx).await.map_err(db_err)?;
        if rows.is_empty()
            && mailboxes::fetch(&mut tx, scope, mailbox, false)
                .await?
                .is_none()
        {
            return Err(Error::NotFound);
        }
        let out = assemble(&mut tx, scope, rows).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(out)
    }

    async fn set_flags(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        op: FlagOp,
        unchanged_since: Option<ModSeq>,
    ) -> Result<FlagsUpdate> {
        match &op {
            FlagOp::Add(f) | FlagOp::Remove(f) | FlagOp::Replace(f) => check_flags(f)?,
        }
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let current = changelog::lock_modseq(&mut tx, scope).await?;
        let rows: Vec<(Uuid, Uuid, i64, i64, Vec<String>)> = sqlx::query_as(
            "SELECT message_id, mailbox_id, uid, modseq, flags FROM messages
             WHERE tenant_id = $1 AND account_id = $2 AND message_id = ANY($3) ORDER BY message_id",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(uuids(ids))
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        let mut update = FlagsUpdate {
            modseq: current,
            ..FlagsUpdate::default()
        };
        let mut changed = Vec::new();
        for (id, mailbox, uid, modseq, flags) in rows {
            let old: Flags = flags.into_iter().map(parse_flag).collect();
            if unchanged_since.is_some_and(|u| modseq > i64::try_from(u.0).unwrap_or(i64::MAX)) {
                update.conflicts.push(MessageId::from_uuid(id));
                continue;
            }
            let new = apply(&op, &old);
            if new != old {
                changed.push((id, MailboxId::from_uuid(mailbox), Uid(to_u32(uid)?), new));
            }
        }
        if changed.is_empty() {
            tx.commit().await.map_err(db_err)?;
            return Ok(update);
        }
        let ms = changelog::bump_modseq(&mut tx, scope).await?;
        let ids: Vec<Uuid> = changed.iter().map(|c| c.0).collect();
        // Флаги не содержат пробелов (check_flags): набор передаётся строкой.
        let joined: Vec<String> = changed.iter().map(|c| flags_vec(&c.3).join(" ")).collect();
        sqlx::query(
            "UPDATE messages m SET flags = string_to_array(d.f, ' '), modseq = $5
             FROM unnest($3::uuid[], $4::text[]) AS d(id, f)
             WHERE m.tenant_id = $1 AND m.account_id = $2 AND m.message_id = d.id",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(&ids)
        .bind(joined)
        .bind(to_i64(ms.0)?)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        let mut boxes: Vec<Uuid> = changed.iter().map(|c| *c.1.as_uuid()).collect();
        boxes.sort_unstable();
        boxes.dedup();
        touch(&mut tx, scope, ms, &boxes).await?;
        let mut changes: Vec<Change> = changed
            .iter()
            .map(|(id, mailbox, uid, _)| Change::message(*id, Op::Updated, *mailbox, *uid))
            .collect();
        changes.extend(
            boxes
                .iter()
                .map(|b| Change::mailbox(MailboxId::from_uuid(*b), Op::Updated)),
        );
        changelog::log(&mut tx, scope, ms, &changes).await?;
        let events: Vec<_> = changed
            .iter()
            .map(|(id, mailbox, _, _)| {
                let message = MessageId::from_uuid(*id);
                changelog::event(
                    scope,
                    Some(*mailbox),
                    CoreEventKind::FlagsChanged { message },
                )
            })
            .collect();
        changelog::append_events(&mut tx, self.cfg.event_partitions, &events).await?;
        tx.commit().await.map_err(db_err)?;
        update.modseq = ms;
        update.changed = ids.into_iter().map(MessageId::from_uuid).collect();
        Ok(update)
    }

    async fn copy(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        to: MailboxId,
    ) -> Result<Vec<Inserted>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let ms = changelog::bump_modseq(&mut tx, scope).await?;
        let src: Vec<Uuid> = require_all(&mut tx, scope, ids)
            .await?
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        let inserted = clone_into(&mut tx, scope, ms, &src, to).await?;
        adjust_refs(&mut tx, scope, &src, 1).await?;
        let events: Vec<_> = inserted
            .iter()
            .map(|i| changelog::event(scope, Some(to), CoreEventKind::Delivered { message: i.id }))
            .collect();
        changelog::append_events(&mut tx, self.cfg.event_partitions, &events).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(inserted)
    }

    async fn move_to(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        to: MailboxId,
    ) -> Result<Vec<Inserted>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let ms = changelog::bump_modseq(&mut tx, scope).await?;
        let found = require_all(&mut tx, scope, ids).await?;
        let src: Vec<Uuid> = found.iter().map(|(id, _)| *id).collect();
        let inserted = clone_into(&mut tx, scope, ms, &src, to).await?;
        // Ссылки на объекты переходят к новым письмам; исходные удаляются (части — каскадом).
        let removed: Vec<(Uuid, Uuid, i64)> = sqlx::query_as(
            "DELETE FROM messages WHERE tenant_id = $1 AND account_id = $2 AND message_id = ANY($3)
             RETURNING message_id, mailbox_id, uid",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(&src)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        let removed: HashMap<Uuid, (MailboxId, Uid)> = removed
            .into_iter()
            .map(|(id, mb, uid)| Ok((id, (MailboxId::from_uuid(mb), Uid(to_u32(uid)?)))))
            .collect::<Result<_>>()?;
        let mut boxes: Vec<Uuid> = found.iter().map(|(_, mb)| *mb).collect();
        boxes.sort_unstable();
        boxes.dedup();
        touch(&mut tx, scope, ms, &boxes).await?;
        let mut changes = Vec::with_capacity(src.len() + boxes.len());
        let mut events = Vec::with_capacity(src.len());
        for (old, new) in src.iter().zip(&inserted) {
            let (from_mailbox, from_uid) = removed.get(old).copied().ok_or(Error::Conflict)?;
            changes.push(Change::message(*old, Op::Destroyed, from_mailbox, from_uid));
            events.push(changelog::event(
                scope,
                Some(to),
                CoreEventKind::Moved {
                    from_message: MessageId::from_uuid(*old),
                    from_mailbox,
                    message: new.id,
                },
            ));
        }
        changes.extend(
            boxes
                .iter()
                .map(|b| Change::mailbox(MailboxId::from_uuid(*b), Op::Updated)),
        );
        changelog::log(&mut tx, scope, ms, &changes).await?;
        changelog::append_events(&mut tx, self.cfg.event_partitions, &events).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(inserted)
    }

    async fn to_trash(&self, scope: AccountScope, ids: &[MessageId]) -> Result<ModSeq> {
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let current = changelog::lock_modseq(&mut tx, scope).await?;
        let trash = by_role(&mut tx, scope, SpecialUse::Trash)
            .await?
            .ok_or(Error::NotFound)?;
        // Отсутствующие и уже лежащие в корзине письма пропускаются.
        let src: Vec<Uuid> = existing(&mut tx, scope, &uuids(ids))
            .await?
            .into_iter()
            .filter(|(_, mb)| mb != trash.as_uuid())
            .map(|(id, _)| id)
            .collect();
        if src.is_empty() {
            tx.commit().await.map_err(db_err)?;
            return Ok(current);
        }
        let ms = changelog::bump_modseq(&mut tx, scope).await?;
        let events: Vec<_> = relocate(
            &mut tx,
            scope,
            ms,
            &src,
            trash,
            Some(self.cfg.retention.trash_retention),
        )
        .await?
        .into_iter()
        .map(|m| {
            let message = MessageId::from_uuid(m.id);
            changelog::event(scope, Some(trash), CoreEventKind::Trashed { message })
        })
        .collect();
        changelog::append_events(&mut tx, self.cfg.event_partitions, &events).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(ms)
    }

    /// Письма возвращаются в исходные папки с теми же `MessageId` и новыми UID; если исходной папки
    /// больше нет — во «Входящие». Письмо не в корзине — `Error::Invalid`.
    async fn restore(&self, scope: AccountScope, ids: &[MessageId]) -> Result<Vec<Inserted>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let ms = changelog::bump_modseq(&mut tx, scope).await?;
        let trash = by_role(&mut tx, scope, SpecialUse::Trash)
            .await?
            .ok_or(Error::NotFound)?;
        let inbox = by_role(&mut tx, scope, SpecialUse::Inbox)
            .await?
            .ok_or(Error::NotFound)?;
        let found = require_all(&mut tx, scope, ids).await?;
        if found.iter().any(|(_, mb)| mb != trash.as_uuid()) {
            return Err(Error::invalid("письмо не в корзине"));
        }
        let src: Vec<Uuid> = found.iter().map(|(id, _)| *id).collect();
        let targets: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
            "SELECT m.message_id, mb.mailbox_id FROM messages m
             LEFT JOIN mailboxes mb ON mb.tenant_id = m.tenant_id AND mb.account_id = m.account_id
                                   AND mb.mailbox_id = m.trashed_from
             WHERE m.tenant_id = $1 AND m.account_id = $2 AND m.message_id = ANY($3)",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(&src)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        let target: HashMap<Uuid, MailboxId> = targets
            .into_iter()
            .map(|(id, mb)| (id, mb.map_or(inbox, MailboxId::from_uuid)))
            .collect();
        // Группы по папке назначения в порядке первого появления.
        let mut groups: Vec<(MailboxId, Vec<Uuid>)> = Vec::new();
        for id in &src {
            let to = target.get(id).copied().unwrap_or(inbox);
            match groups.iter_mut().find(|(g, _)| *g == to) {
                Some((_, v)) => v.push(*id),
                None => groups.push((to, vec![*id])),
            }
        }
        let mut uids = HashMap::new();
        let mut events = Vec::with_capacity(src.len());
        for (to, group) in groups {
            for m in relocate(&mut tx, scope, ms, &group, to, None).await? {
                uids.insert(m.id, m.uid);
                let message = MessageId::from_uuid(m.id);
                events.push(changelog::event(
                    scope,
                    Some(to),
                    CoreEventKind::Moved {
                        from_message: message,
                        from_mailbox: trash,
                        message,
                    },
                ));
            }
        }
        changelog::append_events(&mut tx, self.cfg.event_partitions, &events).await?;
        tx.commit().await.map_err(db_err)?;
        src.iter()
            .map(|id| {
                Ok(Inserted {
                    id: MessageId::from_uuid(*id),
                    uid: uids.get(id).copied().ok_or(Error::Conflict)?,
                    modseq: ms,
                })
            })
            .collect()
    }

    async fn expunge_final(&self, scope: AccountScope, ids: &[MessageId]) -> Result<ModSeq> {
        let mut tx = begin(&self.pool, scope.tenant).await?;
        let current = changelog::lock_modseq(&mut tx, scope).await?;
        let found = existing(&mut tx, scope, &uuids(ids)).await?;
        if found.is_empty() {
            tx.commit().await.map_err(db_err)?;
            return Ok(current);
        }
        let ms = changelog::bump_modseq(&mut tx, scope).await?;
        let src: Vec<Uuid> = found.iter().map(|(id, _)| *id).collect();
        adjust_refs(&mut tx, scope, &src, -1).await?;
        let removed: Vec<(Uuid, Uuid, i64)> = sqlx::query_as(
            "DELETE FROM messages WHERE tenant_id = $1 AND account_id = $2 AND message_id = ANY($3)
             RETURNING message_id, mailbox_id, uid",
        )
        .bind(scope.tenant.as_uuid())
        .bind(scope.account.as_uuid())
        .bind(&src)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        let mut boxes: Vec<Uuid> = removed.iter().map(|r| r.1).collect();
        boxes.sort_unstable();
        boxes.dedup();
        touch(&mut tx, scope, ms, &boxes).await?;
        let mut changes = Vec::with_capacity(removed.len() + boxes.len());
        let mut events = Vec::with_capacity(removed.len());
        for (id, mb, uid) in removed {
            let mailbox = MailboxId::from_uuid(mb);
            changes.push(Change::message(
                id,
                Op::Destroyed,
                mailbox,
                Uid(to_u32(uid)?),
            ));
            let message = MessageId::from_uuid(id);
            events.push(changelog::event(
                scope,
                Some(mailbox),
                CoreEventKind::Purged { message },
            ));
        }
        changes.extend(
            boxes
                .iter()
                .map(|b| Change::mailbox(MailboxId::from_uuid(*b), Op::Updated)),
        );
        changelog::log(&mut tx, scope, ms, &changes).await?;
        changelog::append_events(&mut tx, self.cfg.event_partitions, &events).await?;
        tx.commit().await.map_err(db_err)?;
        Ok(ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_roundtrip_and_validation() {
        let flags: Flags = [
            Flag::Seen,
            Flag::Deleted,
            Flag::Keyword("$Forwarded".into()),
        ]
        .into();
        let back: Flags = flags_vec(&flags).into_iter().map(parse_flag).collect();
        assert_eq!(back, flags);
        assert!(check_flags(&flags).is_ok());
        for bad in ["", r"\Seen", "a b", "x\u{1}"] {
            let f: Flags = [Flag::Keyword(bad.into())].into();
            assert!(check_flags(&f).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn mime_part_order() {
        let mut parts = vec!["10", "2", "1.2", "1", "1.10", "1.3"];
        parts.sort_by(|a, b| part_order(a, b));
        assert_eq!(parts, vec!["1", "1.2", "1.3", "1.10", "2", "10"]);
    }

    #[test]
    fn flag_ops() {
        let old: Flags = [Flag::Seen, Flag::Flagged].into();
        let x: Flags = [Flag::Flagged, Flag::Draft].into();
        assert_eq!(
            apply(&FlagOp::Add(x.clone()), &old),
            [Flag::Seen, Flag::Flagged, Flag::Draft].into()
        );
        assert_eq!(apply(&FlagOp::Remove(x.clone()), &old), [Flag::Seen].into());
        assert_eq!(apply(&FlagOp::Replace(x.clone()), &old), x);
    }
}
