//! Изменение содержимого папок (RFC 9051, 6.3.12, 6.4.3, 6.4.7, 6.4.8; RFC 6851 MOVE; RFC 4315 UIDPLUS):
//! `APPEND`, `COPY`, `MOVE`, `EXPUNGE`, `UID EXPUNGE`, `CLOSE`.
//!
//! - `APPEND` — письмо клиента в папку учётной записи (`MailStore::append`): разбор, объекты частей, квота
//!   и метаданные — как у входящей почты, но это не доставка (решение 2026-11-01-trash-purge): точки принятия
//!   решений доставки не вызываются, письмо хранится побайтово; ответ `[APPENDUID]`.
//! - `COPY` и `MOVE` — `MailStore::copy`/`move_to`, ответ `[COPYUID]`. `MOVE` в корзину — удаление
//!   (`MailStore::delete_to_trash`, SRS 4.7): письмо сохраняет идентификатор и исходную папку для восстановления.
//! - `EXPUNGE` — письма с `\Deleted` переносятся в корзину. В самой корзине `EXPUNGE` (и `CLOSE`) — «очистить
//!   корзину» (`TrashStore::purge`, решение 2026-11-01-trash-purge): по настройке `trash.purge` письмо скрывается
//!   до конца срока корзины или стирается (под Legal Hold — скрывается). Хранилище корзины в шину не пишет:
//!   `MailboxChanged` корзины публикует сессия.
//! - Общие папки — по правам ACL (RFC 4314, 4; `ext`): `APPEND`, `COPY` и `MOVE` в папку — `i`, флаги без права
//!   у нового письма не ставятся; `MOVE` из папки — `t` и `e`; `EXPUNGE` — `e` (`CLOSE` без `e` не удаляет).
//!   Между учётными записями письмо копируется побайтово: `MailStore::fetch_raw` и `append` в папку владельца
//!   назначения (квота — его), `MOVE` затем удаляет исходные в корзину владельца источника.

use std::collections::{HashMap, HashSet};

use time::OffsetDateTime;
use tp_contracts::audit::AuditAction;
use tp_contracts::auth::Rights;
use tp_contracts::bus::{Event, EventKind};
use tp_contracts::meta::{AccountScope, Flag, Inserted, Message, MessageQuery, SpecialUse};
use tp_contracts::{Error, MailboxId, MessageId, Result};

use crate::ext::{allowed_flags, flag_rights};
use crate::proto::types::core::{Tag, Vec1};
use crate::proto::types::datetime::DateTime;
use crate::proto::types::extensions::binary::LiteralOrLiteral8;
use crate::proto::types::extensions::uidplus::{UidElement, UidSet};
use crate::proto::types::flag::Flag as ImapFlag;
use crate::proto::types::mailbox::Mailbox as ImapMailbox;
use crate::proto::types::response::{Code, Data, StatusKind};
use crate::proto::types::sequence::SequenceSet;
use crate::proto::{self};
use crate::session::{Session, Transport};
use crate::store::contract_flags;

/// Набор UID для `COPYUID` в заданном порядке (соответствие исходных и новых UID сохраняется).
fn uid_set(nums: &[u32]) -> Option<UidSet> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for &n in nums {
        match out.last_mut() {
            Some((_, end)) if end.checked_add(1) == Some(n) => *end = n,
            _ => out.push((n, n)),
        }
    }
    let elems: Vec<UidElement> = out
        .into_iter()
        .map(|(a, b)| {
            if a == b {
                UidElement::Single(proto::nz(a))
            } else {
                UidElement::Range(proto::nz(a), proto::nz(b))
            }
        })
        .collect();
    Vec1::try_from(elems).ok().map(UidSet)
}

impl<T: Transport> Session<T> {
    /// `APPEND`.
    pub(crate) async fn append(
        &mut self,
        tag: &Tag<'static>,
        mailbox: &ImapMailbox<'static>,
        flags: &[ImapFlag<'static>],
        date: Option<&DateTime>,
        message: &LiteralOrLiteral8<'static>,
    ) -> Result<()> {
        let Some(flags) = contract_flags(flags) else {
            self.bad(Some(tag), "Flag can't be stored");
            return Ok(());
        };
        let Some((node, rights)) = self.target(tag, &proto::mailbox_bytes(mailbox)).await? else {
            return Ok(());
        };
        let flags = allowed_flags(rights, flags);
        let scope = match node.owner {
            Some(owner) => owner,
            None => self.scope().ok_or(Error::Forbidden)?,
        };
        let data = match message {
            LiteralOrLiteral8::Literal(l) => l.data().to_vec(),
            LiteralOrLiteral8::Literal8(l) => l.data.to_vec(),
        };
        let received_at = date
            .and_then(|d| OffsetDateTime::from_unix_timestamp(d.as_ref().timestamp()).ok())
            .unwrap_or_else(OffsetDateTime::now_utc);
        // Письмо кладёт клиент (черновик, копия отправленного; в общую папку — по праву `i`): не доставка.
        let raw = Box::pin(futures::stream::iter([Ok(bytes::Bytes::from(data))]));
        let appended = self
            .ctx
            .deps
            .store
            .append(scope, node.mailbox.id, raw, flags, received_at)
            .await?;
        let st = self
            .ctx
            .deps
            .mailboxes
            .status(scope, node.mailbox.id)
            .await?;
        self.after_change(node.mailbox.id).await?;
        let code = Code::AppendUid {
            uid_validity: proto::nz(st.uid_validity.0),
            uid: proto::nz(appended.uid.0),
        };
        self.ok(tag, Some(code), "APPEND completed");
        Ok(())
    }

    /// `COPY`/`MOVE` (и `UID`-формы).
    pub(crate) async fn copy_or_move(
        &mut self,
        tag: &Tag<'static>,
        set: &SequenceSet,
        mailbox: &ImapMailbox<'static>,
        uid: bool,
        mv: bool,
    ) -> Result<()> {
        let Some(indexes) = self.resolve_set(set, uid) else {
            self.bad(Some(tag), "Invalid message sequence number");
            return Ok(());
        };
        let Some((node, to_rights)) = self.target(tag, &proto::mailbox_bytes(mailbox)).await?
        else {
            return Ok(());
        };
        let from_rights = self.selected_rights().await?;
        let Some(selected) = self.selected.as_ref() else {
            return Ok(());
        };
        let verb = match (uid, mv) {
            (false, false) => "COPY",
            (true, false) => "UID COPY",
            (false, true) => "MOVE",
            (true, true) => "UID MOVE",
        };
        if mv && selected.read_only {
            self.no(tag, None, "Mailbox is read-only");
            return Ok(());
        }
        if mv && !from_rights.contains(Rights::DELETE_MESSAGES | Rights::EXPUNGE) {
            self.no(tag, Some(proto::code("NOPERM")), "Permission denied");
            return Ok(());
        }
        let source = selected.mailbox;
        let targets: Vec<(usize, u32, MessageId)> = indexes
            .iter()
            .map(|&i| (i, selected.messages[i].0.0, selected.messages[i].1))
            .collect();
        if targets.is_empty() {
            self.ok(tag, None, &format!("{verb} completed"));
            return Ok(());
        }
        let scope = self.mailbox_scope().ok_or(Error::Forbidden)?;
        let to_scope = match node.owner {
            Some(owner) => owner,
            None => self.scope().ok_or(Error::Forbidden)?,
        };
        let ids: Vec<MessageId> = targets.iter().map(|t| t.2).collect();
        let to = node.mailbox.id;
        let same = scope == to_scope;
        let to_trash = same && mv && node.mailbox.role == Some(SpecialUse::Trash) && to != source;
        // Письма чужой папки читаются копированием: запись до выдачи копии.
        if Some(scope) != self.scope() {
            self.audit(scope, AuditAction::Read, source, &ids, None)
                .await?;
        }
        // Пары (исходный UID, новый UID) в порядке писем.
        let pairs: Vec<(u32, u32)> = if !same || !(to_trash || to_rights.contains(flag_rights())) {
            // Другая учётная запись или не все флаги разрешены: побайтовая копия с разрешёнными флагами.
            let pairs = self
                .copy_raw(scope, source, &targets, to_scope, to, to_rights)
                .await?;
            if mv {
                let done: HashSet<u32> = pairs.iter().map(|p| p.0).collect();
                let moved: Vec<MessageId> = targets
                    .iter()
                    .filter(|t| done.contains(&t.1))
                    .map(|t| t.2)
                    .collect();
                if !moved.is_empty() {
                    self.ctx.deps.store.delete_to_trash(scope, &moved).await?;
                }
            }
            pairs
        } else if to_trash {
            self.ctx.deps.store.delete_to_trash(scope, &ids).await?;
            // В корзине письмо сохраняет MessageId: новый UID — по нему.
            let moved = self
                .ctx
                .deps
                .messages
                .list(scope, to, MessageQuery::Ids(ids.clone()), ids.len())
                .await?;
            targets
                .iter()
                .filter_map(|t| moved.iter().find(|m| m.id == t.2).map(|m| (t.1, m.uid.0)))
                .collect()
        } else {
            let inserted: Vec<Inserted> = if mv {
                self.ctx.deps.store.move_to(scope, &ids, to).await?
            } else {
                self.ctx.deps.store.copy(scope, &ids, to).await?
            };
            targets
                .iter()
                .zip(&inserted)
                .map(|(t, ins)| (t.1, ins.uid.0))
                .collect()
        };
        let st = self.ctx.deps.mailboxes.status(to_scope, to).await?;
        let src: Vec<u32> = pairs.iter().map(|p| p.0).collect();
        let dst: Vec<u32> = pairs.iter().map(|p| p.1).collect();
        let code = uid_set(&src)
            .zip(uid_set(&dst))
            .map(|(source, destination)| Code::CopyUid {
                uid_validity: proto::nz(st.uid_validity.0),
                source,
                destination,
            });
        if mv {
            // RFC 6851, 4.3: COPYUID — в нетегированном OK перед EXPUNGE.
            if let Some(code) = code {
                self.untagged(StatusKind::Ok, Some(code), "Moved");
            }
            let moved: HashSet<u32> = src.into_iter().collect();
            let gone: Vec<usize> = targets
                .iter()
                .filter(|t| moved.contains(&t.1))
                .map(|t| t.0)
                .collect();
            let moved_ids: Vec<MessageId> = targets
                .iter()
                .filter(|t| moved.contains(&t.1))
                .map(|t| t.2)
                .collect();
            let action = if to_trash {
                AuditAction::Delete
            } else {
                AuditAction::Move
            };
            self.audit_done(scope, action, source, &moved_ids, Some(format!("to {to}")))
                .await;
            self.expunged(&gone);
            if to != source {
                self.after_change(to).await?;
            }
            self.ok(tag, None, &format!("{verb} completed"));
        } else {
            self.after_change(to).await?;
            self.ok(tag, code, &format!("{verb} completed"));
        }
        Ok(())
    }

    /// Копирует письма `targets` папки `source` учётной записи `scope` в папку `to` учётной записи `to_scope`
    /// побайтово (`fetch_raw` и `append`), с флагами, разрешёнными правами `rights` на папку назначения.
    /// Письма, удалённые другой сессией, пропускаются. Возвращает пары (исходный UID, новый UID).
    async fn copy_raw(
        &self,
        scope: AccountScope,
        source: MailboxId,
        targets: &[(usize, u32, MessageId)],
        to_scope: AccountScope,
        to: MailboxId,
        rights: Rights,
    ) -> Result<Vec<(u32, u32)>> {
        let deps = &self.ctx.deps;
        let ids: Vec<MessageId> = targets.iter().map(|t| t.2).collect();
        let list = deps
            .messages
            .list(scope, source, MessageQuery::Ids(ids), targets.len())
            .await?;
        let by_id: HashMap<MessageId, Message> = list.into_iter().map(|m| (m.id, m)).collect();
        let mut pairs = Vec::with_capacity(targets.len());
        for &(_, uid, id) in targets {
            let Some(m) = by_id.get(&id) else { continue };
            let raw = match deps.store.fetch_raw(scope, id, None).await {
                Ok(raw) => raw,
                Err(Error::NotFound) => continue,
                Err(e) => return Err(e),
            };
            let flags = allowed_flags(rights, m.flags.clone());
            let added = deps
                .store
                .append(to_scope, to, raw, flags, m.received_at)
                .await?;
            pairs.push((uid, added.uid.0));
        }
        Ok(pairs)
    }

    /// После изменения папки `mailbox`: если она выбрана — изменения клиенту (`EXISTS` и т. п.).
    pub(crate) async fn after_change(&mut self, mailbox: MailboxId) -> Result<()> {
        if self.selected.as_ref().is_some_and(|s| s.mailbox == mailbox) {
            self.sync().await?;
        }
        Ok(())
    }

    /// Убирает письма (индексы снимка) из выбранной папки и сообщает `EXPUNGE` (по убыванию номеров,
    /// чтобы номера ещё не сообщённых писем не сдвигались) или `VANISHED` (QRESYNC).
    pub(crate) fn expunged(&mut self, indexes: &[usize]) {
        let mut sorted = indexes.to_vec();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        sorted.dedup();
        let mut seqs = Vec::new();
        let mut uids = Vec::new();
        if let Some(selected) = self.selected.as_mut() {
            for i in sorted {
                if i < selected.messages.len() {
                    uids.push(selected.messages.remove(i).0.0);
                    seqs.push(u32::try_from(i + 1).unwrap_or(u32::MAX));
                }
            }
        }
        // После `ENABLE QRESYNC` — `VANISHED` вместо `EXPUNGE` (RFC 7162, 3.2.10).
        if self.qresync {
            self.vanished_now(&uids);
            return;
        }
        for seq in seqs {
            self.data(Data::Expunge(proto::nz(seq)));
        }
    }

    /// Письма выбранной папки с `\Deleted` (индексы снимка), ограниченные `only`.
    async fn deleted(&self, only: Option<&HashSet<usize>>) -> Result<Vec<(usize, MessageId)>> {
        let Some(selected) = self.selected.as_ref() else {
            return Ok(Vec::new());
        };
        let scope = self.mailbox_scope().ok_or(Error::Forbidden)?;
        let limit = self.ctx.config.limits.max_mailbox_messages;
        let list = self
            .ctx
            .deps
            .messages
            .list(scope, selected.mailbox, MessageQuery::All, limit)
            .await?;
        let deleted: HashSet<MessageId> = list
            .into_iter()
            .filter(|m| m.flags.contains(&Flag::Deleted))
            .map(|m| m.id)
            .collect();
        Ok(selected
            .messages
            .iter()
            .enumerate()
            .filter(|(i, (_, id))| deleted.contains(id) && only.is_none_or(|o| o.contains(i)))
            .map(|(i, (_, id))| (i, *id))
            .collect())
    }

    /// Выбранная папка — корзина.
    async fn in_trash(&self, mailbox: MailboxId) -> Result<bool> {
        let scope = self.mailbox_scope().ok_or(Error::Forbidden)?;
        let mb = self.ctx.deps.mailboxes.get(scope, mailbox).await?;
        Ok(mb.is_some_and(|m| m.role == Some(SpecialUse::Trash)))
    }

    /// «Очистить корзину»: письма `ids` корзины `trash` — `TrashStore::purge` по настройке `trash.purge`;
    /// `MailboxChanged` корзины — здесь (хранилище корзины в шину не пишет). Возвращает убранные у пользователя.
    async fn purge(
        &self,
        scope: AccountScope,
        trash: MailboxId,
        ids: &[MessageId],
    ) -> Result<HashSet<MessageId>> {
        let deps = &self.ctx.deps;
        let out = deps.trash.purge(scope, ids, &deps.hooks).await?;
        let gone: HashSet<MessageId> = out.hidden.into_iter().chain(out.erased).collect();
        if !gone.is_empty() {
            let event = Event {
                tenant: scope.tenant,
                account: Some(scope.account),
                kind: EventKind::MailboxChanged {
                    mailbox: trash,
                    modseq: out.modseq,
                },
            };
            if let Err(e) = deps.bus.publish(event).await {
                tracing::warn!(error = %e, "imap: уведомление об очистке корзины не опубликовано");
            }
        }
        Ok(gone)
    }

    /// `EXPUNGE` и `UID EXPUNGE` (`set` — набор UID).
    pub(crate) async fn expunge(
        &mut self,
        tag: &Tag<'static>,
        set: Option<&SequenceSet>,
    ) -> Result<()> {
        let only = match set {
            Some(set) => match self.resolve_set(set, true) {
                Some(i) => Some(i.into_iter().collect::<HashSet<_>>()),
                None => {
                    self.bad(Some(tag), "Invalid UID set");
                    return Ok(());
                }
            },
            None => None,
        };
        let Some(selected) = self.selected.as_ref() else {
            return Ok(());
        };
        if selected.read_only {
            self.no(tag, None, "Mailbox is read-only");
            return Ok(());
        }
        let mailbox = selected.mailbox;
        if !self.selected_rights().await?.contains(Rights::EXPUNGE) {
            self.no(tag, Some(proto::code("NOPERM")), "Permission denied");
            return Ok(());
        }
        let verb = if set.is_some() {
            "UID EXPUNGE"
        } else {
            "EXPUNGE"
        };
        let found = self.deleted(only.as_ref()).await?;
        if found.is_empty() {
            self.ok(tag, None, &format!("{verb} completed"));
            return Ok(());
        }
        let scope = self.mailbox_scope().ok_or(Error::Forbidden)?;
        let ids: Vec<MessageId> = found.iter().map(|f| f.1).collect();
        let (indexes, removed): (Vec<usize>, Vec<MessageId>) = if self.in_trash(mailbox).await? {
            let gone = self.purge(scope, mailbox, &ids).await?;
            found
                .iter()
                .filter(|f| gone.contains(&f.1))
                .copied()
                .unzip()
        } else {
            self.ctx.deps.store.delete_to_trash(scope, &ids).await?;
            found.iter().copied().unzip()
        };
        self.audit_done(scope, AuditAction::Delete, mailbox, &removed, None)
            .await;
        self.expunged(&indexes);
        self.ok(tag, None, &format!("{verb} completed"));
        Ok(())
    }

    /// `CLOSE`: письма с `\Deleted` — в корзину (в самой корзине — «очистить корзину») без ответов `EXPUNGE`,
    /// папка закрывается.
    pub(crate) async fn close(&mut self, tag: &Tag<'static>) -> Result<()> {
        let Some(selected) = self.selected.as_ref() else {
            return Ok(());
        };
        let (mailbox, read_only) = (selected.mailbox, selected.read_only);
        // Без права `e` письма не удаляются, папка закрывается (RFC 4314, 4).
        if !read_only && self.selected_rights().await?.contains(Rights::EXPUNGE) {
            let found = self.deleted(None).await?;
            if !found.is_empty() {
                let scope = self.mailbox_scope().ok_or(Error::Forbidden)?;
                let ids: Vec<MessageId> = found.iter().map(|f| f.1).collect();
                let removed: Vec<MessageId> = if self.in_trash(mailbox).await? {
                    let gone = self.purge(scope, mailbox, &ids).await?;
                    ids.into_iter().filter(|id| gone.contains(id)).collect()
                } else {
                    self.ctx.deps.store.delete_to_trash(scope, &ids).await?;
                    ids
                };
                self.audit_done(scope, AuditAction::Delete, mailbox, &removed, None)
                    .await;
            }
        }
        self.selected = None;
        self.ok(tag, None, "CLOSE completed");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uid_set_keeps_order() {
        let fmt = |s: Option<UidSet>| {
            s.map(|s| {
                s.0.as_ref()
                    .iter()
                    .map(|e| match e {
                        UidElement::Single(a) => a.to_string(),
                        UidElement::Range(a, b) => format!("{a}:{b}"),
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
        };
        assert_eq!(fmt(uid_set(&[1, 2, 3, 7])), Some("1:3,7".into()));
        assert_eq!(fmt(uid_set(&[5, 3, 4])), Some("5,3:4".into()));
        assert_eq!(fmt(uid_set(&[])), None);
    }
}
