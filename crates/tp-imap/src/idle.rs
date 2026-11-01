//! `IDLE` (RFC 2177, RFC 9051, 6.3.13) и уведомления о выбранной папке.
//!
//! Изменения выбранной папки клиенту сообщает [`Session::sync`]: журнал изменений (`ChangeLog`) после `ModSeq`,
//! уже сообщённого клиенту, — новые письма (`EXISTS`), удалённые и перенесённые (`EXPUNGE`), изменённые флаги
//! (`FETCH (FLAGS … UID … [MODSEQ …])`). Вызывается в `NOOP`, `CHECK`, `IDLE` и после команд, меняющих
//! выбранную папку. Журнал старше хранимого — сверка со списком писем папки.
//!
//! В `IDLE` сессия подписана на шину (`EventBus`, уведомления учётной записи): `MailboxChanged` выбранной
//! папки — `sync`. Шина at-most-once: при обрыве потока сессия переподписывается и сверяется с журналом.
//! `IDLE` длится не дольше [`Timeouts::idle`](crate::config::Timeouts), затем `BYE`; остановка узла — `BYE`.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};

use futures::StreamExt;
use tokio::io::AsyncReadExt;
use tp_contracts::bus::{Event, EventKind, EventStream};
use tp_contracts::meta::{Message, MessageQuery};
use tp_contracts::{Error, MessageId, ModSeq, Result, Uid};

use crate::proto::types::core::{Tag, Vec1};
use crate::proto::types::fetch::MessageDataItem;
use crate::proto::types::response::{CommandContinuationRequest, Data, Response};
use crate::proto::{self, Expect, Input};
use crate::session::{Session, Transport, io_error};
use crate::store::modseq_item;

/// Что разбудило сессию в `IDLE`.
enum Wake {
    Shutdown,
    Timeout,
    Event(Option<Event>),
    Read(std::io::Result<usize>),
}

/// Изменения папки для клиента.
#[derive(Default)]
struct Delta {
    created: Vec<(Uid, MessageId)>,
    updated: HashSet<MessageId>,
    destroyed: HashSet<Uid>,
}

impl<T: Transport> Session<T> {
    /// Изменения выбранной папки после сообщённого `ModSeq`.
    async fn delta(&self) -> Result<Option<(Delta, ModSeq)>> {
        let Some(selected) = self.selected.as_ref() else {
            return Ok(None);
        };
        let scope = self.scope().ok_or(Error::Forbidden)?;
        let limit = self.ctx.config.limits.max_mailbox_messages;
        let mut delta = Delta::default();
        let mut since = selected.modseq;
        loop {
            let changes = match self
                .ctx
                .deps
                .changes
                .message_changes(scope, Some(selected.mailbox), since, limit)
                .await
            {
                Ok(c) => c,
                // Журнал старше хранимого — сверка со списком писем.
                Err(Error::Conflict) => return self.full_delta().await.map(Some),
                Err(e) => return Err(e),
            };
            delta
                .created
                .extend(changes.created.iter().map(|c| (c.uid, c.id)));
            delta.updated.extend(changes.updated.iter().map(|c| c.id));
            delta
                .destroyed
                .extend(changes.destroyed.iter().map(|c| c.uid));
            since = changes.new_state;
            if !changes.has_more {
                return Ok(Some((delta, since)));
            }
        }
    }

    /// Изменения по полному списку писем папки (журнал недоступен): флаги сообщаются для всех писем.
    async fn full_delta(&self) -> Result<(Delta, ModSeq)> {
        let Some(selected) = self.selected.as_ref() else {
            return Ok((Delta::default(), ModSeq(0)));
        };
        let scope = self.scope().ok_or(Error::Forbidden)?;
        let limit = self.ctx.config.limits.max_mailbox_messages;
        let st = self
            .ctx
            .deps
            .mailboxes
            .status(scope, selected.mailbox)
            .await?;
        let list = self
            .ctx
            .deps
            .messages
            .list(scope, selected.mailbox, MessageQuery::All, limit)
            .await?;
        let now: HashSet<Uid> = list.iter().map(|m| m.uid).collect();
        let known: HashSet<Uid> = selected.messages.iter().map(|m| m.0).collect();
        let delta = Delta {
            created: list
                .iter()
                .filter(|m| !known.contains(&m.uid))
                .map(|m| (m.uid, m.id))
                .collect(),
            updated: list.iter().map(|m| m.id).collect(),
            destroyed: known.difference(&now).copied().collect(),
        };
        Ok((delta, st.highest_modseq))
    }

    /// Сообщает клиенту изменения выбранной папки: `EXPUNGE`, `EXISTS`, `FETCH` изменённых флагов.
    pub(crate) async fn sync(&mut self) -> Result<()> {
        let Some((delta, modseq)) = self.delta().await? else {
            return Ok(());
        };
        let Some(selected) = self.selected.as_ref() else {
            return Ok(());
        };
        let mailbox = selected.mailbox;
        let gone: Vec<usize> = selected
            .messages
            .iter()
            .enumerate()
            .filter(|(_, (uid, _))| delta.destroyed.contains(uid))
            .map(|(i, _)| i)
            .collect();
        self.expunged(&gone);
        let Some(selected) = self.selected.as_mut() else {
            return Ok(());
        };
        let last = selected.messages.last().map_or(0, |m| m.0.0);
        let mut fresh: Vec<(Uid, MessageId)> = delta
            .created
            .into_iter()
            .filter(|(uid, _)| uid.0 > last && !delta.destroyed.contains(uid))
            .collect();
        fresh.sort_unstable();
        fresh.dedup();
        let added = !fresh.is_empty();
        selected.messages.extend(fresh.iter().copied());
        selected.modseq = selected.modseq.max(modseq);
        let exists = u32::try_from(selected.messages.len()).unwrap_or(u32::MAX);
        let fresh: HashSet<MessageId> = fresh.into_iter().map(|f| f.1).collect();
        // Флаги изменённых писем, известных клиенту (новые он запросит сам).
        let seq_of: HashMap<MessageId, usize> = selected
            .messages
            .iter()
            .enumerate()
            .map(|(i, (_, id))| (*id, i))
            .collect();
        let updated: Vec<MessageId> = delta
            .updated
            .into_iter()
            .filter(|id| seq_of.contains_key(id) && !fresh.contains(id))
            .collect();
        if added {
            self.data(Data::Exists(exists));
        }
        if updated.is_empty() {
            return Ok(());
        }
        let scope = self.scope().ok_or(Error::Forbidden)?;
        let batch = self.ctx.config.limits.fetch_batch.max(1);
        for chunk in updated.chunks(batch) {
            let list: Vec<Message> = self
                .ctx
                .deps
                .messages
                .list(
                    scope,
                    mailbox,
                    MessageQuery::Ids(chunk.to_vec()),
                    chunk.len(),
                )
                .await?;
            let mut list: Vec<(usize, Message)> = list
                .into_iter()
                .filter_map(|m| seq_of.get(&m.id).map(|&i| (i, m)))
                .collect();
            list.sort_unstable_by_key(|(i, _)| *i);
            for (i, m) in list {
                let mut items = vec![
                    MessageDataItem::Flags(crate::fetch::imap_flags(&m.flags)),
                    MessageDataItem::Uid(proto::nz(m.uid.0)),
                ];
                if self.condstore {
                    items.push(modseq_item(m.modseq));
                }
                if let Ok(items) = Vec1::try_from(items) {
                    let seq = proto::nz(u32::try_from(i + 1).unwrap_or(u32::MAX));
                    self.fetch_data(seq, items, &[]);
                }
            }
            self.maybe_flush().await?;
        }
        Ok(())
    }

    async fn subscribe_bus(&self) -> Option<EventStream> {
        let scope = self.scope()?;
        match self.ctx.deps.bus.subscribe(scope).await {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::debug!(error = %e, "imap: подписка на шину не удалась");
                None
            }
        }
    }

    /// `IDLE`: уведомления до `DONE`, таймаута или остановки узла.
    pub(crate) async fn idle(&mut self, tag: &Tag<'static>) -> Result<()> {
        // Подписка до `+`: изменения после ответа не теряются.
        let mut events = self.subscribe_bus().await;
        if let Err(e) = self.sync().await {
            tracing::debug!(error = %e, "imap: изменения папки не получены");
        }
        self.push(&Response::CommandContinuationRequest(
            CommandContinuationRequest::basic(None, "idling")
                .unwrap_or(CommandContinuationRequest::Base64(Cow::Borrowed(b""))),
        ));
        let deadline = tokio::time::sleep(self.ctx.config.timeouts.idle);
        tokio::pin!(deadline);
        let shutdown = self.ctx.shutdown.clone();
        let mut buf = vec![0u8; 4096];
        loop {
            if let Some(input) = self.reader.next(Expect::IdleDone) {
                match input {
                    Input::IdleDone => self.ok(tag, None, "IDLE terminated"),
                    _ => self.bad(Some(tag), "Expected DONE"),
                }
                return Ok(());
            }
            self.flush().await?;
            let Some(conn) = self.conn.as_mut() else {
                return Ok(());
            };
            let io = conn.io();
            let wake = tokio::select! {
                biased;
                _ = shutdown.cancelled() => Wake::Shutdown,
                _ = &mut deadline => Wake::Timeout,
                ev = async {
                    match events.as_mut() {
                        Some(s) => s.next().await,
                        None => std::future::pending().await,
                    }
                } => Wake::Event(ev),
                r = io.read(&mut buf) => Wake::Read(r),
            };
            match wake {
                Wake::Shutdown => {
                    self.bye(Some(proto::code("UNAVAILABLE")), "Server shutting down");
                    self.close = true;
                    return Ok(());
                }
                Wake::Timeout => {
                    self.bye(None, "IDLE for too long");
                    self.close = true;
                    return Ok(());
                }
                Wake::Event(Some(ev)) => {
                    let relevant = match (&ev.kind, self.selected.as_ref()) {
                        (EventKind::MailboxChanged { mailbox, modseq }, Some(s)) => {
                            *mailbox == s.mailbox && *modseq > s.modseq
                        }
                        _ => false,
                    };
                    if relevant && let Err(e) = self.sync().await {
                        tracing::debug!(error = %e, "imap: изменения папки не получены");
                    }
                }
                // Поток шины оборвался: переподписка и сверка с журналом (уведомления могли потеряться).
                Wake::Event(None) => {
                    events = self.subscribe_bus().await;
                    if let Err(e) = self.sync().await {
                        tracing::debug!(error = %e, "imap: изменения папки не получены");
                    }
                }
                Wake::Read(Ok(0)) => {
                    self.close = true;
                    return Ok(());
                }
                Wake::Read(Ok(n)) => self.reader.push(&buf[..n]),
                Wake::Read(Err(e)) => return Err(io_error(e)),
            }
        }
    }
}
