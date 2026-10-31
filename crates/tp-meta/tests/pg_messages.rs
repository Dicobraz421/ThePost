//! META-04: MessageStore и ChangeLog на PostgreSQL.

mod common;

use std::collections::HashSet;
use std::sync::Arc;

use time::OffsetDateTime;
use tp_contracts::blob::BlobIndex;
use tp_contracts::meta::{
    AccountScope, AccountStore, ChangeLog, Flag, FlagOp, Flags, MailboxStore, MessageQuery,
    MessageStore, NewMailbox, NewMessage, PartRef, SpecialUse,
};
use tp_contracts::{BlobHash, BlobKey, Error, Gen, MailboxId, MessageId, ModSeq, Uid};
use tp_meta::PgMeta;

fn key(scope: AccountScope) -> BlobKey {
    let mut h = [0u8; 32];
    h[..16].copy_from_slice(uuid::Uuid::now_v7().as_bytes());
    h[16..].copy_from_slice(uuid::Uuid::now_v7().as_bytes());
    BlobKey {
        tenant: scope.tenant,
        hash: BlobHash(h),
        generation: Gen::INITIAL,
    }
}

fn message(scope: AccountScope, subject: &str) -> NewMessage {
    let mut msg = NewMessage {
        skeleton: key(scope),
        parts: vec![
            PartRef {
                part: "2".into(),
                blob: key(scope),
                size: 20,
            },
            PartRef {
                part: "1".into(),
                blob: key(scope),
                size: 10,
            },
        ],
        size: 100,
        received_at: OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("ts"),
        flags: [Flag::Keyword("$Forwarded".into())].into(),
        meta: Default::default(),
    };
    msg.meta.envelope.subject = Some(subject.into());
    msg.meta.thread_key = format!("t-{subject}");
    msg
}

async fn folder(meta: &PgMeta, scope: AccountScope, role: SpecialUse) -> MailboxId {
    MailboxStore::list(meta, scope)
        .await
        .expect("list")
        .into_iter()
        .find(|m| m.role == Some(role))
        .expect("folder")
        .id
}

async fn state(meta: &PgMeta, scope: AccountScope) -> ModSeq {
    meta.message_changes(scope, None, ModSeq::ZERO, 100_000)
        .await
        .expect("changes")
        .new_state
}

#[tokio::test]
async fn insert_get_list() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let inbox = folder(&meta, scope, SpecialUse::Inbox).await;
    let mut ins = Vec::new();
    for s in ["a", "b", "c"] {
        ins.push(
            meta.insert(scope, inbox, message(scope, s))
                .await
                .expect("insert"),
        );
    }
    assert_eq!(
        ins.iter().map(|i| i.uid).collect::<Vec<_>>(),
        vec![Uid(1), Uid(2), Uid(3)]
    );

    let got = MessageStore::get(&meta, scope, ins[0].id)
        .await
        .expect("get")
        .expect("some");
    let src = message(scope, "a");
    assert_eq!(
        (got.mailbox, got.uid, got.modseq),
        (inbox, Uid(1), ins[0].modseq)
    );
    assert_eq!((got.size, got.received_at), (src.size, src.received_at));
    assert_eq!(got.flags, src.flags);
    assert_eq!(got.meta, src.meta);
    assert_eq!(
        got.parts
            .iter()
            .map(|p| p.part.as_str())
            .collect::<Vec<_>>(),
        vec!["1", "2"]
    );
    assert_eq!(got.skeleton.tenant, scope.tenant);

    let all = MessageStore::list(&meta, scope, inbox, MessageQuery::All, 10)
        .await
        .expect("all");
    assert_eq!(all.len(), 3);
    let two = MessageStore::list(&meta, scope, inbox, MessageQuery::All, 2)
        .await
        .expect("limit");
    assert_eq!(
        two.iter().map(|m| m.uid).collect::<Vec<_>>(),
        vec![Uid(1), Uid(2)]
    );
    let ranges = MessageQuery::Uids(vec![Uid(1)..=Uid(1), Uid(3)..=Uid(9)]);
    let r = MessageStore::list(&meta, scope, inbox, ranges, 10)
        .await
        .expect("uids");
    assert_eq!(
        r.iter().map(|m| m.uid).collect::<Vec<_>>(),
        vec![Uid(1), Uid(3)]
    );
    let r = MessageStore::list(&meta, scope, inbox, MessageQuery::Ids(vec![ins[1].id]), 10)
        .await
        .expect("ids");
    assert_eq!(r.len(), 1);
    let r = MessageStore::list(
        &meta,
        scope,
        inbox,
        MessageQuery::ChangedSince(ins[1].modseq),
        10,
    )
    .await
    .expect("changed");
    assert_eq!(r.iter().map(|m| m.id).collect::<Vec<_>>(), vec![ins[2].id]);

    let ghost = MailboxId::new();
    assert_eq!(
        MessageStore::list(&meta, scope, ghost, MessageQuery::All, 10).await,
        Err(Error::NotFound)
    );
    assert_eq!(
        meta.insert(scope, ghost, message(scope, "x")).await,
        Err(Error::NotFound)
    );
    let mut bad = message(scope, "x");
    bad.flags = [Flag::Keyword("a b".into())].into();
    assert!(matches!(
        meta.insert(scope, inbox, bad).await,
        Err(Error::Invalid(_))
    ));
    let st = meta.status(scope, inbox).await.expect("status");
    assert_eq!(
        (st.messages, st.unseen, st.size, st.uid_next),
        (3, 3, 300, Uid(4))
    );
    assert_eq!(st.highest_modseq, ins[2].modseq);
}

#[tokio::test]
async fn concurrent_insert_keeps_uid_and_modseq_monotonic() {
    const TASKS: usize = 16;
    const PER_TASK: usize = 10;
    let Some(meta) = common::meta().await else {
        return;
    };
    let meta = Arc::new(meta);
    let scope = common::account(&meta).await;
    let inbox = folder(&meta, scope, SpecialUse::Inbox).await;
    let other = meta
        .create(
            scope,
            NewMailbox {
                parent: None,
                name: "Other".into(),
                role: None,
            },
        )
        .await
        .expect("create")
        .id;

    let mut tasks = Vec::new();
    for t in 0..TASKS {
        let meta = meta.clone();
        let target = if t % 4 == 0 { other } else { inbox };
        tasks.push(tokio::spawn(async move {
            let mut out = Vec::new();
            for i in 0..PER_TASK {
                let ins = meta
                    .insert(scope, target, message(scope, &format!("{t}-{i}")))
                    .await
                    .expect("insert");
                out.push((target, ins));
            }
            out
        }));
    }
    let mut all = Vec::new();
    for t in tasks {
        let out = t.await.expect("join");
        // В пределах одной задачи вставки упорядочены: UID и ModSeq растут.
        for w in out.windows(2) {
            assert!(w[1].1.modseq > w[0].1.modseq);
            if w[0].0 == w[1].0 {
                assert!(w[1].1.uid > w[0].1.uid);
            }
        }
        all.extend(out);
    }
    let modseqs: HashSet<_> = all.iter().map(|(_, i)| i.modseq).collect();
    assert_eq!(
        modseqs.len(),
        all.len(),
        "ModSeq уникальны в учётной записи"
    );

    for mb in [inbox, other] {
        let mut ins: Vec<_> = all
            .iter()
            .filter(|(m, _)| *m == mb)
            .map(|(_, i)| *i)
            .collect();
        ins.sort_by_key(|i| i.uid);
        let expected: Vec<Uid> = (1..=ins.len() as u32).map(Uid).collect();
        assert_eq!(ins.iter().map(|i| i.uid).collect::<Vec<_>>(), expected);
        // Больший UID — больший ModSeq: порядок выдачи совпадает.
        assert!(ins.windows(2).all(|w| w[1].modseq > w[0].modseq));
        let st = meta.status(scope, mb).await.expect("status");
        assert_eq!(st.uid_next, Uid(ins.len() as u32 + 1));
        assert_eq!(st.highest_modseq, ins.last().expect("last").modseq);
    }

    let ch = meta
        .message_changes(scope, None, ModSeq::ZERO, 100_000)
        .await
        .expect("changes");
    assert_eq!(ch.created.len(), TASKS * PER_TASK);
    assert!(!ch.has_more);
}

#[tokio::test]
async fn flags_and_condstore() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let inbox = folder(&meta, scope, SpecialUse::Inbox).await;
    let a = meta
        .insert(scope, inbox, message(scope, "a"))
        .await
        .expect("a");
    let b = meta
        .insert(scope, inbox, message(scope, "b"))
        .await
        .expect("b");
    let seen: Flags = [Flag::Seen].into();

    let up = meta
        .set_flags(scope, &[a.id, b.id], FlagOp::Add(seen.clone()), None)
        .await
        .expect("flags");
    assert_eq!(up.changed.len(), 2);
    assert!(up.modseq > b.modseq);
    assert_eq!(meta.status(scope, inbox).await.expect("status").unseen, 0);
    let got = MessageStore::get(&meta, scope, a.id)
        .await
        .expect("get")
        .expect("a");
    assert_eq!(got.modseq, up.modseq);
    assert!(
        got.flags.contains(&Flag::Seen) && got.flags.contains(&Flag::Keyword("$Forwarded".into()))
    );

    // Без изменений — текущий ModSeq, ничего не изменено.
    let same = meta
        .set_flags(scope, &[a.id], FlagOp::Add(seen.clone()), None)
        .await
        .expect("noop");
    assert_eq!((same.modseq, same.changed.len()), (up.modseq, 0));

    // CONDSTORE: письмо изменено после unchanged_since — конфликт.
    let c = meta
        .set_flags(
            scope,
            &[a.id, MessageId::new()],
            FlagOp::Remove(seen.clone()),
            Some(b.modseq),
        )
        .await
        .expect("condstore");
    assert_eq!(c.conflicts, vec![a.id]);
    assert!(c.changed.is_empty());
    let r = meta
        .set_flags(
            scope,
            &[a.id],
            FlagOp::Replace(Flags::new()),
            Some(up.modseq),
        )
        .await
        .expect("replace");
    assert_eq!(r.changed, vec![a.id]);
    let got = MessageStore::get(&meta, scope, a.id)
        .await
        .expect("get")
        .expect("a");
    assert!(got.flags.is_empty());

    let ch = meta
        .message_changes(scope, Some(inbox), up.modseq, 100)
        .await
        .expect("changes");
    assert_eq!(
        ch.updated.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![a.id]
    );
    assert_eq!(ch.new_state, r.modseq);
}

#[tokio::test]
async fn move_trash_restore_expunge_and_changes() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let inbox = folder(&meta, scope, SpecialUse::Inbox).await;
    let trash = folder(&meta, scope, SpecialUse::Trash).await;
    let s0 = state(&meta, scope).await;
    let mb0 = meta
        .mailbox_changes(scope, ModSeq::ZERO, 100)
        .await
        .expect("mb")
        .new_state;

    let x = meta
        .create(
            scope,
            NewMailbox {
                parent: None,
                name: "X".into(),
                role: None,
            },
        )
        .await
        .expect("x")
        .id;
    let a = meta
        .insert(scope, inbox, message(scope, "a"))
        .await
        .expect("a");
    let b = meta
        .insert(scope, inbox, message(scope, "b"))
        .await
        .expect("b");
    let moved = meta.move_to(scope, &[b.id], x).await.expect("move");
    assert_eq!(moved.len(), 1);
    assert_ne!(moved[0].id, b.id);
    assert_eq!(moved[0].uid, Uid(1));
    assert_eq!(
        MessageStore::get(&meta, scope, b.id).await.expect("get"),
        None
    );
    let trashed_at = meta.to_trash(scope, &[a.id]).await.expect("trash");
    let in_trash = MessageStore::get(&meta, scope, a.id)
        .await
        .expect("get")
        .expect("a");
    assert_eq!((in_trash.mailbox, in_trash.modseq), (trash, trashed_at));
    // Повтор и отсутствующие письма — без изменений.
    assert_eq!(
        meta.to_trash(scope, &[a.id, MessageId::new()])
            .await
            .expect("again"),
        trashed_at
    );

    // Учётная запись: a создано (и перенесено), b создано и удалено, b' создано.
    let ch = meta
        .message_changes(scope, None, s0, 100)
        .await
        .expect("changes");
    let ids = |v: &[tp_contracts::meta::MessageChange]| v.iter().map(|c| c.id).collect::<Vec<_>>();
    assert_eq!(ids(&ch.created), vec![a.id, moved[0].id]);
    assert_eq!(ids(&ch.destroyed), vec![b.id]);
    assert!(ch.updated.is_empty());
    assert_eq!(
        ch.created[0].mailbox, trash,
        "значение — по последнему изменению"
    );
    // Папка «Входящие»: оба письма ушли.
    let ch = meta
        .message_changes(scope, Some(inbox), s0, 100)
        .await
        .expect("inbox");
    assert!(ch.created.is_empty() && ch.updated.is_empty());
    assert_eq!(ch.destroyed.len(), 2);
    assert!(ch.destroyed.iter().any(|c| c.id == a.id && c.uid == a.uid));
    let ch = meta
        .message_changes(scope, Some(trash), s0, 100)
        .await
        .expect("trash");
    assert_eq!(ids(&ch.created), vec![a.id]);
    let ch = meta
        .message_changes(scope, Some(x), s0, 100)
        .await
        .expect("x");
    assert_eq!(ids(&ch.created), vec![moved[0].id]);

    // Восстановление: тот же идентификатор, новый UID в исходной папке.
    let s1 = state(&meta, scope).await;
    assert!(matches!(
        meta.restore(scope, &[moved[0].id]).await,
        Err(Error::Invalid(_))
    ));
    let restored = meta.restore(scope, &[a.id]).await.expect("restore");
    assert_eq!(restored[0].id, a.id);
    assert_eq!(restored[0].uid, Uid(3));
    let ch = meta
        .message_changes(scope, None, s1, 100)
        .await
        .expect("changes");
    assert_eq!(ids(&ch.updated), vec![a.id]);
    let ch = meta
        .message_changes(scope, Some(inbox), s1, 100)
        .await
        .expect("inbox");
    assert_eq!(
        ch.created.iter().map(|c| (c.id, c.uid)).collect::<Vec<_>>(),
        vec![(a.id, Uid(3))]
    );

    // Удаление папки: письма — в корзину; восстановление без папки — во «Входящие».
    meta.delete(scope, x, false).await.expect("delete");
    let b2 = MessageStore::get(&meta, scope, moved[0].id)
        .await
        .expect("get")
        .expect("b'");
    assert_eq!(b2.mailbox, trash);
    let back = meta.restore(scope, &[moved[0].id]).await.expect("restore");
    let b2 = MessageStore::get(&meta, scope, back[0].id)
        .await
        .expect("get")
        .expect("b'");
    assert_eq!(b2.mailbox, inbox);

    // Только из корзины (CTR-18): письмо вне корзины, в том числе восстановленное, остаётся.
    let kept = meta
        .expunge_final(scope, &[a.id, back[0].id])
        .await
        .expect("skip");
    assert!(
        MessageStore::get(&meta, scope, a.id)
            .await
            .expect("get")
            .is_some()
    );
    assert!(
        MessageStore::get(&meta, scope, back[0].id)
            .await
            .expect("get")
            .is_some()
    );
    meta.to_trash(scope, &[a.id]).await.expect("trash");
    let gone = meta.expunge_final(scope, &[a.id]).await.expect("expunge");
    assert!(gone > kept);
    assert_eq!(
        MessageStore::get(&meta, scope, a.id).await.expect("get"),
        None
    );
    let ch = meta
        .message_changes(scope, None, s1, 100)
        .await
        .expect("changes");
    assert_eq!(ids(&ch.destroyed), vec![a.id]);
    assert_eq!(ch.new_state, gone);

    // Постраничная выдача: по одному изменению, без потерь.
    let mut since = s0;
    let mut seen = 0;
    loop {
        let page = meta
            .message_changes(scope, None, since, 1)
            .await
            .expect("page");
        seen += page.created.len() + page.updated.len() + page.destroyed.len();
        assert!(page.new_state > since || !page.has_more);
        since = page.new_state;
        if !page.has_more {
            break;
        }
    }
    assert_eq!(since, gone);
    assert!(seen >= 3);

    // Папки: создана и удалена X — только destroyed; «Входящие» изменены.
    let mb = meta
        .mailbox_changes(scope, mb0, 100)
        .await
        .expect("mailboxes");
    assert_eq!(mb.destroyed, vec![x]);
    assert!(mb.created.is_empty());
    assert!(mb.updated.contains(&inbox));

    assert_eq!(
        meta.message_changes(scope, None, ModSeq(gone.0 + 1), 10)
            .await,
        Err(Error::Conflict)
    );
}

#[tokio::test]
async fn copy_and_expunge_adjust_blob_refs() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let inbox = folder(&meta, scope, SpecialUse::Inbox).await;
    let sent = folder(&meta, scope, SpecialUse::Sent).await;
    let msg = message(scope, "a");
    let keys: Vec<BlobKey> = std::iter::once(msg.skeleton)
        .chain(msg.parts.iter().map(|p| p.blob))
        .collect();
    for k in &keys {
        meta.commit_new(scope.tenant, &k.hash, k.generation, 10, 1)
            .await
            .expect("blob");
    }
    let a = meta.insert(scope, inbox, msg).await.expect("insert");
    let refcnt = |meta: PgMeta, k: BlobKey| async move {
        let mut tx = tp_meta::schema::begin(meta.pool(), k.tenant)
            .await
            .expect("begin");
        let n: i64 =
            sqlx::query_scalar("SELECT refcnt FROM blobs WHERE tenant_id = $1 AND hash = $2")
                .bind(k.tenant.as_uuid())
                .bind(k.hash.as_bytes().as_slice())
                .fetch_one(&mut *tx)
                .await
                .expect("refcnt");
        n
    };

    let copies = meta.copy(scope, &[a.id], sent).await.expect("copy");
    assert_eq!(copies.len(), 1);
    assert_eq!(copies[0].uid, Uid(1));
    for k in &keys {
        assert_eq!(refcnt(meta.clone(), *k).await, 2);
    }
    let copy = MessageStore::get(&meta, scope, copies[0].id)
        .await
        .expect("get")
        .expect("copy");
    assert_eq!((copy.mailbox, copy.parts.len()), (sent, 2));
    assert_eq!(
        meta.copy(scope, &[MessageId::new()], sent).await,
        Err(Error::NotFound)
    );

    // Перенос не меняет счётчики; окончательное удаление снимает ссылки.
    let moved = meta
        .move_to(scope, &[copies[0].id], inbox)
        .await
        .expect("move");
    for k in &keys {
        assert_eq!(refcnt(meta.clone(), *k).await, 2);
    }
    meta.to_trash(scope, &[moved[0].id, a.id])
        .await
        .expect("trash");
    meta.expunge_final(scope, &[moved[0].id, a.id])
        .await
        .expect("expunge");
    for k in &keys {
        assert_eq!(refcnt(meta.clone(), *k).await, 0);
    }
    let far = OffsetDateTime::now_utc() + std::time::Duration::from_secs(3600);
    let gc = meta.gc_candidates(far, 100_000).await.expect("gc");
    assert!(keys.iter().all(|k| gc.contains(k)));
    assert_eq!(
        meta.quota(scope.tenant, scope.account)
            .await
            .expect("quota")
            .messages,
        0
    );
}

#[tokio::test]
async fn core_events_written_in_transaction() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let inbox = folder(&meta, scope, SpecialUse::Inbox).await;
    let a = meta
        .insert(scope, inbox, message(scope, "a"))
        .await
        .expect("a");
    meta.set_flags(scope, &[a.id], FlagOp::Add([Flag::Seen].into()), None)
        .await
        .expect("flags");
    meta.to_trash(scope, &[a.id]).await.expect("trash");

    let mut tx = tp_meta::schema::begin_system(meta.pool())
        .await
        .expect("begin");
    let rows: Vec<(i32, i64, serde_json::Value)> = sqlx::query_as(
        "SELECT partition, pos, kind FROM events WHERE account_id = $1 ORDER BY partition, pos",
    )
    .bind(scope.account.as_uuid())
    .fetch_all(&mut *tx)
    .await
    .expect("events");
    let kinds: Vec<String> = rows
        .iter()
        .map(|(_, _, k)| match k {
            serde_json::Value::String(s) => s.clone(),
            serde_json::Value::Object(o) => o.keys().next().cloned().unwrap_or_default(),
            _ => String::new(),
        })
        .collect();
    // 5 системных папок, доставка, флаги, корзина — в одном разделе по возрастанию позиции.
    assert_eq!(kinds.iter().filter(|k| *k == "MailboxCreated").count(), 5);
    assert_eq!(
        kinds
            .iter()
            .filter(|k| !k.starts_with("Mailbox"))
            .cloned()
            .collect::<Vec<_>>(),
        vec!["Delivered", "FlagsChanged", "Trashed"]
    );
    assert!(rows.windows(2).all(|w| w[0].0 == w[1].0 && w[1].1 > w[0].1));
}

#[tokio::test]
async fn cascade_delete_moves_subtree_to_trash() {
    let Some(meta) = common::meta().await else {
        return;
    };
    let scope = common::account(&meta).await;
    let inbox = folder(&meta, scope, SpecialUse::Inbox).await;
    let trash = folder(&meta, scope, SpecialUse::Trash).await;
    let mk = |parent, name: &str, role| NewMailbox {
        parent,
        name: name.into(),
        role,
    };
    let p = meta.create(scope, mk(None, "P", None)).await.expect("p").id;
    let a = meta
        .create(scope, mk(Some(p), "A", None))
        .await
        .expect("a")
        .id;
    let b = meta
        .create(scope, mk(Some(a), "B", None))
        .await
        .expect("b")
        .id;
    let ma = meta
        .insert(scope, a, message(scope, "a"))
        .await
        .expect("ma");
    let mb = meta
        .insert(scope, b, message(scope, "b"))
        .await
        .expect("mb");
    let s0 = meta
        .mailbox_changes(scope, ModSeq::ZERO, 100)
        .await
        .expect("mb")
        .new_state;

    // Без подтверждения — Conflict, ничего не меняется.
    assert_eq!(meta.delete(scope, p, false).await, Err(Error::Conflict));
    assert!(
        MailboxStore::get(&meta, scope, b)
            .await
            .expect("get")
            .is_some()
    );

    // Системная папка в поддереве — Forbidden даже с каскадом.
    let q = meta.create(scope, mk(None, "Q", None)).await.expect("q").id;
    meta.create(scope, mk(Some(q), "Archive", Some(SpecialUse::Archive)))
        .await
        .expect("archive");
    assert_eq!(meta.delete(scope, q, true).await, Err(Error::Forbidden));

    meta.delete(scope, p, true).await.expect("cascade");
    for id in [p, a, b] {
        assert_eq!(
            MailboxStore::get(&meta, scope, id).await.expect("get"),
            None
        );
    }
    for m in [ma.id, mb.id] {
        let msg = MessageStore::get(&meta, scope, m)
            .await
            .expect("get")
            .expect("msg");
        assert_eq!(msg.mailbox, trash);
    }
    let ch = meta.mailbox_changes(scope, s0, 100).await.expect("changes");
    let mut destroyed = ch.destroyed.clone();
    destroyed.sort();
    let mut expected = vec![p, a, b];
    expected.sort();
    assert_eq!(destroyed, expected);

    // Исходной папки нет — восстановление во «Входящие».
    let back = meta.restore(scope, &[mb.id]).await.expect("restore");
    let msg = MessageStore::get(&meta, scope, back[0].id)
        .await
        .expect("get")
        .expect("msg");
    assert_eq!(msg.mailbox, inbox);
}
