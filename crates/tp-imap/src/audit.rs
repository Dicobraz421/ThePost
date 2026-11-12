//! Аудит доступа к ящику в IMAP и POP3 (IMAP-15; CONTRACTS.md#MailboxAudit, SRS 5.2).
//!
//! Действие с ящиком записывается в [`MailboxAudit`] из [`crate::Deps::audit`]: исполнитель — владелец, если вошёл в свою учётную запись,
//! иначе представитель (общие папки по ACL). Что сохранять, решает политика ящика в хранилище; вход и чтение
//! владельцем не передаются вовсе (никогда не записываются). Точки записи:
//! - `SELECT`/`EXAMINE` чужой папки — `Login`;
//! - `FETCH` содержимого письма (`BODY[…]`, `BINARY[…]`, `RFC822*`) и `COPY`/`MOVE` из чужой папки — `Read`
//!   до выдачи данных: без записи данные не выдаются;
//! - `MOVE` — `Move`; `EXPUNGE`, `UID EXPUNGE`, `CLOSE`, POP3 `QUIT` с `DELE`, `DELETE` папки с письмами —
//!   `Delete` (запись о папке);
//! - `SETACL`, `DELETEACL` — `ChangeRights` (пояснение — идентификатор и новые права).
//!
//! Изменения записываются после выполнения: ошибка журнала уже не отменяет действие и попадает в журнал узла.

use std::net::IpAddr;
use std::sync::Arc;

use time::OffsetDateTime;
use tp_contracts::Result;
use tp_contracts::audit::{AuditAction, AuditActorKind, AuditEntry, AuditObject, MailboxAudit};
use tp_contracts::auth::Principal;
use tp_contracts::ids::{MailboxId, MessageId};
use tp_contracts::meta::AccountScope;

use crate::session::{Session, Transport};

/// Журнал аудита сессии и адрес клиента.
#[derive(Clone)]
pub(crate) struct Audit {
    log: Arc<dyn MailboxAudit>,
    client: Option<String>,
}

impl Audit {
    /// Журнал `log` для сессии клиента `client` протокола `proto` (`imap`, `pop3`).
    pub(crate) fn new(log: Arc<dyn MailboxAudit>, proto: &str, client: Option<IpAddr>) -> Self {
        Self {
            log,
            client: client.map(|ip| format!("{proto} {ip}")),
        }
    }

    /// Записывает действие `action` субъекта `p` с папкой `mailbox` ящика `owner`: по записи на каждое письмо
    /// `messages` (пусто — одна запись о папке).
    pub(crate) async fn record(
        &self,
        p: &Principal,
        owner: AccountScope,
        action: AuditAction,
        mailbox: Option<MailboxId>,
        messages: &[MessageId],
        detail: Option<String>,
    ) -> Result<()> {
        let log = &self.log;
        let kind = if p.tenant == owner.tenant && p.account == owner.account {
            AuditActorKind::Owner
        } else {
            AuditActorKind::Delegate
        };
        if kind == AuditActorKind::Owner && matches!(action, AuditAction::Login | AuditAction::Read)
        {
            return Ok(());
        }
        let entry = |message| AuditEntry {
            scope: owner,
            at: OffsetDateTime::now_utc(),
            actor: p.login.clone(),
            actor_account: Some(p.account),
            actor_kind: kind,
            action,
            object: AuditObject {
                mailbox,
                message,
                detail: detail.clone(),
            },
            client: self.client.clone(),
        };
        if messages.is_empty() {
            log.record_audit(entry(None)).await?;
        }
        for &m in messages {
            log.record_audit(entry(Some(m))).await?;
        }
        Ok(())
    }

    /// [`Audit::record`] после выполненного действия: ошибка журнала — в журнал узла.
    pub(crate) async fn record_done(
        &self,
        p: &Principal,
        owner: AccountScope,
        action: AuditAction,
        mailbox: Option<MailboxId>,
        messages: &[MessageId],
        detail: Option<String>,
    ) {
        if let Err(e) = self
            .record(p, owner, action, mailbox, messages, detail)
            .await
        {
            tracing::error!(error = %e, ?action, "аудит ящика: запись не сохранена");
        }
    }
}

impl<T: Transport> Session<T> {
    /// [`Audit::record`] от имени вошедшего субъекта; до входа — ничего.
    pub(crate) async fn audit(
        &self,
        owner: AccountScope,
        action: AuditAction,
        mailbox: MailboxId,
        messages: &[MessageId],
        detail: Option<String>,
    ) -> Result<()> {
        match &self.principal {
            Some(p) => {
                self.audit
                    .record(p, owner, action, Some(mailbox), messages, detail)
                    .await
            }
            None => Ok(()),
        }
    }

    /// [`Audit::record_done`] от имени вошедшего субъекта; до входа — ничего.
    pub(crate) async fn audit_done(
        &self,
        owner: AccountScope,
        action: AuditAction,
        mailbox: MailboxId,
        messages: &[MessageId],
        detail: Option<String>,
    ) {
        if let Some(p) = &self.principal {
            self.audit
                .record_done(p, owner, action, Some(mailbox), messages, detail)
                .await;
        }
    }
}
