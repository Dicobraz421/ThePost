//! Очередь исходящей почты (SRS 4.1, CONTRACTS.md#Queue (Ф1)): постановка и роль smtp-out.
//!
//! - [`Queue`] — [`Outbound`] поверх [`QueueStore`]: письмо — один объект BLOB (протокол записи SRS 3.4,
//!   одна ссылка на запись очереди), затем `QueueStore::enqueue`; ошибка постановки снимает ссылку.
//! - [`QueueWorker`] — обработка: `claim` записей со сроком попытки под лизу, доставка получателям по доменам
//!   через [`Transport`] (по умолчанию — [`send::MxTransport`] на `mail-send`), затем по каждому получателю:
//!   2xx — `complete(Delivered)`, 5xx или истёк `queue.expire` — `complete(Failed)`, 4xx и сбой соединения —
//!   `reschedule` через `queue.retry`. Когда запись завершена (`complete` = `true`), ссылка на объект снимается.
//!   Устаревший захват (`Conflict`: лиза истекла, запись взял другой узел) — запись пропускается.
//! - [`SmtpOutService`] — роль smtp-out: опрос очереди до остановки.
//!
//! Время попыток — из [`Now`] (тесты — управляемые часы), очередь сама времени не берёт.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, TryStreamExt};
use time::OffsetDateTime;
use tp_contracts::blob::{Acquire, BlobIndex, BlobStore, Commit};
use tp_contracts::queue::{Claimed, OutMessage, OutRcpt, QueueStore, RcptStatus};
use tp_contracts::role::{RoleCtx, RoleService};
use tp_contracts::{BlobKey, Error, Result, TenantId};

use crate::config::{QueueConfig, SmtpConfig};
use crate::deliver::Outbound;

pub mod send;

/// Источник текущего времени (UTC).
pub type Now = Arc<dyn Fn() -> OffsetDateTime + Send + Sync>;

/// Системные часы.
pub fn system_now() -> Now {
    Arc::new(OffsetDateTime::now_utc)
}

/// Хранилища очереди.
#[derive(Clone)]
pub struct QueueDeps {
    /// Записи очереди (`tp-meta`).
    pub store: Arc<dyn QueueStore>,
    /// Объекты писем.
    pub blobs: Arc<dyn BlobStore>,
    /// Учёт ссылок на объекты.
    pub index: Arc<dyn BlobIndex>,
    /// Часы.
    pub now: Now,
}

impl fmt::Debug for QueueDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueDeps").finish_non_exhaustive()
    }
}

impl QueueDeps {
    /// Загружает письмо одним объектом с одной ссылкой (SRS 3.4, шаги 2–3).
    async fn store_blob(&self, tenant: TenantId, data: Bytes) -> Result<BlobKey> {
        let hash = tp_mailstore::split::hash(&data);
        let mut generation = match self.index.acquire(tenant, &hash, 1).await? {
            Acquire::Existing { generation } => {
                return Ok(BlobKey {
                    tenant,
                    hash,
                    generation,
                });
            }
            Acquire::Absent { generation } => generation,
        };
        loop {
            let key = BlobKey {
                tenant,
                hash,
                generation,
            };
            self.blobs.put(&key, data.clone()).await?;
            match self
                .index
                .commit_new(tenant, &hash, generation, data.len() as u64, 1)
                .await?
            {
                Commit::Done { generation } => {
                    return Ok(BlobKey {
                        tenant,
                        hash,
                        generation,
                    });
                }
                Commit::Bumped { generation: next } => generation = next,
            }
        }
    }

    /// Снимает ссылку записи очереди на объект; ошибка — в журнал (объект найдёт сверка «сирот»).
    async fn release(&self, key: &BlobKey) {
        if let Err(e) = self
            .index
            .release(key.tenant, &key.hash, key.generation, 1)
            .await
        {
            tracing::error!(error = %e, blob = %key.hash, "smtp: ссылка очереди на письмо не снята");
        }
    }

    /// Ставит письмо `raw` в очередь получателям `rcpts`; первая попытка — сразу.
    pub async fn put(
        &self,
        tenant: TenantId,
        from: &str,
        rcpts: Vec<OutRcpt>,
        raw: Bytes,
    ) -> Result<()> {
        let size = raw.len() as u64;
        let blob = self.store_blob(tenant, raw).await?;
        let msg = OutMessage {
            tenant,
            blob,
            size,
            from: from.to_owned(),
            dsn: Default::default(),
            rcpts,
            next_attempt: (self.now)(),
        };
        if let Err(e) = self.store.enqueue(msg).await {
            self.release(&blob).await;
            return Err(e);
        }
        Ok(())
    }

    async fn read(&self, key: &BlobKey) -> Result<Bytes> {
        let chunks: Vec<Bytes> = self.blobs.get(key, None).await?.try_collect().await?;
        Ok(chunks.concat().into())
    }
}

/// Постановка в очередь исходящей почты ([`Outbound`] для доставки).
#[derive(Debug, Clone)]
pub struct Queue {
    deps: QueueDeps,
}

impl Queue {
    /// Очередь поверх хранилищ `deps`.
    pub fn new(deps: QueueDeps) -> Self {
        Self { deps }
    }
}

#[async_trait]
impl Outbound for Queue {
    async fn enqueue(
        &self,
        tenant: TenantId,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<()> {
        // Один адрес — один получатель записи (повтор адреса — `Invalid` очереди).
        let mut seen = std::collections::HashSet::new();
        let rcpts = rcpts
            .iter()
            .filter(|r| seen.insert(r.to_lowercase()))
            .map(OutRcpt::new)
            .collect();
        self.deps.put(tenant, mail_from, rcpts, raw).await
    }
}

/// Итог попытки доставки одному получателю.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RcptResult {
    /// Принято (2xx); ответ сервера.
    Delivered(String),
    /// Временная неудача (4xx, сбой соединения, DNS); причина.
    Temporary(String),
    /// Окончательный отказ (5xx); ответ сервера.
    Permanent(String),
}

/// Попытка отправки письма получателям одного домена.
#[derive(Debug, Clone, Copy)]
pub struct SendRequest<'a> {
    /// Секция `smtp.queue`.
    pub cfg: &'a QueueConfig,
    /// Имя узла для `EHLO`.
    pub helo: &'a str,
    /// Домен получателей.
    pub domain: &'a str,
    /// Отправитель конверта (пустой — `<>`).
    pub from: &'a str,
    /// Получатели домена.
    pub rcpts: &'a [String],
    /// Письмо.
    pub message: &'a [u8],
}

/// Отправка письма принимающему серверу.
#[async_trait]
pub trait Transport: Send + Sync + 'static {
    /// Итог по каждому получателю `req.rcpts` в том же порядке.
    async fn send(&self, req: SendRequest<'_>) -> Vec<RcptResult>;
}

/// Обработчик очереди роли smtp-out.
pub struct QueueWorker {
    deps: QueueDeps,
    transport: Arc<dyn Transport>,
    hostname: String,
}

impl fmt::Debug for QueueWorker {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QueueWorker")
            .field("hostname", &self.hostname)
            .finish_non_exhaustive()
    }
}

impl QueueWorker {
    /// Обработчик: хранилища `deps`, отправка `transport`, имя узла `hostname` (`EHLO`).
    pub fn new(
        deps: QueueDeps,
        transport: Arc<dyn Transport>,
        hostname: impl Into<String>,
    ) -> Self {
        Self {
            deps,
            transport,
            hostname: hostname.into(),
        }
    }

    /// Один проход: захват записей со сроком попытки и их обработка. Возвращает число записей.
    pub async fn run_once(&self, cfg: &QueueConfig) -> Result<usize> {
        let now = (self.deps.now)();
        let claimed = self
            .deps
            .store
            .claim(now, cfg.batch.max(1), cfg.lease)
            .await?;
        let n = claimed.len();
        futures::stream::iter(claimed)
            .for_each_concurrent(cfg.concurrency.max(1), |c| async move {
                let id = c.claim.id;
                match self.process(cfg, c).await {
                    Ok(()) => {}
                    Err(Error::Conflict) => {
                        tracing::info!(%id, "smtp: захват записи очереди устарел, запись у другого узла");
                    }
                    Err(e) => tracing::warn!(%id, error = %e, "smtp: ошибка обработки записи очереди"),
                }
            })
            .await;
        Ok(n)
    }

    async fn process(&self, cfg: &QueueConfig, c: Claimed) -> Result<()> {
        let message = match self.deps.read(&c.blob).await {
            Ok(m) => m,
            Err(e) => {
                // Объект временно недоступен: попытка всем получателям откладывается.
                let reason = format!("message body unavailable: {e}");
                let mut outcome = Vec::new();
                for r in &c.rcpts {
                    outcome.push((r, RcptResult::Temporary(reason.clone())));
                }
                return self.finish(cfg, &c, outcome).await;
            }
        };
        // Получатели по доменам: одно соединение на домен.
        let mut domains: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, r) in c.rcpts.iter().enumerate() {
            let domain = r
                .rcpt
                .address
                .rsplit_once('@')
                .map_or("", |(_, d)| d)
                .to_lowercase();
            domains.entry(domain).or_default().push(i);
        }
        let mut outcome = Vec::new();
        for (domain, idx) in &domains {
            let rcpts: Vec<String> = idx
                .iter()
                .map(|i| c.rcpts[*i].rcpt.address.clone())
                .collect();
            let results = if domain.is_empty() {
                vec![RcptResult::Permanent("501 5.1.3 bad recipient address".into()); rcpts.len()]
            } else {
                self.transport
                    .send(SendRequest {
                        cfg,
                        helo: &self.hostname,
                        domain,
                        from: &c.from,
                        rcpts: &rcpts,
                        message: &message,
                    })
                    .await
            };
            for (i, res) in idx.iter().zip(results) {
                outcome.push((&c.rcpts[*i], res));
            }
            // Долгая доставка нескольким доменам: лиза продлевается после каждого.
            if domains.len() > 1 {
                self.deps
                    .store
                    .renew(&c.claim, (self.deps.now)() + cfg.lease)
                    .await?;
            }
        }
        self.finish(cfg, &c, outcome).await
    }

    /// Записывает итоги попытки: завершение, отказ или повтор.
    async fn finish(
        &self,
        cfg: &QueueConfig,
        c: &Claimed,
        outcome: Vec<(&tp_contracts::queue::QueuedRcpt, RcptResult)>,
    ) -> Result<()> {
        let now = (self.deps.now)();
        let expired = now >= c.queued_at + cfg.expire;
        let mut failed = Vec::new();
        for (r, res) in outcome {
            let address = r.rcpt.address.as_str();
            match res {
                RcptResult::Delivered(reply) => {
                    tracing::info!(id = %c.claim.id, %reply, "smtp: доставлено");
                    self.complete(c, address, RcptStatus::Delivered { reply })
                        .await?;
                }
                RcptResult::Permanent(reply) => failed.push((r, reply)),
                RcptResult::Temporary(reason) if expired => {
                    failed.push((r, format!("delivery time expired, last error: {reason}")));
                }
                RcptResult::Temporary(reason) => {
                    let at = now + cfg.retry_after(r.attempts + 1);
                    tracing::info!(id = %c.claim.id, %reason, attempt = r.attempts + 1, "smtp: временная неудача, повтор");
                    self.deps
                        .store
                        .reschedule(&c.claim, address, at, &reason)
                        .await?;
                }
            }
        }
        for (r, reply) in failed {
            tracing::info!(id = %c.claim.id, %reply, "smtp: окончательный отказ");
            self.complete(c, &r.rcpt.address, RcptStatus::Failed { reply })
                .await?;
        }
        Ok(())
    }

    async fn complete(&self, c: &Claimed, rcpt: &str, status: RcptStatus) -> Result<()> {
        if self.deps.store.complete(&c.claim, rcpt, status).await? {
            self.deps.release(&c.blob).await;
        }
        Ok(())
    }
}

/// Роль smtp-out: опрос очереди каждые `queue.poll_interval` (сразу снова — если проход что-то взял).
#[derive(Debug, Clone)]
pub struct SmtpOutService {
    worker: Arc<QueueWorker>,
}

impl SmtpOutService {
    /// Роль с обработчиком `worker`.
    pub fn new(worker: Arc<QueueWorker>) -> Self {
        Self { worker }
    }
}

#[async_trait]
impl RoleService for SmtpOutService {
    fn name(&self) -> &str {
        tp_contracts::role::Role::SmtpOut.as_str()
    }

    async fn run(&self, ctx: RoleCtx) -> Result<()> {
        ctx.ready.set(true);
        loop {
            let cfg = ctx.config::<SmtpConfig>()?;
            if ctx.shutdown.is_cancelled() {
                break;
            }
            // Начатый проход завершается; после сигнала остановки — не дольше `drain_deadline`.
            let drain = async {
                ctx.shutdown.cancelled().await;
                tokio::time::sleep(ctx.drain_deadline).await;
            };
            let n = tokio::select! {
                () = drain => break,
                r = self.worker.run_once(&cfg.queue) => r.unwrap_or_else(|e| {
                    tracing::warn!(error = %e, "smtp: очередь недоступна");
                    0
                }),
            };
            if n >= cfg.queue.batch.max(1) {
                continue;
            }
            tokio::select! {
                () = ctx.shutdown.cancelled() => break,
                () = tokio::time::sleep(cfg.queue.poll_interval) => {}
            }
        }
        ctx.ready.set(false);
        Ok(())
    }
}

#[cfg(test)]
mod tests;
