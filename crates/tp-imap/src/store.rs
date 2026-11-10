//! `STORE` и `UID STORE` (RFC 9051, 6.4.6) с CONDSTORE (RFC 7162): `UNCHANGEDSINCE`, `MODSEQ` в ответах,
//! `[MODIFIED]` для писем, изменённых после указанного `MODSEQ`.
//!
//! Флаги меняет почтовое хранилище (`MailStore::set_flags`: одна транзакция, новый `ModSeq`, уведомление
//! шины). Ответ `FETCH` — по состоянию после изменения; `.SILENT` подавляет его, но при включённом
//! CONDSTORE для изменённых писем `FETCH (MODSEQ …)` отправляется всё равно (RFC 7162, 3.1.3).
//! Письма, не изменённые из-за `UNCHANGEDSINCE`, перечисляются в `OK [MODIFIED …]` (номера или UID).
//! В общей папке — по правам ACL (RFC 4314, 4): флаги без права не меняются (`ext`).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroU64;

use tp_contracts::meta::{Flag, FlagOp, Flags, Message, MessageQuery};
use tp_contracts::{Error, MessageId, ModSeq, Result};

use crate::ext::{allowed_flags, flag_rights, has_any, may_set};
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
        let Some(read_only) = self.selected.as_ref().map(|s| s.read_only) else {
            return Ok(());
        };
        if read_only {
            self.no(tag, None, "Mailbox is read-only");
            return Ok(());
        }
        // Общая папка: флаги без права пропускаются; ни одного разрешённого — `NO [NOPERM]` (RFC 4314, 4).
        let rights = self.selected_rights().await?;
        let full = rights.contains(flag_rights());
        let requested = flags.len();
        let flags = allowed_flags(rights, flags);
        if !has_any(rights, flag_rights()) || (requested > 0 && flags.is_empty()) {
            self.no(tag, Some(proto::code("NOPERM")), "Permission denied");
            return Ok(());
        }
        let Some(selected) = self.selected.as_ref() else {
            return Ok(());
        };
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
        let scope = self.mailbox_scope().ok_or(Error::Forbidden)?;
        let ids: Vec<MessageId> = targets.iter().map(|t| t.2).collect();
        let ops: Vec<(FlagOp, Vec<MessageId>)> = match kind {
            StoreType::Add => vec![(FlagOp::Add(flags), ids)],
            StoreType::Remove => vec![(FlagOp::Remove(flags), ids)],
            StoreType::Replace if full => vec![(FlagOp::Replace(flags), ids)],
            // Неполные права: флаги без права у каждого письма остаются как были.
            StoreType::Replace => {
                let list = self
                    .ctx
                    .deps
                    .messages
                    .list(scope, mailbox, MessageQuery::Ids(ids.clone()), ids.len())
                    .await?;
                let mut groups: BTreeMap<Flags, Vec<MessageId>> = BTreeMap::new();
                for m in list {
                    let kept = m.flags.into_iter().filter(|f| !may_set(rights, f));
                    let new: Flags = kept.chain(flags.iter().cloned()).collect();
                    groups.entry(new).or_default().push(m.id);
                }
                groups
                    .into_iter()
                    .map(|(f, ids)| (FlagOp::Replace(f), ids))
                    .collect()
            }
        };
        let mut conflicts: HashSet<MessageId> = HashSet::new();
        let mut changed: HashSet<MessageId> = HashSet::new();
        for (op, ids) in ops {
            let update = self
                .ctx
                .deps
                .store
                .set_flags(scope, &ids, op, unchanged_since)
                .await?;
            conflicts.extend(update.conflicts);
            changed.extend(update.changed);
        }
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
