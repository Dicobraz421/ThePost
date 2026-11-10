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
//! Воркер не делает периодических проходов (SRS 2.8, таймер 4): один таймер на ближайший срок — первый элемент
//! `TrashStore::expired` по возрастанию `expires_at` на дальний момент. Срок сверяется при запуске, после каждого
//! прохода и по сигналам: таймер перевзводится, если новый срок раньше. Сигналы — системный `TrashChanged`
//! шины (`EventBus::subscribe_system`; публикуют `delete_to_trash` любого узла через `retention::trash_changed`
//! и `TrashStore::purge`), `Resync` этой подписки и сигнал в памяти процесса после фиксации `delete_to_trash`
//! этого [`Store`] (и его клонов). Подписка оформляется до сверки срока, поэтому сигнал между ними не теряется;
//! поток завершился — переподписка и сверка, подписка не оформлена — повтор не позже [`TrashConfig::retry`].
//! Ожидание не дольше [`TrashConfig::max_wait`]: по его истечении срок сверяется заново (без прохода, если ничего
//! не истекло) — страховка от потерянного сигнала (шина at-most-once). После ошибки хранилища — повтор не раньше
//! [`TrashConfig::retry`] (SRS 2.8, таймер 2).

use std::future::Future;
use std::time::Duration;

use futures::StreamExt;
use schemars::JsonSchema;
use serde::Deserialize;
use time::{OffsetDateTime, PrimitiveDateTime};
use tp_contracts::bus::{EventKind, EventStream};
use tp_contracts::config::duration;
use tp_contracts::ext::{Decision, FinalDelete};
use tp_contracts::meta::{AccountScope, Inserted};
use tp_contracts::retention::{TrashItem, TrashObject, trash_changed};
use tp_contracts::{MessageId, ModSeq, Result};

use crate::Store;

/// Истечение срока корзины (секция `trash` конфигурации хранилища).
///
/// ```yaml
/// max_wait: 1h
/// retry: 5m
/// batch: 500
/// defer: 1d
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TrashConfig {
    /// Наибольшее ожидание таймера: сверка ближайшего срока не реже (страховка от потерянного сигнала `TrashChanged`).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub max_wait: Duration,
    /// Пауза перед повтором после ошибки хранилища (элементы с истёкшим сроком не обработаны).
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub retry: Duration,
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
            max_wait: Duration::from_secs(3600),
            retry: Duration::from_secs(300),
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

/// Часы и таймер воркера истечения: в работе — системные ([`SystemTimer`]), в тестах — часы testkit.
pub(crate) trait Timer: Send + Sync {
    /// Текущий момент (UTC).
    fn now(&self) -> OffsetDateTime;
    /// Завершается не раньше момента `at`.
    fn sleep_until(&self, at: OffsetDateTime) -> impl Future<Output = ()> + Send;
}

/// Системные часы и таймер tokio.
struct SystemTimer;

impl Timer for SystemTimer {
    fn now(&self) -> OffsetDateTime {
        OffsetDateTime::now_utc()
    }

    fn sleep_until(&self, at: OffsetDateTime) -> impl Future<Output = ()> + Send {
        let wait = Duration::try_from(at - OffsetDateTime::now_utc()).unwrap_or(Duration::ZERO);
        tokio::time::sleep(wait)
    }
}

/// Следующий сигнал подписки; без подписки — никогда.
async fn next_event(events: &mut Option<EventStream>) -> Option<tp_contracts::bus::Event> {
    match events {
        Some(s) => s.next().await,
        None => std::future::pending().await,
    }
}

/// Момент позже любого срока: `TrashStore::expired` на него — элементы по возрастанию срока.
const FAR: OffsetDateTime = PrimitiveDateTime::MAX.assume_utc();

/// `at + d`; вне диапазона — `FAR`.
fn after(at: OffsetDateTime, d: Duration) -> OffsetDateTime {
    time::Duration::try_from(d)
        .ok()
        .and_then(|d| at.checked_add(d))
        .unwrap_or(FAR)
}

impl Store {
    pub(crate) async fn trash_impl(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
    ) -> Result<ModSeq> {
        let sources = self.mailboxes_of(scope, ids).await?;
        let modseq = self.d.messages.to_trash(scope, ids).await?;
        // После фиксации: воркер истечения этого узла сверит ближайший срок.
        self.trash_wake.notify_one();
        if !sources.is_empty() {
            // Воркеры истечения других узлов (и этого — по шине).
            if let Err(e) = self.d.bus.publish(trash_changed(scope)).await {
                tracing::warn!(error = %e, "mailstore: сигнал корзины не опубликован");
            }
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

    /// Воркер истечения до завершения `shutdown`: один таймер на ближайший срок, без периодических проходов.
    pub async fn run_trash_expiry(&self, shutdown: impl Future<Output = ()>) {
        self.trash_worker(&SystemTimer, shutdown).await;
    }

    pub(crate) async fn trash_worker(
        &self,
        timer: &impl Timer,
        shutdown: impl Future<Output = ()>,
    ) {
        tokio::pin!(shutdown);
        // После ошибки хранилища — не раньше этого момента (иначе истёкший необработанный элемент
        // взводил бы таймер в прошлое).
        let mut not_before: Option<OffsetDateTime> = None;
        // Системные сигналы шины; `None` — подписка не оформлена.
        let mut events: Option<EventStream> = None;
        loop {
            // До сверки срока: сигнал между сверкой и подпиской не теряется.
            if events.is_none() {
                events = match self.d.bus.subscribe_system().await {
                    Ok(s) => Some(s),
                    Err(e) => {
                        tracing::warn!(error = %e, "mailstore: подписка на сигналы корзины не оформлена");
                        None
                    }
                };
            }
            let due = match self.next_trash_expiry().await {
                Ok(next) => next.map(|at| not_before.map_or(at, |nb| at.max(nb))),
                Err(e) => {
                    tracing::warn!(error = %e, "mailstore: ближайший срок корзины не получен");
                    let at = after(timer.now(), self.cfg.trash.retry);
                    not_before = Some(at);
                    Some(at)
                }
            };
            // Не дольше `max_wait` (без подписки — `retry`): по истечении — только сверка срока.
            let wait = if events.is_some() {
                self.cfg.trash.max_wait
            } else {
                self.cfg.trash.max_wait.min(self.cfg.trash.retry)
            };
            let cap = after(timer.now(), wait);
            let (at, pass) = match due {
                Some(at) if at <= cap => (at, true),
                _ => (cap, false),
            };
            let sleep = timer.sleep_until(at);
            tokio::pin!(sleep);
            let fire = loop {
                tokio::select! {
                    () = &mut shutdown => return,
                    // Помещение в корзину этим узлом: сверить срок и перевзвести таймер.
                    () = self.trash_wake.notified() => break false,
                    ev = next_event(&mut events) => match ev.map(|e| e.kind) {
                        // Корзина изменилась (на любом узле) или сигналы потеряны: сверить срок.
                        Some(EventKind::TrashChanged { .. } | EventKind::Resync) => break false,
                        Some(_) => {}
                        // Поток завершился (потеря связи): переподписка и сверка.
                        None => {
                            events = None;
                            break false;
                        }
                    },
                    () = &mut sleep => break pass,
                }
            };
            if fire {
                let now = timer.now();
                not_before = match self.expire_trash(now).await {
                    Ok(p) => {
                        if p.expired > 0 {
                            tracing::info!(
                                purged = p.purged,
                                deferred = p.deferred,
                                failed = p.failed,
                                "mailstore: истечение корзины"
                            );
                        }
                        (p.failed > 0).then(|| after(now, self.cfg.trash.retry))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "mailstore: истечение корзины не выполнено");
                        Some(after(now, self.cfg.trash.retry))
                    }
                };
            }
        }
    }

    /// Ближайший срок истечения среди всех элементов корзины; корзины пусты — `None`.
    async fn next_trash_expiry(&self) -> Result<Option<OffsetDateTime>> {
        let first = self.d.trash.expired(FAR, 1).await?;
        Ok(first.first().map(|i| i.expires_at))
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

    use tp_contracts::bus::EventBus;
    use tp_contracts::ext::{DecisionHook, DecisionHooks};
    use tp_contracts::mailstore::{Delivery, IngestRequest, MailStore};
    use tp_contracts::meta::{Flags, MailboxStore, MessageStore, SpecialUse};
    use tp_contracts::retention::TrashStore;
    use tp_contracts::retention::trash_changed;
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

    /// Корзина, считающая проходы истечения (`expired` не на дальний момент).
    struct Counting {
        inner: tp_testkit::MemTrash,
        passes: Arc<Mutex<usize>>,
    }

    #[async_trait::async_trait]
    impl TrashStore for Counting {
        async fn list(
            &self,
            scope: AccountScope,
            offset: usize,
            limit: usize,
        ) -> Result<Vec<TrashItem>> {
            self.inner.list(scope, offset, limit).await
        }

        async fn list_hidden(
            &self,
            scope: AccountScope,
            offset: usize,
            limit: usize,
        ) -> Result<Vec<TrashItem>> {
            self.inner.list_hidden(scope, offset, limit).await
        }

        async fn restore(
            &self,
            scope: AccountScope,
            object: TrashObject,
        ) -> Result<tp_contracts::retention::Restored> {
            self.inner.restore(scope, object).await
        }

        async fn purge(
            &self,
            scope: AccountScope,
            ids: &[MessageId],
            hooks: &DecisionHooks,
        ) -> Result<tp_contracts::retention::PurgeOutcome> {
            self.inner.purge(scope, ids, hooks).await
        }

        async fn expired(&self, now: OffsetDateTime, limit: usize) -> Result<Vec<TrashItem>> {
            if now != FAR {
                *self.passes.lock().expect("lock") += 1;
            }
            self.inner.expired(now, limit).await
        }

        async fn defer(
            &self,
            scope: AccountScope,
            object: TrashObject,
            until: OffsetDateTime,
        ) -> Result<()> {
            self.inner.defer(scope, object, until).await
        }
    }

    /// Таймер на часах testkit: запоминает взведённые сроки.
    struct TestTimer {
        clock: tp_testkit::ManualClock,
        armed: Arc<Mutex<Vec<OffsetDateTime>>>,
    }

    impl Timer for TestTimer {
        fn now(&self) -> OffsetDateTime {
            tp_testkit::Clock::now(&self.clock)
        }

        fn sleep_until(&self, at: OffsetDateTime) -> impl Future<Output = ()> + Send {
            self.armed.lock().expect("lock").push(at);
            let clock = self.clock.clone();
            async move {
                while tp_testkit::Clock::now(&clock) < at {
                    tokio::time::sleep(Duration::from_millis(1)).await;
                }
            }
        }
    }

    async fn until(what: &str, mut cond: impl AsyncFnMut() -> bool) {
        let wait = async {
            while !cond().await {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .unwrap_or_else(|_| panic!("не дождались: {what}"));
    }

    /// Взведённые сроки без подряд идущих повторов (сверка по лишнему сигналу срок не меняет).
    fn distinct(armed: &Mutex<Vec<OffsetDateTime>>) -> Vec<OffsetDateTime> {
        let mut v = armed.lock().expect("lock").clone();
        v.dedup();
        v
    }

    /// Воркер на часах testkit поверх окружения с двумя письмами во «Входящих».
    struct Rig {
        env: Env,
        clock: tp_testkit::ManualClock,
        t0: OffsetDateTime,
        scope: AccountScope,
        ids: Vec<MessageId>,
        /// Хранилище воркера (проходы считаются).
        store: Store,
        passes: Arc<Mutex<usize>>,
        armed: Arc<Mutex<Vec<OffsetDateTime>>>,
        stop: tokio::sync::oneshot::Sender<()>,
        worker: tokio::task::JoinHandle<()>,
    }

    async fn rig(max_wait: Duration) -> Rig {
        let clock = tp_testkit::ManualClock::default();
        let t0 = tp_testkit::Clock::now(&clock);
        let env = Env::with_clock(clock.shared());
        let scope = env.meta.account("a@example.org").await.expect("acc");
        let inbox = env
            .meta
            .mailbox_by_role(scope, SpecialUse::Inbox)
            .expect("inbox");
        let raw = tp_testkit::corpus::corpus_mail("mixed-pdf")
            .expect("mail")
            .raw;
        let mut ids = Vec::new();
        for _ in 0..2 {
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
                    received_at: t0,
                })
                .await
                .expect("ingest");
            ids.push(done.delivered[0].message);
        }

        let passes = Arc::new(Mutex::new(0usize));
        let mut store = env.store.clone();
        store.d.trash = Arc::new(Counting {
            inner: env.trash.clone(),
            passes: passes.clone(),
        });
        store.cfg.trash.max_wait = max_wait;

        let armed = Arc::new(Mutex::new(Vec::new()));
        let timer = TestTimer {
            clock: clock.clone(),
            armed: armed.clone(),
        };
        let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
        let worker = {
            let store = store.clone();
            tokio::spawn(async move {
                store
                    .trash_worker(&timer, async {
                        let _ = stopped.await;
                    })
                    .await;
            })
        };
        Rig {
            env,
            clock,
            t0,
            scope,
            ids,
            store,
            passes,
            armed,
            stop,
            worker,
        }
    }

    impl Rig {
        fn passed(&self) -> usize {
            *self.passes.lock().expect("lock")
        }

        async fn exists(&self, id: MessageId) -> bool {
            MessageStore::get(&self.env.meta, self.scope, id)
                .await
                .expect("get")
                .is_some()
        }

        async fn shutdown(self) {
            self.stop.send(()).expect("stop");
            tokio::time::timeout(Duration::from_secs(5), self.worker)
                .await
                .expect("worker stopped")
                .expect("join");
        }
    }

    #[tokio::test]
    async fn worker_timer_on_nearest_expiry() {
        let max_wait = Duration::from_secs(365 * 86_400);
        let r = rig(max_wait).await;
        let (t0, ids) = (r.t0, r.ids.clone());
        // Тот же сигнал воркеру; политика изменилась — срок корзины 1 день.
        let mut short = r.store.clone();
        short.d.messages = Arc::new(
            r.env
                .meta
                .clone()
                .with_trash_retention(Duration::from_secs(86_400)),
        );

        let a_due = t0 + days(30);
        r.store
            .delete_to_trash(r.scope, &ids[..1])
            .await
            .expect("trash");
        until("таймер на срок первого", async || {
            distinct(&r.armed).last() == Some(&a_due)
        })
        .await;

        r.clock.set(t0 + days(1));
        let b_due = t0 + days(2);
        short
            .delete_to_trash(r.scope, &ids[1..])
            .await
            .expect("trash");
        until(
            "перевзвод на более ранний срок",
            async || distinct(&r.armed).last() == Some(&b_due),
        )
        .await;

        r.clock.set(b_due - time::Duration::seconds(1));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(r.passed(), 0, "до срока проходов нет");
        assert!(r.exists(ids[1]).await);

        r.clock.set(b_due);
        until("удаление второго в срок", async || {
            !r.exists(ids[1]).await
        })
        .await;
        until(
            "таймер снова на срок первого",
            async || distinct(&r.armed) == [a_due, b_due, a_due],
        )
        .await;
        assert_eq!(r.passed(), 1, "один проход на срок");
        assert!(r.exists(ids[0]).await);

        r.clock.set(a_due);
        until("удаление первого в срок", async || {
            !r.exists(ids[0]).await
        })
        .await;
        let cap = after(a_due, max_wait);
        until(
            "корзины пусты — таймер на max_wait",
            async || distinct(&r.armed) == [a_due, b_due, a_due, cap],
        )
        .await;
        assert_eq!(r.passed(), 2, "без промежуточных проходов");
        r.shutdown().await;
    }

    #[tokio::test]
    async fn delete_publishes_trash_changed() {
        let f = fixture(DecisionHooks::default(), 1).await;
        let mut sys = f.env.bus.subscribe_system().await.expect("sub");
        f.delete(&f.ids).await;
        let ev = tokio::time::timeout(Duration::from_secs(5), sys.next())
            .await
            .expect("signal")
            .expect("stream");
        assert_eq!(ev, trash_changed(f.scope));
        // Ничего не перенесено (письма нет) — сигнала нет.
        let _ = f
            .env
            .store
            .delete_to_trash(f.scope, &[MessageId::new()])
            .await;
        let none = tokio::time::timeout(Duration::from_millis(50), sys.next()).await;
        assert!(none.is_err(), "лишний сигнал: {none:?}");
    }

    #[tokio::test]
    async fn worker_follows_other_node() {
        let max_wait = Duration::from_secs(365 * 86_400);
        let r = rig(max_wait).await;
        let (t0, ids) = (r.t0, r.ids.clone());
        until("подписка воркера", async || {
            r.env.bus.subscribers() == 1
        })
        .await;
        until(
            "пустая корзина — таймер на max_wait",
            async || distinct(&r.armed) == [after(t0, max_wait)],
        )
        .await;

        // «Другой узел»: свой `Store` (свой сигнал в памяти) над теми же метаданными и шиной.
        let other = |secs| {
            let mut d = r.env.store.d.clone();
            d.messages = Arc::new(
                r.env
                    .meta
                    .clone()
                    .with_trash_retention(Duration::from_secs(secs)),
            );
            Store::new(d)
        };
        other(86_400)
            .delete_to_trash(r.scope, &ids[..1])
            .await
            .expect("trash");
        let a_due = t0 + days(1);
        until("перевзвод по TrashChanged", async || {
            distinct(&r.armed).last() == Some(&a_due)
        })
        .await;

        // Потеря связи: воркер переподписывается и снова получает сигналы.
        r.env.bus.disconnect();
        until("переподписка", async || {
            r.env.bus.subscribers() == 1
        })
        .await;
        other(600)
            .delete_to_trash(r.scope, &ids[1..])
            .await
            .expect("trash");
        let b_due = t0 + time::Duration::minutes(10);
        until(
            "перевзвод после переподписки",
            async || distinct(&r.armed).last() == Some(&b_due),
        )
        .await;

        r.clock.set(b_due);
        until("удаление в срок без max_wait", async || {
            !r.exists(ids[1]).await
        })
        .await;
        assert_eq!(r.passed(), 1);
        assert!(r.exists(ids[0]).await);
        r.shutdown().await;
    }

    #[tokio::test]
    async fn worker_wakes_within_max_wait() {
        let hour = Duration::from_secs(3600);
        let r = rig(hour).await;
        let t0 = r.t0;
        until(
            "пустая корзина — таймер на max_wait",
            async || distinct(&r.armed) == [after(t0, hour)],
        )
        .await;

        // «Другой узел»: помещение в корзину мимо этого `Store` — сигнала нет; срок — 10 минут.
        r.env
            .meta
            .clone()
            .with_trash_retention(Duration::from_secs(600))
            .to_trash(r.scope, &r.ids[..1])
            .await
            .expect("trash");
        let due = t0 + time::Duration::minutes(10);
        r.clock.set(due);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(r.exists(r.ids[0]).await, "без сигнала — до max_wait");
        assert_eq!(r.passed(), 0);

        // Часы — на `t0 + max_wait`, это раньше `due + max_wait`.
        r.clock.set(after(t0, hour));
        until(
            "удаление не позже срок + max_wait",
            async || !r.exists(r.ids[0]).await,
        )
        .await;
        assert_eq!(r.passed(), 1, "один проход после сверки");
        assert!(r.exists(r.ids[1]).await);
        r.shutdown().await;
    }
}
