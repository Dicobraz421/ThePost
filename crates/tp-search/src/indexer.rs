//! Индексатор — роль indexer (SRS 4.6): подписчик журнала событий ядра (CONTRACTS.md#Events).
//!
//! Раздел журнала обрабатывает один узел под лизой `events/indexer/{раздел}`: лизы и назначение разделов
//! узлам (`PartitionAssigner`) ведёт обработчик журнала событий ядра, общий для подписчиков
//! (`tp-meta`, `EventConsumer`), а [`Indexer`] — подписчик ([`EventSink`]) с именем [`SUBSCRIBER`].
//! Все события учётной записи — в одном разделе, поэтому индекс ящика изменяет один индексатор.
//!
//! Пачка событий переводится в изменения индекса: доставка, отправка, перенос в корзину — письмо
//! индексируется заново по текущему состоянию ([`DocumentSource`]); перенос — прежний идентификатор
//! удаляется, новый индексируется; окончательное удаление — письмо удаляется из индекса. Изменения
//! копятся и фиксируются коммитом индекса пакетом: набралось [`IndexerConfig::commit_docs`] или с первого
//! изменения прошло [`IndexerConfig::commit_interval`], и в конце пачки — до возврата `Ok`, после которого
//! журнал фиксирует позицию. Сбой до фиксации — пачка придёт снова; обработка идемпотентна.
//!
//! С хранилищем сегментов ([`Indexer::with_store`]) индекс перед записью сверяется с манифестом (нет на узле —
//! загружается из BlobStore), а после коммита выгружается и манифест записывается (SRCH-05). Манифест изменил
//! другой узел — индекс на узле забывается и `Conflict`: повтор пачки начнётся с загрузки.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::time::Instant;
use tp_contracts::config::duration;
use tp_contracts::events::{CoreEventKind, EventSink, LoggedEvent};
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, Error, MessageId, Result, TenantId};

use crate::index::{IndexDoc, IndexManager, IndexOp, blocking};
use crate::persist::IndexStore;

/// Имя подписчика журнала событий (ключ позиций и лиз разделов).
pub const SUBSCRIBER: &str = "indexer";

/// Индексатор (секция `search.indexer`; поля необязательны, неизвестное — ошибка).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct IndexerConfig {
    /// Изменений в пакете, после которых индекс фиксируется (не меньше одного).
    pub commit_docs: usize,
    /// Наибольшее время от первого изменения пакета до коммита индекса (`2s`, `500ms`): задержка
    /// появления письма в поиске после начала обработки его события.
    #[serde(with = "duration")]
    #[schemars(with = "duration::HumanDuration")]
    pub commit_interval: Duration,
}

impl Default for IndexerConfig {
    fn default() -> Self {
        Self {
            commit_docs: 1000,
            commit_interval: Duration::from_secs(2),
        }
    }
}

/// Содержимое письма для индекса: разбор письма и текст вложений (реализация — сборка приложения
/// поверх хранилища писем; в тестах — в памяти).
#[async_trait]
pub trait DocumentSource: Send + Sync + 'static {
    /// Документ письма по текущему состоянию; письма уже нет — `None` (удаляется из индекса).
    async fn document(&self, scope: AccountScope, message: MessageId) -> Result<Option<IndexDoc>>;
}

/// Подписчик журнала событий, обновляющий индексы ящиков.
pub struct Indexer {
    indexes: Arc<IndexManager>,
    source: Arc<dyn DocumentSource>,
    store: Option<Arc<IndexStore>>,
    cfg: IndexerConfig,
}

impl fmt::Debug for Indexer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Indexer")
            .field("indexes", &self.indexes)
            .field("cfg", &self.cfg)
            .finish_non_exhaustive()
    }
}

/// Накопленные изменения по учётным записям.
type Pending = HashMap<(TenantId, AccountId), Vec<IndexOp>>;

impl Indexer {
    /// Индексатор индексов `indexes` с содержимым писем из `source`.
    pub fn new(
        indexes: Arc<IndexManager>,
        source: Arc<dyn DocumentSource>,
        cfg: IndexerConfig,
    ) -> Self {
        Self {
            indexes,
            source,
            store: None,
            cfg,
        }
    }

    /// Индексы сохраняются в BlobStore с манифестом (SRCH-05).
    pub fn with_store(mut self, store: Arc<IndexStore>) -> Self {
        self.store = Some(store);
        self
    }

    /// Изменения индекса по событию.
    async fn ops(&self, scope: AccountScope, kind: &CoreEventKind) -> Result<Vec<IndexOp>> {
        Ok(match kind {
            CoreEventKind::Delivered { message }
            | CoreEventKind::Submitted {
                message: Some(message),
            }
            | CoreEventKind::Trashed { message } => vec![self.current(scope, *message).await?],
            CoreEventKind::Moved {
                from_message,
                message,
                ..
            } => {
                let mut ops = Vec::with_capacity(2);
                if from_message != message {
                    ops.push(IndexOp::Delete(*from_message));
                }
                ops.push(self.current(scope, *message).await?);
                ops
            }
            CoreEventKind::Purged { message } => vec![IndexOp::Delete(*message)],
            // Флаги и папки не индексируются; письма удалённой папки приходят событиями корзины.
            CoreEventKind::Submitted { message: None }
            | CoreEventKind::FlagsChanged { .. }
            | CoreEventKind::MailboxCreated
            | CoreEventKind::MailboxDeleted => Vec::new(),
        })
    }

    /// Письмо по текущему состоянию: документ или удаление, если письма уже нет.
    async fn current(&self, scope: AccountScope, message: MessageId) -> Result<IndexOp> {
        Ok(match self.source.document(scope, message).await? {
            Some(doc) => IndexOp::Upsert(Box::new(doc)),
            None => IndexOp::Delete(message),
        })
    }

    /// Коммит накопленных изменений: по коммиту на учётную запись.
    async fn flush(&self, pending: &mut Pending) -> Result<()> {
        for ((tenant, account), ops) in std::mem::take(pending) {
            self.commit(AccountScope { tenant, account }, ops).await?;
        }
        Ok(())
    }

    /// Коммит изменений индекса учётной записи и его выгрузка.
    async fn commit(&self, scope: AccountScope, ops: Vec<IndexOp>) -> Result<()> {
        let _gate = self.indexes.lock(scope).await;
        // Только удаления — индекс не создаётся, если его ещё нет.
        let create = !ops.iter().all(|op| matches!(op, IndexOp::Delete(_)));
        let index = match (self.indexes.cached(scope), &self.store) {
            // Индекс пишет только этот узел (лиза): выгруженный ранее не сверяется с манифестом.
            (Some(index), Some(_)) if index.synced().version > 0 => Some(index),
            (_, Some(store)) => store.sync(&self.indexes, scope, create).await?,
            (_, None) => {
                let m = self.indexes.clone();
                blocking(move || m.open(scope)).await?
            }
        };
        let index = match index {
            Some(index) => index,
            None if !create => return Ok(()),
            None => {
                let m = self.indexes.clone();
                blocking(move || m.open_or_create(scope)).await?
            }
        };
        let i = index.clone();
        blocking(move || i.apply(&ops)).await?;
        if let Some(store) = &self.store {
            match store.publish(scope, &index).await {
                Ok(_) => {}
                Err(Error::Conflict) => {
                    self.indexes.forget(scope);
                    return Err(Error::Conflict);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

#[async_trait]
impl EventSink for Indexer {
    fn name(&self) -> &str {
        SUBSCRIBER
    }

    async fn handle(&self, batch: &[LoggedEvent]) -> Result<()> {
        let mut pending = Pending::new();
        let (mut count, mut started) = (0, None::<Instant>);
        for logged in batch {
            let e = &logged.event;
            let scope = AccountScope {
                tenant: e.tenant,
                account: e.account,
            };
            let ops = self.ops(scope, &e.kind).await?;
            if !ops.is_empty() {
                started.get_or_insert_with(Instant::now);
                count += ops.len();
                pending
                    .entry((scope.tenant, scope.account))
                    .or_default()
                    .extend(ops);
            }
            if count >= self.cfg.commit_docs.max(1)
                || started.is_some_and(|t| t.elapsed() >= self.cfg.commit_interval)
            {
                self.flush(&mut pending).await?;
                (count, started) = (0, None);
            }
        }
        self.flush(&mut pending).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::tests::{manager, scope};
    use crate::{QueryConfig, Search};
    use std::collections::HashSet;
    use std::sync::Mutex;
    use time::OffsetDateTime;
    use tp_contracts::MailboxId;
    use tp_contracts::events::{CoreEvent, EventLog, EventPos, Partition};
    use tp_contracts::lease::LeaseStore;
    use tp_contracts::search::{SearchIndex, SearchSort, TextField, TextQuery};
    use tp_testkit::{MemEventLog, MemLease};

    /// Содержимое писем в памяти; задержка на письмо и наблюдатель — для теста задержки индексации.
    #[derive(Default)]
    struct MemSource {
        docs: Mutex<HashMap<MessageId, IndexDoc>>,
        delay: Duration,
        /// Перед выдачей письма: что уже видно в поиске.
        observe: Option<(Search, AccountScope)>,
        /// Письмо, момент выдачи, видимые в этот момент письма.
        seen: Mutex<Vec<(MessageId, Instant, HashSet<MessageId>)>>,
    }

    impl MemSource {
        fn put(&self, doc: IndexDoc) {
            self.docs.lock().expect("lock").insert(doc.message, doc);
        }

        fn remove(&self, message: MessageId) {
            self.docs.lock().expect("lock").remove(&message);
        }
    }

    #[async_trait]
    impl DocumentSource for MemSource {
        async fn document(&self, _: AccountScope, message: MessageId) -> Result<Option<IndexDoc>> {
            if let Some((search, scope)) = &self.observe {
                let all = TextQuery::And(Vec::new());
                let visible = search
                    .query(*scope, &[], &all, SearchSort::NewestFirst, 10_000)
                    .await?;
                self.seen.lock().expect("lock").push((
                    message,
                    Instant::now(),
                    visible.into_iter().collect(),
                ));
                tokio::time::sleep(self.delay).await;
            }
            Ok(self.docs.lock().expect("lock").get(&message).cloned())
        }
    }

    fn mail(mailbox: MailboxId, subject: &str) -> IndexDoc {
        IndexDoc {
            message: MessageId::new(),
            mailbox,
            received_at: OffsetDateTime::UNIX_EPOCH,
            subject: subject.to_owned(),
            from: String::new(),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            body: String::new(),
            attachments: String::new(),
            headers: Vec::new(),
        }
    }

    fn event(scope: AccountScope, mailbox: MailboxId, kind: CoreEventKind) -> CoreEvent {
        CoreEvent {
            tenant: scope.tenant,
            account: scope.account,
            mailbox: Some(mailbox),
            kind,
            at: OffsetDateTime::now_utc(),
        }
    }

    /// Обработка журнала, как у обработчика журнала событий: лиза раздела, чтение с позиции, пачка, фиксация.
    async fn drain(log: &MemEventLog, leases: &MemLease, sink: &dyn EventSink) -> usize {
        let mut handled = 0;
        for p in 0..log.partitions() {
            let p = Partition(p);
            let name = format!("events/{}/{}", sink.name(), p.0);
            let lease = leases
                .try_acquire(&name, "node-1", Duration::from_secs(30))
                .await
                .expect("lease")
                .expect("free");
            loop {
                let after = log.position(sink.name(), p).await.expect("position");
                let batch = log.read(p, after, 100).await.expect("read");
                let Some(last) = batch.last().map(|e| e.pos) else {
                    break;
                };
                sink.handle(&batch).await.expect("handle");
                log.commit(sink.name(), p, last).await.expect("commit");
                handled += batch.len();
            }
            leases.release(&lease).await.expect("release");
        }
        handled
    }

    async fn subjects(search: &Search, scope: AccountScope, text: &str) -> Vec<MessageId> {
        search
            .query(
                scope,
                &[],
                &TextQuery::term(TextField::Subject, text),
                SearchSort::NewestFirst,
                100,
            )
            .await
            .expect("query")
    }

    #[tokio::test]
    async fn events_update_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let indexes = Arc::new(manager(&dir));
        let search = Search::new(indexes.clone(), QueryConfig::default());
        let source = Arc::new(MemSource::default());
        let indexer = Indexer::new(indexes.clone(), source.clone(), IndexerConfig::default());
        assert_eq!(indexer.name(), SUBSCRIBER);
        let (log, leases) = (MemEventLog::new(4), MemLease::default());
        let (a, b) = (scope(), scope());
        let (inbox, trash, archive) = (MailboxId::new(), MailboxId::new(), MailboxId::new());

        let report = mail(inbox, "Квартальный отчёт");
        let invoice = mail(inbox, "Счёт на оплату");
        let gone = mail(inbox, "Удалено до индексации");
        let other = mail(inbox, "Отчёт другого ящика");
        for d in [&report, &invoice, &other] {
            source.put(d.clone());
        }
        for (scope, d) in [(a, &report), (a, &invoice), (a, &gone), (b, &other)] {
            log.append(event(
                scope,
                inbox,
                CoreEventKind::Delivered { message: d.message },
            ));
        }
        log.append(event(
            a,
            inbox,
            CoreEventKind::FlagsChanged {
                message: report.message,
            },
        ));
        assert_eq!(drain(&log, &leases, &indexer).await, 5);
        assert_eq!(subjects(&search, a, "отчеты").await, [report.message]);
        assert_eq!(subjects(&search, b, "отчеты").await, [other.message]);
        assert_eq!(subjects(&search, a, "удалено").await, []);

        // Перенос — новый идентификатор; корзина — та же запись в другой папке; окончательное удаление.
        let moved = IndexDoc {
            message: MessageId::new(),
            mailbox: archive,
            ..report.clone()
        };
        source.put(moved.clone());
        source.remove(report.message);
        log.append(event(
            a,
            archive,
            CoreEventKind::Moved {
                from_message: report.message,
                from_mailbox: inbox,
                message: moved.message,
            },
        ));
        source.put(IndexDoc {
            mailbox: trash,
            ..invoice.clone()
        });
        log.append(event(
            a,
            trash,
            CoreEventKind::Trashed {
                message: invoice.message,
            },
        ));
        assert_eq!(drain(&log, &leases, &indexer).await, 2);
        assert_eq!(subjects(&search, a, "отчет").await, [moved.message]);
        let in_trash = search
            .query(
                a,
                &[trash],
                &TextQuery::And(Vec::new()),
                SearchSort::NewestFirst,
                10,
            )
            .await
            .expect("trash");
        assert_eq!(in_trash, [invoice.message]);

        source.remove(invoice.message);
        log.append(event(
            a,
            trash,
            CoreEventKind::Purged {
                message: invoice.message,
            },
        ));
        // Удаление в ящике без индекса индекс не создаёт.
        let c = scope();
        log.append(event(
            c,
            inbox,
            CoreEventKind::Purged {
                message: MessageId::new(),
            },
        ));
        assert_eq!(drain(&log, &leases, &indexer).await, 2);
        assert_eq!(subjects(&search, a, "счет").await, []);
        assert!(!indexes.path(c).exists());

        // Повторная доставка пачки (не менее одного раза) — тот же результат.
        let before = indexes.open(a).expect("open").expect("index").docs();
        for p in 0..log.partitions() {
            let batch = log.read(Partition(p), EventPos(0), 1000).await;
            indexer.handle(&batch.expect("read")).await.expect("replay");
        }
        assert_eq!(
            indexes.open(a).expect("open").expect("index").docs(),
            before
        );
        assert_eq!(subjects(&search, a, "отчет").await, [moved.message]);
    }

    #[tokio::test]
    async fn indexing_delay_within_commit_interval() {
        let dir = tempfile::tempdir().expect("tempdir");
        let indexes = Arc::new(manager(&dir));
        let a = scope();
        let interval = Duration::from_millis(200);
        let step = Duration::from_millis(20);
        let source = Arc::new(MemSource {
            delay: step,
            observe: Some((Search::new(indexes.clone(), QueryConfig::default()), a)),
            ..MemSource::default()
        });
        let cfg = IndexerConfig {
            commit_docs: 10_000,
            commit_interval: interval,
        };
        let indexer = Indexer::new(indexes.clone(), source.clone(), cfg);
        let inbox = MailboxId::new();
        let log = MemEventLog::new(1);
        for i in 0..80 {
            let d = mail(inbox, &format!("Письмо {i}"));
            log.append(event(
                a,
                inbox,
                CoreEventKind::Delivered { message: d.message },
            ));
            source.put(d);
        }
        let batch = log
            .read(Partition(0), EventPos(0), 1000)
            .await
            .expect("read");
        indexer.handle(&batch).await.expect("handle");
        assert_eq!(indexes.open(a).expect("open").expect("index").docs(), 80);

        // Письмо видно в поиске не позже интервала коммита после выдачи, хотя вся пачка (80 × 20 мс)
        // обрабатывается дольше: коммит идёт пакетами по времени, а не в конце пачки. Задержка считается
        // в выдачах писем: каждая занимает не меньше `step`, поэтому за интервал их не больше
        // `interval / step`, ещё одна — на срабатывание коммита и одна — на наблюдение. Длительность самого
        // коммита (fsync) в счёт не входит: во время коммита выдач нет.
        let seen = source.seen.lock().expect("lock").clone();
        assert_eq!(seen.len(), 80);
        let window = (interval.as_millis() / step.as_millis()) as usize + 2;
        for (i, (message, ..)) in seen.iter().enumerate().take(seen.len() - window) {
            let j = seen
                .iter()
                .position(|(_, _, visible)| visible.contains(message))
                .expect("письмо не стало видно до конца пачки");
            assert!(
                j - i <= window,
                "письмо {i} стало видно через {} выдач",
                j - i
            );
        }
    }

    #[test]
    fn config_defaults_and_durations() {
        assert_eq!(
            crate::tests::from_pairs::<IndexerConfig>(&[]).expect("defaults"),
            IndexerConfig::default()
        );
        let c = crate::tests::from_pairs::<IndexerConfig>(&[("commit_interval", "500ms")])
            .expect("interval");
        assert_eq!(c.commit_interval, Duration::from_millis(500));
        assert!(crate::tests::from_pairs::<IndexerConfig>(&[("commit_interval", "5")]).is_err());
        assert!(crate::tests::from_pairs::<IndexerConfig>(&[("batch", "5")]).is_err());
    }

    #[tokio::test]
    async fn indexer_publishes_and_fails_over() {
        use crate::persist::tests::{node, store};
        let (_, _, store) = store();
        let source = Arc::new(MemSource::default());
        let indexer_on = |n: &crate::persist::tests::Node| {
            Indexer::new(n.indexes.clone(), source.clone(), IndexerConfig::default())
                .with_store(store.clone())
        };
        let (a, b) = (node(), node());
        let (on_a, on_b) = (indexer_on(&a), indexer_on(&b));
        let (log, leases) = (MemEventLog::new(2), MemLease::default());
        let scope = scope();
        let inbox = MailboxId::new();
        let deliver = |d: &IndexDoc| {
            source.put(d.clone());
            log.append(event(
                scope,
                inbox,
                CoreEventKind::Delivered { message: d.message },
            ));
        };

        let report = mail(inbox, "Квартальный отчёт");
        deliver(&report);
        assert_eq!(drain(&log, &leases, &on_a).await, 1);
        assert_eq!(store.manifest(scope).await.expect("manifest").docs(), 1);

        // Отказ узла A: узел B загружает индекс из BlobStore и продолжает индексацию.
        let invoice = mail(inbox, "Счёт на оплату");
        deliver(&invoice);
        assert_eq!(drain(&log, &leases, &on_b).await, 1);
        assert_eq!(subjects(&b.search, scope, "отчет").await, [report.message]);
        assert_eq!(subjects(&b.search, scope, "счет").await, [invoice.message]);
        assert_eq!(store.manifest(scope).await.expect("manifest").docs(), 2);

        // Узел A вернулся с устаревшим индексом: конфликт, повтор пачки начинается с загрузки.
        let late = mail(inbox, "Позднее письмо");
        deliver(&late);
        let mut retried = false;
        for p in 0..log.partitions() {
            let p = Partition(p);
            let after = log.position(SUBSCRIBER, p).await.expect("position");
            let batch = log.read(p, after, 100).await.expect("read");
            let Some(last) = batch.last().map(|e| e.pos) else {
                continue;
            };
            assert!(matches!(on_a.handle(&batch).await, Err(Error::Conflict)));
            on_a.handle(&batch).await.expect("retry");
            log.commit(SUBSCRIBER, p, last).await.expect("commit");
            retried = true;
        }
        assert!(retried);
        let m = store.manifest(scope).await.expect("manifest");
        assert_eq!(m.docs(), 3);
        assert_eq!(subjects(&a.search, scope, "письмо").await, [late.message]);
        assert_eq!(subjects(&a.search, scope, "счет").await, [invoice.message]);
    }
}
