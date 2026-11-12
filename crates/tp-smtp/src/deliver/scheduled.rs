//! Отложенная отправка и её отмена (SRS 4.1, решение 2026-11-04-exchange-parity; SMTP-24).
//!
//! [`Delivery::schedule`] — фильтры Milter исходящей почты и правила потока (один раз на письмо, до постановки в очередь) и постановка
//! письма всем получателям, включая локальных, со сроком выпуска. До срока запись можно отменить
//! ([`Delivery::cancel_scheduled`]): она удаляется, тело освобождается. В срок запись берёт обработчик очереди
//! ([`crate::queue::QueueWorker`]): локальные получатели доставляются в ящики через [`LocalRelease`] — группы
//! раскрываются на момент выпуска, применяются правила Sieve получателей; внешние уходят по SMTP.

use async_trait::async_trait;
use bytes::Bytes;
use time::OffsetDateTime;
use tp_contracts::queue::SubmitStatus;
use tp_contracts::{Error, QueueId, Result, TenantId};

use super::{Delivery, Flowed, Pre, Resolved};
use crate::queue::{LocalRelease, RcptResult};

impl Delivery {
    /// Ставит письмо в очередь со сроком выпуска `release_at` (`Outbound::schedule`). Лимиты отправки в час — те же,
    /// что у [`Delivery::submit`] (SMTP-43): письмо учитывается при постановке, сверх предела — `Error::Unavailable`
    /// и событие отказа в журнале доставки. Письмо, отклонённое фильтром Milter или правилом потока, —
    /// `Error::Forbidden`, временный отказ и узел без очереди — `Error::Unavailable`.
    /// Письмо, удалённое фильтром или правилом потока, в очередь не ставится: возвращается идентификатор без записи (отмена такой
    /// отправки — `NotFound`, как у уже доставленной).
    pub(super) async fn schedule(
        &self,
        tenant: TenantId,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
        release_at: OffsetDateTime,
    ) -> Result<QueueId> {
        let Some(outbound) = &self.deps.outbound else {
            return Err(Error::UNAVAILABLE);
        };
        if self
            .over_send_limit(tenant, mail_from, rcpts, &raw)
            .await?
            .is_some()
        {
            return Err(Error::UNAVAILABLE);
        }
        let Flowed {
            mail_from,
            rcpts,
            raw,
        } = match self
            .pre_submit(&crate::server::next_id(), tenant, mail_from, rcpts, raw)
            .await?
        {
            Pre::Done(SubmitStatus::Failed(reply)) if reply.starts_with('4') => {
                return Err(Error::UNAVAILABLE);
            }
            Pre::Done(SubmitStatus::Failed(_)) => return Err(Error::Forbidden),
            Pre::Done(SubmitStatus::Delivered | SubmitStatus::Queued) => {
                tracing::info!("smtp: отложенное письмо удалено фильтром или правилом потока");
                return Ok(QueueId::new());
            }
            Pre::Go(m) => m,
        };
        outbound
            .schedule(tenant, &mail_from, &rcpts, raw, release_at)
            .await
    }

    /// Отмена отложенной отправки до выпуска (`Outbound::cancel_scheduled`).
    pub(super) async fn cancel_scheduled(&self, tenant: TenantId, queue: QueueId) -> Result<()> {
        match &self.deps.outbound {
            Some(outbound) => outbound.cancel_scheduled(tenant, queue).await,
            None => Err(Error::UNAVAILABLE),
        }
    }
}

#[async_trait]
impl LocalRelease for Delivery {
    async fn release(
        &self,
        tenant: TenantId,
        from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<Vec<Option<RcptResult>>> {
        if tenant != self.deps.tenant {
            return Ok(vec![None; rcpts.len()]);
        }
        // Локальный — адрес, который узел доставляет сам: ящик, группа, неизвестный адрес своего домена (отказ),
        // адрес SRS (уведомление исходному отправителю). Внешний домен и адрес внутренней ретрансляции — по SMTP.
        let mut local = Vec::new();
        for rcpt in rcpts {
            if self.srs_reverse(rcpt).await?.is_some()
                || self.target(rcpt).await?.resolved != Resolved::External
            {
                local.push(rcpt.clone());
            }
        }
        if local.is_empty() {
            return Ok(vec![None; rcpts.len()]);
        }
        let id = crate::server::next_id();
        // Уведомление отправителю — по итогам записи очереди (обработчик), не здесь.
        let statuses = self.dispatch(&id, from, &local, raw, false).await?;
        tracing::info!(%id, count = local.len(), "smtp: запись очереди выпущена локальным получателям");
        Ok(rcpts
            .iter()
            .map(|r| {
                let (_, status) = statuses.iter().find(|(x, _)| x == r)?;
                Some(match status {
                    SubmitStatus::Delivered => RcptResult::Delivered("250 2.0.0 delivered".into()),
                    // Ждёт решения модератора группы.
                    SubmitStatus::Queued => RcptResult::Delivered("250 2.0.0 held".into()),
                    SubmitStatus::Failed(reply) if reply.starts_with('4') => {
                        RcptResult::Temporary(reply.clone())
                    }
                    SubmitStatus::Failed(reply) => RcptResult::Permanent(reply.clone()),
                })
            })
            .collect())
    }
}
