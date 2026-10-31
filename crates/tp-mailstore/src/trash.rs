//! Корзина и истечение срока (CONTRACTS.md#Trash, Retention, Quota (Ф1); SRS 4.7, 3.4).
//!
//! `delete_to_trash` и `restore` переносят письма хранилищем метаданных одной транзакцией; ссылки на объекты
//! сохраняются (SRS 3.4, удаление шаг 1). Срок хранения (`TrashItem::expires_at`) вычисляет по `PolicyResolver`
//! и фиксирует при переносе реализация `TrashStore`/`MessageStore`: истечение срок заново не вычисляет,
//! поэтому смена политики действует только на удалённое после неё.
//!
//! Истечение ([`Store::expire_trash`]; воркер [`Store::run_trash_expiry`] роли maintenance):
//! `TrashStore::expired` пакетами → `DecisionHooks::before_final_delete` (`FinalDelete::mailbox` — исходная папка)
//! → `MessageStore::expunge_final` → `MailboxChanged` корзины. `expunge_final` уменьшает счётчики ссылок частей
//! и скелета в своей транзакции (SRS 3.4, удаление шаг 2), поэтому `BlobIndex::release` здесь не вызывается —
//! иначе ссылки снялись бы дважды; объекты удаляет GC BLOB. Запрет хука (Legal Hold, политика хранения)
//! или его ошибка — `TrashStore::defer` на [`TrashConfig::defer`]: окончательное удаление необратимо, при сомнении
//! элемент сохраняется. Повтор прохода безопасен: удалённые письма `expunge_final` пропускает.
//!
//! Ограничение контракта Ф1: письмо, восстановленное между `expired` и `expunge_final`, будет удалено —
//! `expunge_final` не проверяет, что письмо ещё в корзине.

use std::future::Future;
use std::time::Duration;

use schemars::JsonSchema;
use serde::Deserialize;
use time::OffsetDateTime;
use tp_contracts::config::duration;
use tp_contracts::ext::{Decision, FinalDelete};
use tp_contracts::meta::{AccountScope, Inserted};
use tp_contracts::retention::{TrashItem, TrashObject};
use tp_contracts::{MessageId, ModSeq, Result};

use crate::Store;

/// Истечение срока корзины (секция `trash` конфигурации хранилища).
///
/// ```yaml
/// interval: 1h
/// batch: 500
/// defer: 1d
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TrashConfig {
    /// Пауза между проходами воркера.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub interval: Duration,
    /// Элементов за один запрос `TrashStore::expired`.
    pub batch: usize,
    /// Отсрочка окончательного удаления после запрета или ошибки точки принятия решений.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub defer: Duration,
}

impl Default for TrashConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(3600),
            batch: 500,
            defer: Duration::from_secs(86_400),
        }
    }
}

/// Итог прохода истечения.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TrashPass {
    /// Получено элементов с истёкшим сроком.
    pub expired: usize,
    /// Удалено окончательно.
    pub purged: usize,
    /// Отложено (запрет или ошибка точки принятия решений).
    pub deferred: usize,
    /// Не обработано из-за ошибки хранилища — повторятся в следующем проходе.
    pub failed: usize,
}

impl Store {
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

    /// Проход истечения на момент `now`: пакеты элементов с `expires_at <= now`, пока они есть и проход
    /// продвигается. Ошибка — только у запроса `expired`; ошибки отдельных элементов — в [`TrashPass::failed`].
    pub async fn expire_trash(&self, now: OffsetDateTime) -> Result<TrashPass> {
        let batch = self.cfg.trash.batch.max(1);
        let mut pass = TrashPass::default();
        loop {
            let items = self.d.trash.expired(now, batch).await?;
            let full = items.len() >= batch;
            let done = pass.purged + pass.deferred;
            pass.expired += items.len();
            self.expire_batch(now, items, &mut pass).await;
            if !full || pass.purged + pass.deferred == done {
                return Ok(pass);
            }
        }
    }

    /// Воркер истечения: проход каждые [`TrashConfig::interval`] до завершения `shutdown`.
    pub async fn run_trash_expiry(&self, shutdown: impl Future<Output = ()>) {
        let period = self.cfg.trash.interval.max(Duration::from_millis(1));
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => return,
                _ = tick.tick() => match self.expire_trash(OffsetDateTime::now_utc()).await {
                    Ok(p) if p.expired > 0 => tracing::info!(
                        purged = p.purged,
                        deferred = p.deferred,
                        failed = p.failed,
                        "mailstore: истечение корзины"
                    ),
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "mailstore: истечение корзины не выполнено"),
                },
            }
        }
    }

    async fn expire_batch(&self, now: OffsetDateTime, items: Vec<TrashItem>, pass: &mut TrashPass) {
        let mut allowed: Vec<(AccountScope, Vec<MessageId>)> = Vec::new();
        for item in items {
            let TrashObject::Message(message) = item.object else {
                // Объекты, появившиеся позже Ф1, этот воркер не удаляет.
                pass.failed += 1;
                continue;
            };
            let op = FinalDelete {
                scope: item.scope,
                mailbox: item.origin,
                message,
                received_at: item.received_at,
            };
            match self.d.hooks.before_final_delete(&op).await {
                Ok(Decision::Deny(reason)) => {
                    tracing::info!(%reason, "mailstore: окончательное удаление отложено точкой принятия решений");
                    self.defer(now, &item, pass).await;
                }
                Ok(_) => match allowed.iter_mut().find(|(s, _)| *s == item.scope) {
                    Some((_, ids)) => ids.push(message),
                    None => allowed.push((item.scope, vec![message])),
                },
                Err(e) => {
                    tracing::warn!(error = %e, "mailstore: точка принятия решений не ответила, удаление отложено");
                    self.defer(now, &item, pass).await;
                }
            }
        }
        for (scope, ids) in allowed {
            match self.d.messages.expunge_final(scope, &ids).await {
                Ok(modseq) => {
                    pass.purged += ids.len();
                    let boxes = self.touched(scope, modseq, ids.len()).await;
                    self.notify(scope, boxes, modseq).await;
                }
                Err(e) => {
                    tracing::warn!(error = %e, "mailstore: окончательное удаление не выполнено");
                    pass.failed += ids.len();
                }
            }
        }
    }

    async fn defer(&self, now: OffsetDateTime, item: &TrashItem, pass: &mut TrashPass) {
        let until = time::Duration::try_from(self.cfg.trash.defer)
            .ok()
            .and_then(|d| now.checked_add(d));
        let Some(until) = until else {
            tracing::warn!("mailstore: срок отсрочки удаления вне диапазона");
            pass.failed += 1;
            return;
        };
        match self.d.trash.defer(item.scope, item.object, until).await {
            Ok(()) => pass.deferred += 1,
            Err(e) => {
                tracing::warn!(error = %e, "mailstore: отсрочка удаления не записана");
                pass.failed += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use futures::StreamExt;
    use tp_contracts::bus::{EventBus, EventKind, EventStream};
    use tp_contracts::ext::{DecisionHook, DecisionHooks};
    use tp_contracts::mailstore::{Delivery, IngestRequest, MailStore};
    use tp_contracts::meta::{Flags, MailboxStore, MessageStore, SpecialUse};
    use tp_contracts::retention::TrashStore;
    use tp_contracts::{MailboxId, TenantId};

    use super::*;
    use crate::testenv::{Env, stream};

    struct Fixture {
        env: Env,
        scope: AccountScope,
        inbox: MailboxId,
        trash: MailboxId,
        ids: Vec<MessageId>,
        now: OffsetDateTime,
    }

    async fn fixture(hooks: DecisionHooks, n: usize) -> Fixture {
        let env = Env::with_hooks(hooks);
        let scope = env.meta.account("a@example.org").await.expect("acc");
        let inbox = env
            .meta
            .mailbox_by_role(scope, SpecialUse::Inbox)
            .expect("inbox");
        let trash = env
            .meta
            .mailbox_by_role(scope, SpecialUse::Trash)
            .expect("trash");
        let raw = tp_testkit::corpus::corpus_mail("mixed-pdf")
            .expect("mail")
            .raw;
        let now = OffsetDateTime::now_utc();
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
                    received_at: now,
                })
                .await
                .expect("ingest");
            ids.push(done.delivered[0].message);
        }
        Fixture {
            env,
            scope,
            inbox,
            trash,
            ids,
            now,
        }
    }

    impl Fixture {
        /// Удаляет письма в корзину (срок — от момента удаления, не раньше `self.now`).
        async fn delete(&self, ids: &[MessageId]) {
            self.env
                .store
                .delete_to_trash(self.scope, ids)
                .await
                .expect("trash");
        }

        async fn item(&self, id: MessageId) -> Option<TrashItem> {
            TrashStore::list(&self.env.trash, self.scope, 0, 100)
                .await
                .expect("list")
                .into_iter()
                .find(|i| i.object == TrashObject::Message(id))
        }

        async fn exists(&self, id: MessageId) -> bool {
            MessageStore::get(&self.env.meta, self.scope, id)
                .await
                .expect("get")
                .is_some()
        }

        /// Счётчики ссылок объектов письма.
        async fn refs(&self, id: MessageId) -> Vec<u32> {
            let m = MessageStore::get(&self.env.meta, self.scope, id)
                .await
                .expect("get")
                .expect("msg");
            std::iter::once(m.skeleton)
                .chain(m.parts.iter().map(|p| p.blob))
                .map(|k| self.env.index.row(k.tenant, &k.hash).expect("row").refcnt)
                .collect()
        }
    }

    fn days(n: i64) -> time::Duration {
        time::Duration::days(n)
    }

    async fn drain(events: &mut EventStream) -> Vec<(MailboxId, ModSeq)> {
        let mut out = Vec::new();
        while let Ok(Some(ev)) =
            tokio::time::timeout(Duration::from_millis(50), events.next()).await
        {
            if let EventKind::MailboxChanged { mailbox, modseq } = ev.kind {
                out.push((mailbox, modseq));
            }
        }
        out
    }

    #[tokio::test]
    async fn expiry_releases_refs() {
        let f = fixture(DecisionHooks::default(), 2).await;
        assert!(f.refs(f.ids[1]).await.iter().all(|&r| r == 2));
        f.delete(&f.ids[..1]).await;
        assert!(
            f.refs(f.ids[1]).await.iter().all(|&r| r == 2),
            "корзина сохраняет ссылки"
        );

        let early = f.env.store.expire_trash(f.now + days(29)).await;
        assert_eq!(early, Ok(TrashPass::default()));
        assert!(f.exists(f.ids[0]).await);

        let mut events = f.env.bus.subscribe(f.scope).await.expect("sub");
        let pass = f
            .env
            .store
            .expire_trash(f.now + days(31))
            .await
            .expect("expire");
        assert_eq!(
            pass,
            TrashPass {
                expired: 1,
                purged: 1,
                ..TrashPass::default()
            }
        );
        assert!(!f.exists(f.ids[0]).await);
        assert!(
            f.refs(f.ids[1]).await.iter().all(|&r| r == 1),
            "ссылки сняты"
        );
        let st = f.env.meta.status(f.scope, f.trash).await.expect("st");
        assert_eq!(drain(&mut events).await, [(f.trash, st.highest_modseq)]);
        let left = TrashStore::list(&f.env.trash, f.scope, 0, 10).await;
        assert_eq!(left, Ok(vec![]));

        // Повтор безопасен.
        let again = f.env.store.expire_trash(f.now + days(31)).await;
        assert_eq!(again, Ok(TrashPass::default()));
    }

    /// Legal Hold: запрещает окончательное удаление и запоминает запросы.
    #[derive(Default)]
    struct Hold {
        calls: Mutex<Vec<FinalDelete>>,
    }

    #[async_trait::async_trait]
    impl DecisionHook for Hold {
        fn name(&self) -> &str {
            "hold"
        }

        async fn before_final_delete(&self, op: &FinalDelete) -> Result<Decision> {
            self.calls.lock().expect("lock").push(op.clone());
            Ok(Decision::Deny("legal hold".into()))
        }
    }

    #[tokio::test]
    async fn hook_deny_keeps_item() {
        let hold = Arc::new(Hold::default());
        let f = fixture(DecisionHooks::new(vec![hold.clone()]), 1).await;
        f.delete(&f.ids).await;
        let before = f.refs(f.ids[0]).await;
        let at = f.now + days(31);
        let pass = f.env.store.expire_trash(at).await.expect("expire");
        assert_eq!((pass.expired, pass.purged, pass.deferred), (1, 0, 1));
        assert!(f.exists(f.ids[0]).await, "запрет хука сохраняет письмо");
        assert_eq!(f.refs(f.ids[0]).await, before);
        let calls = hold.calls.lock().expect("lock").clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            (calls[0].scope, calls[0].mailbox, calls[0].message),
            (f.scope, f.inbox, f.ids[0])
        );
        let item = f.item(f.ids[0]).await.expect("item");
        assert_eq!(item.expires_at, at + days(1), "отсрочка по конфигурации");

        // До конца отсрочки элемент не запрашивается.
        let pass = f.env.store.expire_trash(at).await.expect("expire");
        assert_eq!(pass, TrashPass::default());
        assert_eq!(hold.calls.lock().expect("lock").len(), 1);
    }

    #[tokio::test]
    async fn retention_fixed_at_delete() {
        let f = fixture(DecisionHooks::default(), 2).await;
        f.delete(&f.ids[..1]).await;
        // Политика изменилась: действует только на удалённое после неё.
        f.env
            .meta
            .clone()
            .with_trash_retention(Duration::from_secs(86_400))
            .to_trash(f.scope, &f.ids[1..])
            .await
            .expect("trash");

        let pass = f
            .env
            .store
            .expire_trash(f.now + days(2))
            .await
            .expect("expire");
        assert_eq!(pass.purged, 1);
        assert!(f.exists(f.ids[0]).await);
        assert!(!f.exists(f.ids[1]).await);

        let pass = f
            .env
            .store
            .expire_trash(f.now + days(31))
            .await
            .expect("expire");
        assert_eq!(pass.purged, 1);
        assert!(!f.exists(f.ids[0]).await);
    }

    #[tokio::test]
    async fn restored_is_not_purged_and_batches_continue() {
        let f = fixture(DecisionHooks::default(), 3).await;
        f.delete(&f.ids).await;
        f.env
            .store
            .restore(f.scope, &f.ids[..1])
            .await
            .expect("restore");
        let mut cfg = crate::Config::default();
        cfg.trash.batch = 1;
        let store = crate::Store::with_config(f.env.store.d.clone(), cfg);
        let pass = store.expire_trash(f.now + days(31)).await.expect("expire");
        assert_eq!((pass.expired, pass.purged), (2, 2), "пакеты по одному");
        assert!(f.exists(f.ids[0]).await, "восстановленное не удаляется");
        assert!(!f.exists(f.ids[1]).await && !f.exists(f.ids[2]).await);
    }

    #[tokio::test]
    async fn worker_stops_on_shutdown() {
        let f = fixture(DecisionHooks::default(), 1).await;
        tokio::time::timeout(
            Duration::from_secs(5),
            f.env.store.run_trash_expiry(async {}),
        )
        .await
        .expect("worker stopped");
    }
}
