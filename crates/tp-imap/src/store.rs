//! `STORE` и `UID STORE` (RFC 9051, 6.4.6) с CONDSTORE (RFC 7162): `UNCHANGEDSINCE`, `MODSEQ` в ответах,
//! `[MODIFIED]` для писем, изменённых после указанного `MODSEQ`.
//!
//! Флаги меняет почтовое хранилище (`MailStore::set_flags`: одна транзакция, новый `ModSeq`, уведомление
//! шины). Ответ `FETCH` — по состоянию после изменения; `.SILENT` подавляет его, но при включённом
//! CONDSTORE для изменённых писем `FETCH (MODSEQ …)` отправляется всё равно (RFC 7162, 3.1.3).
//! Письма, не изменённые из-за `UNCHANGEDSINCE`, перечисляются в `OK [MODIFIED …]` (номера или UID).

use std::collections::{HashMap, HashSet};
use std::num::NonZeroU64;

use tp_contracts::meta::{Flag, FlagOp, Flags, Message, MessageQuery};
use tp_contracts::{Error, MessageId, ModSeq, Result};

use crate::proto::types::command::{CommandBody, StoreModifier};
use crate::proto::types::core::{Tag, Vec1};
use crate::proto::types::fetch::MessageDataItem;
use crate::proto::types::flag::{Flag as ImapFlag, StoreResponse, StoreType};
use crate::proto::types::response::Code;
use crate::proto::{self};
use crate::session::{Session, Transport};

/// Флаги команды в флаги хранилища; `None` — флаг нельзя установить (`\Recent`, `\Foo`).
pub(crate) fn contract_flags(flags: &[ImapFlag<'_>]) -> Option<Flags> {
    flags
        .iter()
        .map(|f| {
            Some(match f {
                ImapFlag::Seen => Flag::Seen,
                ImapFlag::Answered => Flag::Answered,
                ImapFlag::Flagged => Flag::Flagged,
                ImapFlag::Deleted => Flag::Deleted,
                ImapFlag::Draft => Flag::Draft,
                ImapFlag::Keyword(k) => Flag::Keyword(k.inner().to_owned()),
                ImapFlag::Extension(_) => return None,
            })
        })
        .collect()
}

/// `MODSEQ` для ответа (`MODSEQ` письма не бывает нулевым после изменения).
pub(crate) fn modseq_item(m: ModSeq) -> MessageDataItem<'static> {
    MessageDataItem::ModSeq(NonZeroU64::new(m.0).unwrap_or(NonZeroU64::MIN))
}

impl<T: Transport> Session<T> {
    /// `STORE`/`UID STORE`.
    pub(crate) async fn store(
        &mut self,
        tag: &Tag<'static>,
        body: CommandBody<'static>,
    ) -> Result<()> {
        let CommandBody::Store {
            sequence_set,
            kind,
            response,
            flags,
            uid,
            modifiers,
        } = body
        else {
            return Ok(());
        };
        let Some(indexes) = self.resolve_set(&sequence_set, uid) else {
            self.bad(Some(tag), "Invalid message sequence number");
            return Ok(());
        };
        let Some(flags) = contract_flags(&flags) else {
            self.bad(Some(tag), "Flag can't be stored");
            return Ok(());
        };
        let Some(selected) = self.selected.as_ref() else {
            return Ok(());
        };
        if selected.read_only {
            self.no(tag, None, "Mailbox is read-only");
            return Ok(());
        }
        let mailbox = selected.mailbox;
        let targets: Vec<(usize, u32, MessageId)> = indexes
            .iter()
            .map(|&i| (i, selected.messages[i].0.0, selected.messages[i].1))
            .collect();
        let unchanged_since = modifiers
            .iter()
            .map(|StoreModifier::UnchangedSince(n)| ModSeq(*n))
            .next();
        // UNCHANGEDSINCE — команда, включающая CONDSTORE (RFC 7162, 3.1).
        if unchanged_since.is_some() {
            self.condstore = true;
        }
        let verb = if uid { "UID STORE" } else { "STORE" };
        if targets.is_empty() {
            self.ok(tag, None, &format!("{verb} completed"));
            return Ok(());
        }
        let op = match kind {
            StoreType::Add => FlagOp::Add(flags),
            StoreType::Remove => FlagOp::Remove(flags),
            StoreType::Replace => FlagOp::Replace(flags),
        };
        let scope = self.scope().ok_or(Error::Forbidden)?;
        let ids: Vec<MessageId> = targets.iter().map(|t| t.2).collect();
        let update = self
            .ctx
            .deps
            .store
            .set_flags(scope, &ids, op, unchanged_since)
            .await?;
        let conflicts: HashSet<MessageId> = update.conflicts.iter().copied().collect();
        let changed: HashSet<MessageId> = update.changed.iter().copied().collect();
        let silent = response == StoreResponse::Silent;
        if !silent || self.condstore {
            let batch = self.ctx.config.limits.fetch_batch.max(1);
            for chunk in targets.chunks(batch) {
                let ids: Vec<MessageId> = chunk.iter().map(|t| t.2).collect();
                let list = self
                    .ctx
                    .deps
                    .messages
                    .list(scope, mailbox, MessageQuery::Ids(ids.clone()), ids.len())
                    .await?;
                let by_id: HashMap<MessageId, Message> =
                    list.into_iter().map(|m| (m.id, m)).collect();
                for &(i, msg_uid, id) in chunk {
                    // Удалено другой сессией или не изменено из-за UNCHANGEDSINCE — без FETCH.
                    let Some(msg) = by_id.get(&id).filter(|_| !conflicts.contains(&id)) else {
                        continue;
                    };
                    if silent && !changed.contains(&id) {
                        continue;
                    }
                    let mut items = Vec::new();
                    if !silent {
                        items.push(MessageDataItem::Flags(crate::fetch::imap_flags(&msg.flags)));
                    }
                    if uid {
                        items.push(MessageDataItem::Uid(proto::nz(msg_uid)));
                    }
                    if self.condstore {
                        items.push(modseq_item(msg.modseq));
                    }
                    if let Ok(items) = Vec1::try_from(items) {
                        let seq = proto::nz(u32::try_from(i + 1).unwrap_or(u32::MAX));
                        self.fetch_data(seq, items, &[]);
                    }
                }
                self.maybe_flush().await?;
            }
        }
        let failed: Vec<u32> = targets
            .iter()
            .filter(|t| conflicts.contains(&t.2))
            .map(|&(i, msg_uid, _)| {
                if uid {
                    msg_uid
                } else {
                    u32::try_from(i + 1).unwrap_or(u32::MAX)
                }
            })
            .collect();
        match proto::sequence_set(&failed) {
            Some(set) => self.ok(tag, Some(Code::Modified(set)), "Conditional STORE failed"),
            None => self.ok(tag, None, &format!("{verb} completed")),
        }
        Ok(())
    }
}
