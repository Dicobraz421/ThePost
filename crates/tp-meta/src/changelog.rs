//! Журнал изменений учётной записи и запись событий ядра (CONTRACTS.md#ChangeLog, #Events; SRS 2.5).
//!
//! Изменяющая операция в одной транзакции: [`bump_modseq`] (строка учётной записи блокируется до фиксации —
//! номера изменений видны по порядку), запись изменений [`log`], события ядра [`append_events`] — последними,
//! чтобы блокировка счётчика раздела держалась минимально.

use std::collections::BTreeMap;

use sqlx::PgConnection;
use time::OffsetDateTime;
use tp_contracts::events::{CoreEvent, CoreEventKind, partition_of};
use tp_contracts::meta::AccountScope;
use tp_contracts::{Error, MailboxId, ModSeq, Result, Uid};
use uuid::Uuid;

use crate::schema::{db_err, to_u64};

/// Объект журнала.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Object {
    Message,
    Mailbox,
}

impl Object {
    fn as_str(self) -> &'static str {
        match self {
            Object::Message => "message",
            Object::Mailbox => "mailbox",
        }
    }
}

/// Вид изменения. `Moved` — письмо сменило папку с тем же идентификатором (корзина).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Created,
    Updated,
    Destroyed,
    Moved,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Op::Created => "created",
            Op::Updated => "updated",
            Op::Destroyed => "destroyed",
            Op::Moved => "moved",
        }
    }
}

/// Запись журнала изменений.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Change {
    pub object: Object,
    pub id: Uuid,
    pub op: Op,
    /// Папка письма (для `Moved` — новая).
    pub mailbox: Option<MailboxId>,
    /// UID письма (для `Moved` — новый).
    pub uid: Option<Uid>,
    /// Прежние папка и UID (`Moved`).
    pub from: Option<(MailboxId, Uid)>,
}

impl Change {
    /// Изменение папки.
    pub fn mailbox(id: MailboxId, op: Op) -> Self {
        Self {
            object: Object::Mailbox,
            id: *id.as_uuid(),
            op,
            mailbox: None,
            uid: None,
            from: None,
        }
    }

    /// Изменение письма в папке.
    pub fn message(id: Uuid, op: Op, mailbox: MailboxId, uid: Uid) -> Self {
        Self {
            object: Object::Message,
            id,
            op,
            mailbox: Some(mailbox),
            uid: Some(uid),
            from: None,
        }
    }
}

/// Следующий `ModSeq` учётной записи; строка учётной записи блокируется до конца транзакции.
pub(crate) async fn bump_modseq(tx: &mut PgConnection, scope: AccountScope) -> Result<ModSeq> {
    let ms: Option<i64> = sqlx::query_scalar(
        "UPDATE accounts SET modseq = modseq + 1
         WHERE tenant_id = $1 AND account_id = $2 RETURNING modseq",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    ms.ok_or(Error::NotFound).and_then(to_u64).map(ModSeq)
}

/// Текущий `ModSeq` учётной записи.
pub(crate) async fn current_modseq(tx: &mut PgConnection, scope: AccountScope) -> Result<ModSeq> {
    let ms: Option<i64> =
        sqlx::query_scalar("SELECT modseq FROM accounts WHERE tenant_id = $1 AND account_id = $2")
            .bind(scope.tenant.as_uuid())
            .bind(scope.account.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
    ms.ok_or(Error::NotFound).and_then(to_u64).map(ModSeq)
}

/// Записывает изменения с номером `modseq`; повтор той же записи — пустая операция.
pub(crate) async fn log(
    tx: &mut PgConnection,
    scope: AccountScope,
    modseq: ModSeq,
    changes: &[Change],
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let mut objects = Vec::with_capacity(changes.len());
    let mut ids = Vec::with_capacity(changes.len());
    let mut ops = Vec::with_capacity(changes.len());
    let mut mailboxes = Vec::with_capacity(changes.len());
    let mut uids = Vec::with_capacity(changes.len());
    let mut from_mailboxes = Vec::with_capacity(changes.len());
    let mut from_uids = Vec::with_capacity(changes.len());
    for c in changes {
        objects.push(c.object.as_str());
        ids.push(c.id);
        ops.push(c.op.as_str());
        mailboxes.push(c.mailbox.map(|m| *m.as_uuid()));
        uids.push(c.uid.map(|u| i64::from(u.0)));
        from_mailboxes.push(c.from.map(|(m, _)| *m.as_uuid()));
        from_uids.push(c.from.map(|(_, u)| i64::from(u.0)));
    }
    sqlx::query(
        "INSERT INTO changelog (tenant_id, account_id, modseq, object, object_id, op,
                                mailbox_id, uid, from_mailbox_id, from_uid)
         SELECT $1, $2, $3, c.* FROM unnest($4::text[], $5::uuid[], $6::text[], $7::uuid[], $8::int8[],
                                            $9::uuid[], $10::int8[]) AS c
         ON CONFLICT DO NOTHING",
    )
    .bind(scope.tenant.as_uuid())
    .bind(scope.account.as_uuid())
    .bind(crate::schema::to_i64(modseq.0)?)
    .bind(objects)
    .bind(ids)
    .bind(ops)
    .bind(mailboxes)
    .bind(uids)
    .bind(from_mailboxes)
    .bind(from_uids)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Событие ядра учётной записи с текущим моментом.
pub(crate) fn event(
    scope: AccountScope,
    mailbox: Option<MailboxId>,
    kind: CoreEventKind,
) -> CoreEvent {
    CoreEvent {
        tenant: scope.tenant,
        account: scope.account,
        mailbox,
        kind,
        at: OffsetDateTime::now_utc(),
    }
}

fn partition_arg(partition: u32) -> Result<i32> {
    i32::try_from(partition).map_err(|_| Error::invalid("номер раздела"))
}

/// Добавляет события в журнал событий ядра в транзакции вызывающего.
///
/// Позиции раздела выдаёт счётчик `event_partitions`: его строка блокируется до фиксации,
/// поэтому позиции становятся видимыми строго по возрастанию. Разделы блокируются по возрастанию номера.
pub(crate) async fn append_events(
    tx: &mut PgConnection,
    partitions: u32,
    events: &[CoreEvent],
) -> Result<()> {
    let mut by_partition: BTreeMap<u32, Vec<&CoreEvent>> = BTreeMap::new();
    for e in events {
        by_partition
            .entry(partition_of(e.account, partitions).0)
            .or_default()
            .push(e);
    }
    for (partition, batch) in by_partition {
        let n = i64::try_from(batch.len()).map_err(|_| Error::invalid("слишком много событий"))?;
        let last: i64 = sqlx::query_scalar(
            "INSERT INTO event_partitions (partition, last_pos) VALUES ($1, $2)
             ON CONFLICT (partition) DO UPDATE SET last_pos = event_partitions.last_pos + EXCLUDED.last_pos
             RETURNING last_pos",
        )
        .bind(partition_arg(partition)?)
        .bind(n)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let first = last - n + 1;
        let mut positions = Vec::with_capacity(batch.len());
        let mut tenants = Vec::with_capacity(batch.len());
        let mut accounts = Vec::with_capacity(batch.len());
        let mut mailboxes = Vec::with_capacity(batch.len());
        let mut kinds = Vec::with_capacity(batch.len());
        let mut ats = Vec::with_capacity(batch.len());
        for (i, e) in (0i64..).zip(batch) {
            positions.push(first + i);
            tenants.push(*e.tenant.as_uuid());
            accounts.push(*e.account.as_uuid());
            mailboxes.push(e.mailbox.map(|m| *m.as_uuid()));
            kinds.push(serde_json::to_string(&e.kind).map_err(|e| Error::internal(e.to_string()))?);
            ats.push(e.at);
        }
        sqlx::query(
            "INSERT INTO events (partition, pos, tenant_id, account_id, mailbox_id, kind, at)
             SELECT $1, e.pos, e.tenant_id, e.account_id, e.mailbox_id, e.kind::jsonb, e.at
             FROM unnest($2::int8[], $3::uuid[], $4::uuid[], $5::uuid[], $6::text[], $7::timestamptz[])
                  AS e(pos, tenant_id, account_id, mailbox_id, kind, at)",
        )
        .bind(partition_arg(partition)?)
        .bind(positions)
        .bind(tenants)
        .bind(accounts)
        .bind(mailboxes)
        .bind(kinds)
        .bind(ats)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    }
    Ok(())
}
