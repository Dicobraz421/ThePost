//! QRESYNC (RFC 7162, 3.2): быстрая ресинхронизация папки.
//!
//! - `ENABLE QRESYNC` включает QRESYNC и CONDSTORE; без него параметр `QRESYNC` в `SELECT` и модификатор
//!   `VANISHED` — `BAD`.
//! - `SELECT`/`EXAMINE … (QRESYNC (uidvalidity modseq [known-uids [seq-match]]))`: при совпадении UIDVALIDITY
//!   после обычных ответов — `* VANISHED (EARLIER)` удалённых после `modseq` (из журнала изменений,
//!   в пределах `known-uids`) и `FETCH (UID FLAGS MODSEQ)` писем, изменённых после `modseq`.
//!   `seq-match-data` не используется (RFC 7162, 3.2.5.2 разрешает его не учитывать).
//! - `UID FETCH … (CHANGEDSINCE m VANISHED)`: перед `FETCH` — `* VANISHED (EARLIER)` удалённых после `m`
//!   UID из набора.
//! - После `ENABLE QRESYNC` удаление писем сообщается `* VANISHED`, а не `EXPUNGE` (RFC 7162, 3.2.10).
//!
//! Журнал изменений старше `modseq` (`Error::Conflict`) — удалёнными считаются все UID из `known-uids`
//! (или `1:UIDNEXT-1`), которых нет в папке: RFC 7162, 3.2.5.1 разрешает сообщать и UID, которых не было.

use std::collections::HashSet;
use std::num::NonZeroU32;

use tp_contracts::meta::Message;
use tp_contracts::{Error, MailboxId, ModSeq, Result, Uid, UidValidity};

use crate::proto::types::core::Vec1;
use crate::proto::types::fetch::MessageDataItem;
use crate::proto::types::response::Data;
use crate::proto::types::sequence::{Sequence, SequenceSet};
use crate::proto::{self};
use crate::session::{Session, Transport};
use crate::store::modseq_item;

/// Параметр `QRESYNC` команды `SELECT`/`EXAMINE`.
#[derive(Debug, Clone)]
pub(crate) struct QResync {
    /// UIDVALIDITY, известная клиенту.
    pub uid_validity: u32,
    /// Последний `MODSEQ`, известный клиенту.
    pub modseq: u64,
    /// UID, известные клиенту; `None` — все.
    pub known_uids: Option<SequenceSet>,
}

/// Отрезки набора (`*` — `largest`), по возрастанию, слитые.
pub(crate) fn set_ranges(set: &SequenceSet, largest: u32) -> Vec<(u32, u32)> {
    let largest = NonZeroU32::new(largest).unwrap_or(NonZeroU32::MIN);
    let mut ranges: Vec<(u32, u32)> = set
        .0
        .as_ref()
        .iter()
        .map(|s| match s {
            Sequence::Single(v) => (v.expand(largest).get(), v.expand(largest).get()),
            Sequence::Range(a, b) => {
                let (a, b) = (a.expand(largest).get(), b.expand(largest).get());
                (a.min(b), a.max(b))
            }
        })
        .collect();
    ranges.sort_unstable();
    let mut out: Vec<(u32, u32)> = Vec::new();
    for (a, b) in ranges {
        match out.last_mut() {
            Some((_, end)) if a <= end.saturating_add(1) => *end = (*end).max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// Отрезки `ranges` без чисел `present` (то и другое по возрастанию).
pub(crate) fn minus(ranges: &[(u32, u32)], present: &[u32]) -> Vec<(u32, u32)> {
    let mut out = Vec::new();
    let mut p = present.iter().map(|&n| u64::from(n)).peekable();
    for &(a, b) in ranges {
        let (mut start, b) = (u64::from(a), u64::from(b));
        while let Some(&n) = p.peek() {
            if n > b {
                break;
            }
            p.next();
            if n < start {
                continue;
            }
            if n > start {
                out.push((start, n - 1));
            }
            start = n + 1;
        }
        if start <= b {
            out.push((start, b));
        }
    }
    out.into_iter()
        .filter(|(a, _)| *a > 0)
        .filter_map(|(a, b)| Some((u32::try_from(a).ok()?, u32::try_from(b).ok()?)))
        .collect()
}

fn contains(ranges: &[(u32, u32)], n: u32) -> bool {
    ranges.iter().any(|(a, b)| (*a..=*b).contains(&n))
}

impl<T: Transport> Session<T> {
    /// UID писем папки, удалённых после `since`, в пределах `within` (по возрастанию отрезками).
    /// Журнал старше `since` — все UID `within`, которых нет среди `present`.
    async fn vanished_since(
        &self,
        mailbox: MailboxId,
        since: ModSeq,
        within: &[(u32, u32)],
        present: &[u32],
    ) -> Result<Vec<(u32, u32)>> {
        let scope = self.mailbox_scope().ok_or(Error::Forbidden)?;
        let limit = self.ctx.config.limits.max_mailbox_messages.max(1);
        let present_set: HashSet<u32> = present.iter().copied().collect();
        let mut uids = Vec::new();
        let mut since = since;
        loop {
            let changes = match self
                .ctx
                .deps
                .changes
                .message_changes(scope, Some(mailbox), since, limit)
                .await
            {
                Ok(c) => c,
                Err(Error::Conflict) => return Ok(minus(within, present)),
                Err(e) => return Err(e),
            };
            uids.extend(
                changes
                    .destroyed
                    .iter()
                    .filter(|c| c.mailbox == mailbox)
                    .map(|c| c.uid.0)
                    .filter(|u| contains(within, *u) && !present_set.contains(u)),
            );
            since = changes.new_state;
            if !changes.has_more {
                break;
            }
        }
        Ok(proto::runs(&uids))
    }

    fn vanished_data(&mut self, ranges: &[(u32, u32)], earlier: bool) {
        if let Some(set) = proto::range_set(ranges) {
            self.data(Data::Vanished {
                earlier,
                known_uids: set,
            });
        }
    }

    /// Ответы `SELECT … (QRESYNC …)` перед завершающим `OK`: `VANISHED (EARLIER)` и `FETCH` изменённых.
    /// `list` — письма папки при выборе (по возрастанию UID).
    pub(crate) async fn qresync_select(
        &mut self,
        q: &QResync,
        mailbox: MailboxId,
        uid_validity: UidValidity,
        uid_next: Uid,
        list: &[Message],
    ) -> Result<()> {
        // Другая UIDVALIDITY: кэш клиента недействителен, он увидит это по коду `UIDVALIDITY`.
        if q.uid_validity != uid_validity.0 {
            return Ok(());
        }
        let largest = uid_next.0.saturating_sub(1).max(1);
        let within = match &q.known_uids {
            Some(set) => set_ranges(set, largest),
            None => vec![(1, largest)],
        };
        let present: Vec<u32> = list.iter().map(|m| m.uid.0).collect();
        let vanished = self
            .vanished_since(mailbox, ModSeq(q.modseq), &within, &present)
            .await?;
        self.vanished_data(&vanished, true);
        for (i, m) in list.iter().enumerate() {
            if m.modseq.0 <= q.modseq {
                continue;
            }
            let items = vec![
                MessageDataItem::Uid(proto::nz(m.uid.0)),
                MessageDataItem::Flags(crate::fetch::imap_flags(&m.flags)),
                modseq_item(m.modseq),
            ];
            if let Ok(items) = Vec1::try_from(items) {
                let seq = proto::nz(u32::try_from(i + 1).unwrap_or(u32::MAX));
                self.fetch_data(seq, items, &[]);
            }
            self.maybe_flush().await?;
        }
        Ok(())
    }

    /// `UID FETCH … (CHANGEDSINCE m VANISHED)`: `VANISHED (EARLIER)` UID набора, удалённых после `m`.
    pub(crate) async fn fetch_vanished(&mut self, set: &SequenceSet, since: u64) -> Result<()> {
        let Some(selected) = self.selected.as_ref() else {
            return Ok(());
        };
        let mailbox = selected.mailbox;
        let present: Vec<u32> = selected.messages.iter().map(|m| m.0.0).collect();
        let scope = self.mailbox_scope().ok_or(Error::Forbidden)?;
        let st = self.ctx.deps.mailboxes.status(scope, mailbox).await?;
        let largest = st.uid_next.0.saturating_sub(1).max(1);
        let within = set_ranges(set, largest);
        let vanished = self
            .vanished_since(mailbox, ModSeq(since), &within, &present)
            .await?;
        self.vanished_data(&vanished, true);
        Ok(())
    }

    /// Удалённые из выбранной папки письма (UID) — `* VANISHED` (после `ENABLE QRESYNC`).
    pub(crate) fn vanished_now(&mut self, uids: &[u32]) {
        let ranges = proto::runs(uids);
        self.vanished_data(&ranges, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_minus_present() {
        assert_eq!(
            minus(&[(1, 10)], &[2, 3, 7, 10]),
            vec![(1, 1), (4, 6), (8, 9)]
        );
        assert_eq!(minus(&[(1, 3), (5, 6)], &[]), vec![(1, 3), (5, 6)]);
        assert_eq!(minus(&[(2, 3)], &[1, 2, 3, 4]), Vec::<(u32, u32)>::new());
        assert_eq!(minus(&[(1, 5)], &[5]), vec![(1, 4)]);
        assert_eq!(
            minus(&[(1, 2), (4, 6)], &[1, 5]),
            vec![(2, 2), (4, 4), (6, 6)]
        );
        assert_eq!(
            minus(&[(u32::MAX - 1, u32::MAX)], &[u32::MAX]),
            vec![(u32::MAX - 1, u32::MAX - 1)]
        );
    }
}
