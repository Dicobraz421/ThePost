//! Флаги, копирование, перемещение, корзина (CONTRACTS.md#MailStore, #EventBus).
//!
//! Изменение выполняет хранилище метаданных одной транзакцией (новый `ModSeq`, журнал изменений, событие
//! ядра; у `copy` — и ссылки на объекты частей и скелета, поэтому здесь `BlobIndex` не вызывается: иначе
//! ссылки учлись бы дважды). После фиксации — `MailboxChanged` для каждой затронутой папки с `ModSeq`
//! изменения. Затронутые папки берутся из журнала изменений (`ModSeq` изменения — один на операцию);
//! исходные папки переноса в корзину журнал учётной записи не хранит — они читаются до операции.
//!
//! Корзина (`delete_to_trash`, `restore`) — здесь без воркера истечения; срок хранения, окончательное
//! удаление и `before_final_delete` — MS-05.

use tp_contracts::meta::{AccountScope, FlagOp, FlagsUpdate, Inserted};
use tp_contracts::{MailboxId, MessageId, ModSeq, Result};

use crate::Store;

impl Store {
    pub(crate) async fn set_flags_impl(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        op: FlagOp,
        unchanged_since: Option<ModSeq>,
    ) -> Result<FlagsUpdate> {
        let update = self
            .d
            .messages
            .set_flags(scope, ids, op, unchanged_since)
            .await?;
        if !update.changed.is_empty() {
            let boxes = self
                .touched(scope, update.modseq, update.changed.len())
                .await;
            self.notify(scope, boxes, update.modseq).await;
        }
        Ok(update)
    }

    pub(crate) async fn copy_impl(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        to: MailboxId,
    ) -> Result<Vec<Inserted>> {
        let inserted = self.d.messages.copy(scope, ids, to).await?;
        if let Some(modseq) = inserted.first().map(|i| i.modseq) {
            self.notify(scope, [to], modseq).await;
        }
        Ok(inserted)
    }

    pub(crate) async fn move_impl(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        to: MailboxId,
    ) -> Result<Vec<Inserted>> {
        let inserted = self.d.messages.move_to(scope, ids, to).await?;
        if let Some(modseq) = inserted.first().map(|i| i.modseq) {
            // Исходные письма удалены в той же транзакции: журнал хранит их прежние папки.
            let boxes = self.touched(scope, modseq, inserted.len() * 2).await;
            self.notify(scope, boxes.into_iter().chain([to]), modseq)
                .await;
        }
        Ok(inserted)
    }

    pub(crate) async fn trash_impl(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
    ) -> Result<ModSeq> {
        let sources = self.mailboxes_of(scope, ids).await?;
        let modseq = self.d.messages.to_trash(scope, ids).await?;
        if !sources.is_empty() {
            let trash = self.touched(scope, modseq, ids.len()).await;
            self.notify(scope, sources.into_iter().chain(trash), modseq)
                .await;
        }
        Ok(modseq)
    }

    pub(crate) async fn restore_impl(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
    ) -> Result<Vec<Inserted>> {
        // Все письма — в корзине (иначе `Invalid`): исходная папка одна.
        let trash = self.mailboxes_of(scope, &ids[..ids.len().min(1)]).await?;
        let inserted = self.d.messages.restore(scope, ids).await?;
        if let Some(modseq) = inserted.first().map(|i| i.modseq) {
            let boxes = self.touched(scope, modseq, inserted.len()).await;
            self.notify(scope, trash.into_iter().chain(boxes), modseq)
                .await;
        }
        Ok(inserted)
    }

    /// Папки писем изменения `modseq` по журналу изменений учётной записи. Ошибка журнала — без уведомления
    /// (шина at-most-once; клиенты сверяются по `ModSeq`).
    async fn touched(&self, scope: AccountScope, modseq: ModSeq, hint: usize) -> Vec<MailboxId> {
        let since = ModSeq(modseq.0.saturating_sub(1));
        match self
            .d
            .changes
            .message_changes(scope, None, since, hint.max(1))
            .await
        {
            Ok(c) => c
                .created
                .iter()
                .chain(&c.updated)
                .chain(&c.destroyed)
                .filter(|ch| ch.modseq == modseq)
                .map(|ch| ch.mailbox)
                .collect(),
            Err(e) => {
                tracing::warn!(error = %e, "mailstore: папки изменения не получены из журнала");
                Vec::new()
            }
        }
    }

    /// Текущие папки писем (отсутствующие пропускаются).
    async fn mailboxes_of(&self, scope: AccountScope, ids: &[MessageId]) -> Result<Vec<MailboxId>> {
        let mut out = Vec::new();
        for id in ids {
            if let Some(m) = self.d.messages.get(scope, *id).await?
                && !out.contains(&m.mailbox)
            {
                out.push(m.mailbox);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt;
    use tp_contracts::bus::{EventBus, EventKind, EventStream};
    use tp_contracts::mailstore::{Delivery, IngestRequest, MailStore};
    use tp_contracts::meta::SpecialUse;
    use tp_contracts::meta::{Flag, Flags, MailboxStore, MessageQuery, MessageStore, NewMailbox};
    use tp_contracts::{Error, TenantId};
    use tp_testkit::blob::read_all;
    use tp_testkit::{Clock, SystemClock};

    use super::*;
    use crate::testenv::{Env, stream};

    struct Fixture {
        env: Env,
        scope: AccountScope,
        inbox: MailboxId,
        ids: Vec<MessageId>,
        raw: Vec<u8>,
    }

    async fn fixture(n: usize) -> Fixture {
        let env = Env::new();
        let scope = env.meta.account("a@example.org").await.expect("acc");
        let inbox = env
            .meta
            .mailbox_by_role(scope, SpecialUse::Inbox)
            .expect("inbox");
        let raw = tp_testkit::corpus::corpus_mail("mixed-pdf")
            .expect("mail")
            .raw;
        let mut ids = Vec::new();
        for _ in 0..n {
            let done = env
                .store
                .ingest(IngestRequest {
                    tenant: TenantId::DEFAULT,
                    mail_from: String::new(),
                    raw: stream(&raw, 4096),
                    deliveries: vec![Delivery {
                        account: scope.account,
                        mailbox: inbox,
                        extra_headers: vec![],
                        flags: Flags::new(),
                    }],
                    received_at: SystemClock.now(),
                })
                .await
                .expect("ingest");
            ids.push(done.delivered[0].message);
        }
        Fixture {
            env,
            scope,
            inbox,
            ids,
            raw,
        }
    }

    /// Уведомления, пришедшие за короткое время: папка и `ModSeq`, по возрастанию.
    async fn drain(events: &mut EventStream) -> Vec<(MailboxId, ModSeq)> {
        let mut out = Vec::new();
        while let Ok(Some(ev)) =
            tokio::time::timeout(Duration::from_millis(50), events.next()).await
        {
            if let EventKind::MailboxChanged { mailbox, modseq } = ev.kind {
                out.push((mailbox, modseq));
            }
        }
        out.sort();
        out
    }

    async fn message(f: &Fixture, id: MessageId) -> tp_contracts::meta::Message {
        MessageStore::get(&f.env.meta, f.scope, id)
            .await
            .expect("get")
            .expect("msg")
    }

    fn sorted(mut v: Vec<(MailboxId, ModSeq)>) -> Vec<(MailboxId, ModSeq)> {
        v.sort();
        v
    }

    async fn folder(f: &Fixture, name: &str) -> MailboxId {
        f.env
            .meta
            .create(
                f.scope,
                NewMailbox {
                    parent: None,
                    name: name.into(),
                    role: None,
                },
            )
            .await
            .expect("create")
            .id
    }

    #[tokio::test]
    async fn set_flags_publishes_modseq() {
        let f = fixture(3).await;
        let mut events = f.env.bus.subscribe(f.scope).await.expect("sub");
        let before = f.env.meta.status(f.scope, f.inbox).await.expect("st");
        let seen: Flags = [Flag::Seen].into();
        let upd = f
            .env
            .store
            .set_flags(f.scope, &f.ids[..2], FlagOp::Add(seen.clone()), None)
            .await
            .expect("flags");
        assert_eq!(upd.changed.len(), 2);
        assert!(upd.modseq > before.highest_modseq);
        assert_eq!(drain(&mut events).await, [(f.inbox, upd.modseq)]);

        // Ничего не изменилось — уведомления нет.
        let same = f
            .env
            .store
            .set_flags(f.scope, &f.ids[..2], FlagOp::Add(seen), None)
            .await
            .expect("flags");
        assert!(same.changed.is_empty());
        assert_eq!(same.modseq, upd.modseq);
        assert!(drain(&mut events).await.is_empty());

        // CONDSTORE: письма изменены после `unchanged_since` — конфликт.
        let cond = f
            .env
            .store
            .set_flags(
                f.scope,
                &f.ids,
                FlagOp::Add([Flag::Flagged].into()),
                Some(ModSeq(upd.modseq.0 - 1)),
            )
            .await
            .expect("flags");
        assert_eq!(cond.conflicts.len(), 2);
        assert_eq!(cond.changed, [f.ids[2]]);
        assert_eq!(drain(&mut events).await, [(f.inbox, cond.modseq)]);
        let msg = message(&f, f.ids[2]).await;
        assert_eq!(msg.modseq, cond.modseq);
    }

    #[tokio::test]
    async fn copy_adds_refs() {
        let f = fixture(2).await;
        let to = folder(&f, "Archive").await;
        let msg = message(&f, f.ids[0]).await;
        let refs = |f: &Fixture| {
            std::iter::once(msg.skeleton)
                .chain(msg.parts.iter().map(|p| p.blob))
                .map(|k| f.env.index.row(k.tenant, &k.hash).expect("row").refcnt)
                .collect::<Vec<_>>()
        };
        let before = refs(&f);
        assert!(before.iter().all(|&r| r == 2), "{before:?}");
        let mut events = f.env.bus.subscribe(f.scope).await.expect("sub");
        let ins = f.env.store.copy(f.scope, &f.ids, to).await.expect("copy");
        assert_eq!(ins.len(), 2);
        assert!(
            ins.iter().all(|i| !f.ids.contains(&i.id)),
            "новые MessageId"
        );
        assert_eq!(refs(&f), before.iter().map(|r| r + 2).collect::<Vec<_>>());
        assert_eq!(drain(&mut events).await, [(to, ins[0].modseq)]);
        let got = read_all(
            f.env
                .store
                .fetch_raw(f.scope, ins[0].id, None)
                .await
                .expect("raw"),
        )
        .await
        .expect("read");
        assert!(got.as_ref() == f.raw.as_slice());
        assert_eq!(f.env.store.copy(f.scope, &[], to).await, Ok(vec![]));
        assert!(drain(&mut events).await.is_empty());
    }

    #[tokio::test]
    async fn move_notifies_both_folders() {
        let f = fixture(2).await;
        let to = folder(&f, "Projects").await;
        let msg = message(&f, f.ids[0]).await;
        let part = msg.parts[0].blob;
        let before = f
            .env
            .index
            .row(part.tenant, &part.hash)
            .expect("row")
            .refcnt;
        let mut events = f.env.bus.subscribe(f.scope).await.expect("sub");
        let ins = f
            .env
            .store
            .move_to(f.scope, &f.ids, to)
            .await
            .expect("move");
        let modseq = ins[0].modseq;
        assert_eq!(
            drain(&mut events).await,
            sorted(vec![(f.inbox, modseq), (to, modseq)])
        );
        assert_eq!(
            f.env
                .index
                .row(part.tenant, &part.hash)
                .expect("row")
                .refcnt,
            before,
            "перенос не меняет ссылки"
        );
        let left = MessageStore::list(&f.env.meta, f.scope, f.inbox, MessageQuery::All, 10)
            .await
            .expect("list");
        assert!(left.is_empty());
        let st = f.env.meta.status(f.scope, to).await.expect("st");
        assert_eq!((st.messages, st.highest_modseq), (2, modseq));
        let got = read_all(
            f.env
                .store
                .fetch_raw(f.scope, ins[1].id, None)
                .await
                .expect("raw"),
        )
        .await
        .expect("read");
        assert!(got.as_ref() == f.raw.as_slice());
    }

    #[tokio::test]
    async fn trash_and_restore_notify() {
        let f = fixture(2).await;
        let trash = f
            .env
            .meta
            .mailbox_by_role(f.scope, SpecialUse::Trash)
            .expect("trash");
        let mut events = f.env.bus.subscribe(f.scope).await.expect("sub");
        let ms = f
            .env
            .store
            .delete_to_trash(f.scope, &f.ids[..1])
            .await
            .expect("trash");
        assert_eq!(
            drain(&mut events).await,
            sorted(vec![(f.inbox, ms), (trash, ms)])
        );
        let ins = f
            .env
            .store
            .restore(f.scope, &f.ids[..1])
            .await
            .expect("restore");
        assert_eq!(ins[0].id, f.ids[0], "корзина сохраняет MessageId");
        let ms = ins[0].modseq;
        assert_eq!(
            drain(&mut events).await,
            sorted(vec![(f.inbox, ms), (trash, ms)])
        );
        let err = f.env.store.restore(f.scope, &f.ids[..1]).await;
        assert!(matches!(err, Err(Error::Invalid(_))));
        assert!(drain(&mut events).await.is_empty());
    }
}
