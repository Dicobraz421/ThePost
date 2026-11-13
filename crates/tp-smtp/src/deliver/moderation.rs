//! Решения модератора очереди модерации (CTR-31, CTR-43; FIX-01): `impl Moderation for Delivery`.
//!
//! Письмо уже забрано из очереди вызывающим (`GroupStore::take_held`, Admin API). Одобрено — доставка без повторной
//! модерации: письмо группы — её участникам (раскрытие вложенных групп, по одной доставке на учётную запись),
//! письмо правила потока (`group = None`) — исходным получателям (локальные — в ящики, группы раскрываются без
//! модерации, внешние — в очередь исходящей почты). Правила Sieve, пересылки модулей и автоответы — как при обычной
//! доставке; недоставленным получателям — уведомление отправителю. Отклонено — уведомление о недоставке (RFC 3464)
//! отправителю с причиной модератора: локальному — во «Входящие», внешнему — через очередь. Решение — в журнал
//! доставки. Успешное решение снимает ссылку письма очереди на объект; ошибка — ссылка остаётся (вызывающий
//! возвращает письмо в очередь).

use async_trait::async_trait;
use bytes::Bytes;
use std::collections::HashSet;

use time::OffsetDateTime;
use tp_contracts::groups::{
    DistributionGroup, HeldMessage, Moderation, check_delivery, validate_reject_reason,
};
use tp_contracts::queue::DsnRet;
use tp_contracts::{Error, Result, TenantId};

use super::{
    Dd, Delivery, Ek, Filing, NO_OUTBOUND, Origin, Prepared, accounts_of, dsn_step, junk_rcpts,
    local_rcpts, no_outbound, outcome_steps, push_unique,
};
use crate::dsn::{self, DsnSink};
use crate::groups::Groups;
use crate::sieve::{self, Sieved};
use crate::tracking::Step;

impl Delivery {
    /// Группы узла для решения по письму организации `tenant`: другая организация — `Invalid`, без групп —
    /// `Unavailable`.
    fn held_groups(&self, tenant: TenantId) -> Result<&Groups> {
        if tenant != self.deps.tenant {
            return Err(Error::invalid(
                "очередь модерации другой организации, не узла",
            ));
        }
        self.groups.as_ref().ok_or(Error::UNAVAILABLE)
    }

    /// Доставки участникам группы `group` (включённые учётные записи, каждая — один раз) с адресом группы;
    /// контакты в её составе — внешние адресаты.
    async fn members_of(
        &self,
        groups: &Groups,
        group: &DistributionGroup,
        filing: &Filing,
    ) -> Result<Prepared> {
        let rcpt = group.address.clone().unwrap_or_default();
        let mut out = Prepared {
            deliveries: Vec::new(),
            junk: HashSet::new(),
            external: Vec::new(),
            relayed: Vec::new(),
            queued: Vec::new(),
            held: Vec::new(),
            failed: Vec::new(),
        };
        let members = groups.members(self.deps.tenant, group).await?;
        out.external = members.contacts;
        for id in members.accounts {
            if let Some(acc) = self.deps.accounts.get(self.deps.tenant, id).await?
                && acc.enabled
            {
                self.push_delivery(&mut out, &rcpt, &acc, None, filing)
                    .await?;
            }
        }
        Ok(out)
    }
}

/// Адреса письма для журнала и уведомления: исходные получатели, иначе адрес группы.
async fn recipients_of(
    groups: &Groups,
    tenant: TenantId,
    message: &HeldMessage,
) -> Result<Vec<String>> {
    if !message.recipients.is_empty() {
        return Ok(message.recipients.clone());
    }
    let Some(gid) = message.group else {
        return Ok(Vec::new());
    };
    Ok(match groups.get(tenant, gid).await? {
        Some(DistributionGroup {
            address: Some(a), ..
        }) => vec![a],
        // Группа удалена или без адреса: в уведомлении — её идентификатор.
        _ => vec![gid.to_string()],
    })
}

#[async_trait]
impl Moderation for Delivery {
    async fn deliver(
        &self,
        tenant: TenantId,
        group: Option<&DistributionGroup>,
        message: &HeldMessage,
    ) -> Result<()> {
        check_delivery(tenant, group, message)?;
        let groups = self.held_groups(tenant)?;
        let raw = groups.read(&message.blob).await?;
        let id = format!("held-{}", message.id);
        let mail_from = message.sender.as_str();
        let received_at = OffsetDateTime::now_utc();
        let filing = Filing::new(false, false, mail_from, &raw);
        let filing = &filing;
        let (rcpts, mut prepared) = match group {
            Some(g) => (
                g.address.iter().cloned().collect::<Vec<_>>(),
                self.members_of(groups, g, filing).await?,
            ),
            // Письмо правила потока: группы среди получателей раскрываются без модерации (`Origin::Module`).
            None => (
                message.recipients.clone(),
                self.prepare(&id, &message.recipients, filing, Origin::Module)
                    .await?,
            ),
        };
        let mut failed = prepared.failed;
        let delivered = accounts_of(&prepared.deliveries);
        let mut forward = Vec::new();
        let Sieved {
            redirect: hops,
            mut replies,
            steps: mut sieve_steps,
        } = self
            .apply_rules(
                &id,
                mail_from,
                &raw,
                &mut prepared.deliveries,
                &prepared.junk,
                false,
            )
            .await?;
        let local = local_rcpts(&prepared.deliveries);
        let spam = junk_rcpts(&prepared.deliveries, &prepared.junk);
        if !prepared.deliveries.is_empty() {
            let targets: Vec<String> = prepared.deliveries.iter().map(|(r, _)| r.clone()).collect();
            match self
                .ingest_all(
                    &id,
                    mail_from,
                    raw.clone(),
                    prepared.deliveries,
                    received_at,
                    &mut failed,
                )
                .await
            {
                Ok(ingested) => push_unique(&mut forward, ingested.forward),
                // Все ящики переполнены: письмо одобрено, отправителю — уведомление.
                Err(Error::QuotaExceeded) => {
                    for r in targets {
                        if !r.is_empty() && !failed.iter().any(|f| f.address == r) {
                            failed.push(dsn::Recipient::new(
                                r,
                                dsn::Action::Failed,
                                "552 5.2.2 mailbox full",
                            ));
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        let forwarded = self
            .forward(
                &id,
                mail_from,
                &raw,
                forward,
                filing,
                received_at,
                delivered,
            )
            .await?;
        let relayed = self
            .relay(&id, mail_from, hops, filing, received_at, None)
            .await?;
        sieve::merge_replies(&mut replies, relayed.replies);
        sieve_steps.extend(relayed.steps);
        let mut out = prepared.external;
        push_unique(&mut out, prepared.relayed);
        if self.deps.outbound.is_none() && !out.is_empty() {
            // Узел без очереди: внешним получателям письмо не уйдёт — отправителю уведомление.
            no_outbound(&id, out.len());
            failed.extend(
                out.drain(..)
                    .map(|r| dsn::Recipient::new(r, dsn::Action::Failed, NO_OUTBOUND)),
            );
        }
        self.queue_out(&id, message.sender_account, mail_from, out, raw.clone())
            .await?;
        self.queue_forward(&id, mail_from, forwarded, raw.clone(), None)
            .await?;
        let mut steps =
            vec![Step::new(Ek::Receive, Dd::Accept, rcpts).reason("approved by moderator")];
        outcome_steps(&mut steps, &[], &local, &failed, &spam);
        steps.extend(sieve_steps);
        self.send_replies(&id, mail_from, &raw, replies, &mut steps)
            .await;
        if !failed.is_empty() && !mail_from.is_empty() {
            let sent = self
                .bounce(&id, mail_from, &failed, &raw, received_at)
                .await;
            steps.push(dsn_step(mail_from, sent));
        }
        self.track(mail_from, message.message_id.as_deref(), steps)
            .await;
        groups.release(&message.blob).await;
        tracing::info!(%id, group = ?message.group, "smtp: письмо очереди модерации одобрено и доставлено");
        Ok(())
    }

    async fn reject(&self, tenant: TenantId, message: &HeldMessage, reason: &str) -> Result<()> {
        validate_reject_reason(reason)?;
        let groups = self.held_groups(tenant)?;
        let id = format!("held-{}", message.id);
        let rcpts = recipients_of(groups, tenant, message).await?;
        let mut steps = vec![
            Step::new(Ek::Reject, Dd::Reject, rcpts.clone())
                .reason(format!("rejected by moderator: {reason}")),
        ];
        if !message.sender.is_empty() {
            let raw = groups.read(&message.blob).await?;
            let reply = format!("550 5.7.1 {reason}");
            let failed: Vec<dsn::Recipient> = rcpts
                .iter()
                .map(|r| dsn::Recipient::new(r.clone(), dsn::Action::Failed, reply.clone()))
                .collect();
            let notice = dsn::build(&dsn::Report {
                hostname: &self.hostname,
                to: &message.sender,
                envid: None,
                arrival: message.held_at,
                now: OffsetDateTime::now_utc(),
                retry_until: None,
                rcpts: &failed,
                original: &raw,
                ret: DsnRet::Headers,
                id: &id,
            });
            // Уведомление не отправлено — решение не принято: письмо вернётся в очередь.
            self.send_dsn(tenant, &message.sender, Bytes::from(notice))
                .await?;
            steps.push(dsn_step(&message.sender, true));
        }
        self.track(&message.sender, message.message_id.as_deref(), steps)
            .await;
        groups.release(&message.blob).await;
        tracing::info!(%id, group = ?message.group, "smtp: письмо очереди модерации отклонено");
        Ok(())
    }
}
