//! Аудит доступа к ящику (JMAP-18; CONTRACTS.md#MailboxAudit, SRS 5.2, решение 2026-11-04-exchange-parity).
//!
//! Методы JMAP сообщают журналу [`MailboxAudit`](tp_contracts::audit::MailboxAudit) о каждом действии с ящиком,
//! записывать ли его — решает политика ящика в хранилище:
//! - владелец в своём ящике: удаление писем (`Email/set` destroy — в корзину и очистка корзины) — `Delete`, перенос
//!   между папками (`Email/set` update `mailboxIds`) — `Move`, отправка (`EmailSubmission/set` create) — `Send`,
//!   изменение скриптов Sieve и автоответа (`SieveScript/set`, `VacationResponse/set`) — `ChangeRules`. Пишутся только
//!   при включённой политике ящика или организации;
//! - не владелец: отправка от адреса другой учётной записи — `SendAs` или `SendOnBehalf` в ящике владельца адреса
//!   (исполнитель — представитель); пишется по умолчанию. Других действий в чужом ящике у JMAP нет: методам доступна
//!   только своя учётная запись;
//! - чтение владельцем (`Email/get`, `Email/query` и прочие `/get`) журналу не сообщается.
//!
//! Запись делается после выполненного действия. Сбой журнала действие не отменяет: ошибка — в журнале узла
//! (`error`), метод отвечает успехом.

use time::OffsetDateTime;
use tp_contracts::audit::{
    AuditAction, AuditActorKind, AuditEntry, AuditObject, MAX_AUDIT_TEXT_LEN,
};
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, MailboxId, MessageId};

use crate::core::Ctx;

/// Пояснение записи: без управляющих символов, не длиннее [`MAX_AUDIT_TEXT_LEN`]; пустое — `None`.
pub(crate) fn detail(text: &str) -> Option<String> {
    let clean: String = text
        .chars()
        .filter(|c| !c.is_control())
        .take(MAX_AUDIT_TEXT_LEN)
        .collect();
    (!clean.is_empty()).then_some(clean)
}

async fn record(
    ctx: &Ctx<'_>,
    scope: AccountScope,
    kind: AuditActorKind,
    action: AuditAction,
    object: AuditObject,
) {
    let entry = AuditEntry {
        scope,
        at: OffsetDateTime::now_utc(),
        actor: ctx.principal.login.clone(),
        actor_account: Some(ctx.principal.account),
        actor_kind: kind,
        action,
        object,
        client: Some("jmap".to_owned()),
    };
    if let Err(e) = ctx.jmap.d.audit.record_audit(entry).await {
        tracing::error!(error = %e, ?action, "jmap: запись аудита ящика не сделана");
    }
}

/// Действие владельца в своём ящике.
pub(crate) async fn owner(ctx: &Ctx<'_>, action: AuditAction, object: AuditObject) {
    record(ctx, ctx.scope, AuditActorKind::Owner, action, object).await;
}

/// Действие владельца с письмами своего ящика (папка, письмо). Политика читается один раз: если действие не
/// записывается, журнал по каждому письму не вызывается.
pub(crate) async fn owner_messages(
    ctx: &Ctx<'_>,
    action: AuditAction,
    messages: &[(MailboxId, MessageId)],
) {
    if messages.is_empty() {
        return;
    }
    match ctx.jmap.d.audit.audit_policy(ctx.scope).await {
        Ok(p) if !p.records(AuditActorKind::Owner, action) => return,
        Ok(_) => {}
        Err(e) => {
            tracing::error!(error = %e, ?action, "jmap: политика аудита ящика не прочитана");
            return;
        }
    }
    for (mailbox, message) in messages {
        let object = AuditObject {
            mailbox: Some(*mailbox),
            message: Some(*message),
            detail: None,
        };
        owner(ctx, action, object).await;
    }
}

/// Действие представителя в ящике учётной записи `mailbox_owner`.
pub(crate) async fn delegate(
    ctx: &Ctx<'_>,
    mailbox_owner: AccountId,
    action: AuditAction,
    object: AuditObject,
) {
    let scope = AccountScope {
        tenant: ctx.scope.tenant,
        account: mailbox_owner,
    };
    record(ctx, scope, AuditActorKind::Delegate, action, object).await;
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use tp_contracts::audit::{AuditAction, AuditActorKind, AuditPolicy, MailboxAudit};
    use tp_contracts::auth::SendRight;
    use tp_contracts::meta::SpecialUse;

    use crate::testenv::Env;

    const USING: &[&str] = &[
        "urn:ietf:params:jmap:core",
        "urn:ietf:params:jmap:mail",
        "urn:ietf:params:jmap:submission",
        "urn:ietf:params:jmap:vacationresponse",
    ];

    async fn calls(env: &Env, calls: Value) -> Vec<Value> {
        let resp = env
            .request(json!({"using": USING, "methodCalls": calls}))
            .await;
        resp["methodResponses"]
            .as_array()
            .cloned()
            .unwrap_or_else(|| panic!("нет ответов: {resp}"))
    }

    /// Черновик от `from` и его отправка; возвращает идентификатор письма.
    async fn send(env: &Env, from: &str) -> String {
        let acc = env.account();
        let r = calls(
            env,
            json!([
                ["Email/set", {"accountId": acc, "create": {"d": {
                    "mailboxIds": {env.mailbox(SpecialUse::Drafts).to_string(): true},
                    "from": [{"email": from}],
                    "to": [{"email": "rcpt@external.test"}],
                    "messageId": ["m1@example.org"],
                    "subject": "Отчёт",
                    "textBody": [{"partId": "t"}],
                    "bodyValues": {"t": {"value": "текст"}},
                }}}, "0"],
                ["EmailSubmission/set", {"accountId": acc, "create": {"s": {
                    "identityId": crate::submission::identity_id("user@example.org"),
                    "emailId": "#d",
                }}}, "1"]
            ]),
        )
        .await;
        assert!(r[1][1]["created"]["s"].is_object(), "{:?}", r[1]);
        r[0][1]["created"]["d"]["id"]
            .as_str()
            .expect("email")
            .to_owned()
    }

    /// Доступ представителя записан по умолчанию: отправка от адреса другой учётной записи — в её ящике, со своим
    /// действием для Send As и Send on Behalf. Чтение и отправка владельцем при политике по умолчанию — нет.
    #[tokio::test]
    async fn delegate_send_is_recorded_owner_read_is_not() {
        let env = Env::new().await;
        let acc = env.account();
        let boss = env.user("boss@example.org").await.account;
        let shared = env.user("shared@example.org").await.account;
        {
            let mut rights = env.authz.0.lock().expect("lock");
            rights.insert("boss@example.org".into(), SendRight::SendOnBehalf);
            rights.insert("shared@example.org".into(), SendRight::SendAs);
        }
        let own = send(&env, "user@example.org").await;
        calls(
            &env,
            json!([
                ["Email/get", {"accountId": acc, "ids": [own]}, "g"],
                ["Email/query", {"accountId": acc}, "q"],
                ["Mailbox/get", {"accountId": acc}, "m"],
            ]),
        )
        .await;
        assert_eq!(env.audit.entries(), vec![], "владелец: чтение и отправка");

        send(&env, "boss@example.org").await;
        send(&env, "shared@example.org").await;
        let entries = env.audit.entries();
        assert_eq!(entries.len(), 2, "{entries:?}");
        for (e, owner, action) in [
            (&entries[0], boss, AuditAction::SendOnBehalf),
            (&entries[1], shared, AuditAction::SendAs),
        ] {
            assert_eq!(e.scope.account, owner, "{e:?}");
            assert_eq!(e.scope.tenant, env.scope.tenant);
            assert_eq!(e.actor, "user@example.org");
            assert_eq!(e.actor_account, Some(env.scope.account));
            assert_eq!(e.actor_kind, AuditActorKind::Delegate);
            assert_eq!(e.action, action);
            assert_eq!(e.object.detail.as_deref(), Some("m1@example.org"));
            assert_eq!(e.client.as_deref(), Some("jmap"));
        }
    }

    /// Действия владельца — по политике ящика: отправка, перенос, удаление (в корзину и из корзины), изменение
    /// автоответа записаны; чтение — нет даже при политике «записывать всё».
    #[tokio::test]
    async fn owner_actions_by_policy() {
        let env = Env::new().await;
        let acc = env.account();
        env.audit
            .set_audit_policy(
                env.scope.tenant,
                Some(env.scope.account),
                Some(AuditPolicy::all()),
            )
            .await
            .expect("policy");
        let email = send(&env, "user@example.org").await;
        let sent = env.mailbox(SpecialUse::Sent);
        let drafts = env.mailbox(SpecialUse::Drafts);
        let r = calls(
            &env,
            json!([
                ["Email/set", {"accountId": acc, "update": {&email: {
                    format!("mailboxIds/{drafts}"): null, format!("mailboxIds/{sent}"): true}}}, "mv"],
                ["Email/set", {"accountId": acc, "update": {&email: {"keywords/$seen": true}}}, "kw"],
                ["Email/get", {"accountId": acc, "ids": [email]}, "g"],
                ["Email/set", {"accountId": acc, "destroy": [email]}, "d1"],
                ["Email/set", {"accountId": acc, "destroy": [email]}, "d2"],
                ["VacationResponse/set", {"accountId": acc, "update": {"singleton": {"isEnabled": true}}}, "v"],
            ]),
        )
        .await;
        assert_eq!(r[3][1]["destroyed"], json!([email]), "{:?}", r[3]);
        assert_eq!(r[4][1]["destroyed"], json!([email]), "{:?}", r[4]);
        assert!(r[5][1]["updated"].is_object(), "{:?}", r[5]);
        let entries = env.audit.entries();
        let actions: Vec<AuditAction> = entries.iter().map(|e| e.action).collect();
        assert_eq!(
            actions,
            vec![
                AuditAction::Send,
                AuditAction::Move,
                AuditAction::Delete,
                AuditAction::Delete,
                AuditAction::ChangeRules,
            ],
            "{entries:?}"
        );
        assert!(entries.iter().all(|e| e.actor_kind == AuditActorKind::Owner
            && e.scope == env.scope
            && e.actor == "user@example.org"));
        assert_eq!(entries[0].object.detail.as_deref(), Some("m1@example.org"));
        assert_eq!(entries[1].object.mailbox, Some(sent));
        assert_eq!(entries[2].object.mailbox, Some(sent));
        assert_eq!(
            entries[3].object.mailbox,
            Some(env.mailbox(SpecialUse::Trash))
        );
        assert_eq!(entries[4].object.detail.as_deref(), Some("vacation"));
    }

    /// Сбой журнала аудита действие не отменяет.
    #[tokio::test]
    async fn audit_failure_keeps_action() {
        let env = Env::new().await;
        env.user("boss@example.org").await;
        env.authz
            .0
            .lock()
            .expect("lock")
            .insert("boss@example.org".into(), SendRight::SendAs);
        env.audit
            .fail_next(tp_contracts::Error::Unavailable { retry_after: None });
        send(&env, "boss@example.org").await;
        assert_eq!(env.outbound.queued().len(), 1);
        assert!(env.audit.entries().is_empty());
    }
}
