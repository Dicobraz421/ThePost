//! Очередь исходящей почты (SRS 4.1, CONTRACTS.md#Queue (Ф1)): постановка и роль smtp-out.
//!
//! - [`Queue`] — [`Outbound`] поверх [`QueueStore`]: письмо — один объект BLOB (протокол записи SRS 3.4,
//!   одна ссылка на запись очереди), затем `QueueStore::enqueue`; ошибка постановки снимает ссылку.
//! - [`QueueWorker`] — обработка: `claim` записей со сроком попытки под лизу, доставка получателям по доменам
//!   через [`Transport`] (по умолчанию — [`send::MxTransport`] на `mail-send`), затем по каждому получателю:
//!   2xx — `complete(Delivered)`, 5xx или истёк `queue.expire` — `complete(Failed)`, 4xx и сбой соединения —
//!   `reschedule` через `queue.retry`. Когда запись завершена (`complete` = `true`), ссылка на объект снимается.
//!   Устаревший захват (`Conflict`: лиза истекла, запись взял другой узел) — запись пропускается.
//!   Уведомления отправителю (DSN: отказ, задержка, успех) — [`crate::dsn`].
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
use tp_contracts::queue::{
    Claimed, DsnNotify, DsnRet, OutMessage, OutRcpt, QueueStore, QueuedRcpt, RcptStatus,
};
use tp_contracts::role::{RoleCtx, RoleService};
use tp_contracts::{BlobKey, Error, Result, TenantId};

use crate::config::{QueueConfig, SmtpConfig};
use crate::deliver::Outbound;
use crate::dkim_sign::DkimSign;
use crate::dsn::{self, DsnSink};
use crate::metrics::{RcptOutcome, SmtpMetrics};

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

/// Постановка в очередь исходящей почты ([`Outbound`] для доставки); с [`Queue::with_dkim`] письма
/// подписываются DKIM перед постановкой.
#[derive(Debug, Clone)]
pub struct Queue {
    deps: QueueDeps,
    dkim: Option<Arc<DkimSign>>,
}

impl Queue {
    /// Очередь поверх хранилищ `deps`.
    pub fn new(deps: QueueDeps) -> Self {
        Self { deps, dkim: None }
    }

    /// Подпись DKIM исходящих писем (SMTP-08).
    pub fn with_dkim(mut self, dkim: Arc<DkimSign>) -> Self {
        self.dkim = Some(dkim);
        self
    }

    /// Письмо с подписью DKIM, если домен `From` подписывается.
    async fn signed(&self, raw: Bytes) -> Result<Bytes> {
        let Some(dkim) = &self.dkim else {
            return Ok(raw);
        };
        Ok(match dkim.sign(&raw).await? {
            Some(header) => {
                let mut out = Vec::with_capacity(header.len() + raw.len());
                out.extend_from_slice(header.as_bytes());
                out.extend_from_slice(&raw);
                Bytes::from(out)
            }
            None => raw,
        })
    }
}

#[async_trait]
impl DsnSink for Queue {
    async fn send_dsn(&self, tenant: TenantId, to: &str, raw: Bytes) -> Result<()> {
        let raw = self.signed(raw).await?;
        self.deps.put(tenant, "", vec![OutRcpt::new(to)], raw).await
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
        let raw = self.signed(raw).await?;
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
    dsn: Arc<dyn DsnSink>,
    metrics: Option<Arc<SmtpMetrics>>,
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
            dsn: Arc::new(Queue::new(deps.clone())),
            deps,
            transport,
            hostname: hostname.into(),
            metrics: None,
        }
    }

    /// Метрики очереди (SMTP-11).
    pub fn with_metrics(mut self, metrics: Arc<SmtpMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Куда отправлять уведомления о доставке (по умолчанию — в эту же очередь; локальному отправителю
    /// удобнее [`crate::Delivery`]).
    pub fn with_dsn(mut self, sink: Arc<dyn DsnSink>) -> Self {
        self.dsn = sink;
        self
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
        if let Some(m) = &self.metrics {
            m.claimed(n);
        }
        futures::stream::iter(claimed)
            .for_each_concurrent(cfg.concurrency.max(1), |c| async move {
                let id = c.claim.id;
                let m = self.metrics.as_deref();
                m.inspect(|m| m.inflight(1));
                let res = self.process(cfg, c).await;
                m.inspect(|m| m.inflight(-1));
                match res {
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
                return self.finish(cfg, &c, &[], outcome).await;
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
        self.finish(cfg, &c, &message, outcome).await
    }

    /// Записывает итоги попытки: завершение, повтор, отказ — и уведомления отправителю (RFC 3464):
    /// об успехе — только по `NOTIFY=SUCCESS`, о задержке — однажды после `queue.delay_notify`, об отказе —
    /// до `complete(Failed)` (сбой постановки уведомления — ошибка записи, отказ повторится после лизы).
    /// Письму с пустым отправителем (само уведомление) уведомлений нет.
    async fn finish(
        &self,
        cfg: &QueueConfig,
        c: &Claimed,
        message: &[u8],
        outcome: Vec<(&QueuedRcpt, RcptResult)>,
    ) -> Result<()> {
        let now = (self.deps.now)();
        let age = now - c.queued_at;
        let expired = age >= cfg.expire;
        let mut failed = Vec::new();
        let mut delayed = Vec::new();
        let mut delivered = Vec::new();
        for (r, res) in outcome {
            if let Some(m) = &self.metrics {
                let kind = match &res {
                    RcptResult::Delivered(_) => RcptOutcome::Delivered,
                    RcptResult::Permanent(_) => RcptOutcome::Failed,
                    RcptResult::Temporary(_) if expired => RcptOutcome::Failed,
                    RcptResult::Temporary(_) => RcptOutcome::Deferred,
                };
                m.rcpt(kind, age.as_seconds_f64());
            }
            let address = r.rcpt.address.as_str();
            let notify = r.rcpt.notify.unwrap_or(DsnNotify {
                success: false,
                failure: true,
                delay: true,
            });
            match res {
                RcptResult::Delivered(reply) => {
                    tracing::info!(id = %c.claim.id, %reply, "smtp: доставлено");
                    if notify.success {
                        delivered.push(dsn_rcpt(r, dsn::Action::Delivered, &reply));
                    }
                    self.complete(c, address, RcptStatus::Delivered { reply })
                        .await?;
                }
                RcptResult::Permanent(reply) => failed.push((r, reply, None)),
                RcptResult::Temporary(reason) if expired => {
                    let reply = format!("delivery time expired, last error: {reason}");
                    failed.push((r, reply, Some("4.4.7")));
                }
                RcptResult::Temporary(reason) => {
                    let at = now + cfg.retry_after(r.attempts + 1);
                    tracing::info!(id = %c.claim.id, %reason, attempt = r.attempts + 1, "smtp: временная неудача, повтор");
                    // Порог задержки пересечён этой попыткой (предыдущая была раньше порога).
                    let prev = if r.attempts == 0 {
                        time::Duration::ZERO
                    } else {
                        age - cfg.retry_after(r.attempts)
                    };
                    if notify.delay && age >= cfg.delay_notify && prev < cfg.delay_notify {
                        delayed.push(dsn_rcpt(r, dsn::Action::Delayed, &reason));
                    }
                    self.deps
                        .store
                        .reschedule(&c.claim, address, at, &reason)
                        .await?;
                }
            }
        }
        // Уведомления об успехе и задержке — справочные: сбой их постановки только в журнал.
        for (rcpts, retry_until) in [(delivered, None), (delayed, Some(c.queued_at + cfg.expire))] {
            if let Err(e) = self.notify(c, message, &rcpts, retry_until).await {
                tracing::warn!(id = %c.claim.id, error = %e, "smtp: уведомление о доставке не поставлено");
            }
        }
        let notify_failed: Vec<_> = failed
            .iter()
            .filter(|(r, _, _)| r.rcpt.notify.is_none_or(|n| n.failure))
            .map(|(r, reply, status)| {
                let mut x = dsn_rcpt(r, dsn::Action::Failed, reply);
                if let Some(s) = status {
                    x.status = (*s).to_owned();
                }
                x
            })
            .collect();
        self.notify(c, message, &notify_failed, None).await?;
        for (r, reply, _) in failed {
            tracing::info!(id = %c.claim.id, %reply, "smtp: окончательный отказ");
            self.complete(c, &r.rcpt.address, RcptStatus::Failed { reply })
                .await?;
        }
        Ok(())
    }

    /// Уведомление отправителю записи `c` о получателях `rcpts` (пусто или пустой отправитель — ничего).
    async fn notify(
        &self,
        c: &Claimed,
        message: &[u8],
        rcpts: &[dsn::Recipient],
        retry_until: Option<OffsetDateTime>,
    ) -> Result<()> {
        if rcpts.is_empty() || c.from.is_empty() {
            return Ok(());
        }
        let id = c.claim.id.to_string();
        let raw = dsn::build(&dsn::Report {
            hostname: &self.hostname,
            to: &c.from,
            envid: c.dsn.envid.as_deref(),
            arrival: c.queued_at,
            now: (self.deps.now)(),
            retry_until,
            rcpts,
            original: message,
            ret: c.dsn.ret.unwrap_or(DsnRet::Headers),
            id: &id,
        });
        self.dsn.send_dsn(c.tenant, &c.from, Bytes::from(raw)).await
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

fn dsn_rcpt(r: &QueuedRcpt, action: dsn::Action, reply: &str) -> dsn::Recipient {
    dsn::Recipient {
        orcpt: r.rcpt.orcpt.clone(),
        ..dsn::Recipient::new(&r.rcpt.address, action, reply)
    }
}

#[cfg(test)]
pub(crate) mod tests;
