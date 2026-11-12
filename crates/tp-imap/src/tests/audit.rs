//! Аудит доступа к ящику (IMAP-15): владелец — только по политике, чтение владельцем — никогда.

use tp_contracts::audit::{AuditAction, AuditActorKind, AuditPolicy, MailboxAudit};
use tp_contracts::meta::{Flag, SpecialUse};

use super::Env;

const PLAIN: &[u8] = b"From: a@example.org\r\nSubject: hi\r\n\r\nHello\r\n";

#[tokio::test]
async fn owner_read_not_recorded_delete_by_policy() {
    let env = Env::new().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    let deleted = [Flag::Deleted].into_iter().collect();
    env.deliver(inbox, PLAIN, deleted).await;
    let second = env.deliver(inbox, PLAIN, Default::default()).await.message;
    let mut c = env.logged_in().await;
    assert!(c.cmd("S1 SELECT INBOX").await.contains("S1 OK"));
    assert!(
        c.cmd("F1 FETCH 1:2 (BODY[] RFC822.TEXT)")
            .await
            .contains("Hello")
    );
    // Политика по умолчанию: действия владельца не записываются.
    assert!(c.cmd("E1 EXPUNGE").await.contains("E1 OK"));
    assert!(env.audit.entries().is_empty());
    // Политика «всё»: удаление и перемещение владельца записываются, чтение — по-прежнему нет.
    env.audit
        .set_audit_policy(env.principal.tenant, None, Some(AuditPolicy::all()))
        .await
        .expect("policy");
    assert!(c.cmd("F2 FETCH 1 BODY[]").await.contains("Hello"));
    assert!(c.cmd("M1 MOVE 1 Trash").await.contains("M1 OK"));
    let e = env.audit.entries();
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0].action, AuditAction::Delete);
    assert_eq!(e[0].actor_kind, AuditActorKind::Owner);
    assert_eq!(e[0].scope, env.scope());
    assert_eq!(e[0].object.mailbox, Some(inbox));
    assert_eq!(e[0].object.message, Some(second));
    assert_eq!(e[0].client.as_deref(), Some("imap 192.0.2.1"));
}

#[tokio::test]
async fn owner_move_by_policy() {
    let env = Env::new().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    let id = env.deliver(inbox, PLAIN, Default::default()).await.message;
    env.audit
        .set_audit_policy(
            env.principal.tenant,
            Some(env.principal.account),
            Some(AuditPolicy::all()),
        )
        .await
        .expect("policy");
    let mut c = env.logged_in().await;
    assert!(c.cmd("S1 SELECT INBOX").await.contains("S1 OK"));
    assert!(c.cmd("C1 CREATE Work").await.contains("C1 OK"));
    assert!(c.cmd("M1 MOVE 1 Work").await.contains("M1 OK"));
    let e = env.audit.entries();
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0].action, AuditAction::Move);
    assert_eq!(e[0].object.mailbox, Some(inbox));
    assert_eq!(e[0].object.message, Some(id));
    assert_eq!(e[0].actor, env.principal.login);
    assert_eq!(e[0].actor_account, Some(env.principal.account));
}

#[tokio::test]
async fn owner_delete_folder_with_messages_by_policy() {
    let env = Env::new().await;
    env.audit
        .set_audit_policy(env.principal.tenant, None, Some(AuditPolicy::all()))
        .await
        .expect("policy");
    let mut c = env.logged_in().await;
    assert!(c.cmd("C1 CREATE Empty").await.contains("C1 OK"));
    assert!(c.cmd("C2 CREATE Work").await.contains("C2 OK"));
    let append = format!(
        "A1 APPEND Work {{{}+}}\r\n{}",
        PLAIN.len(),
        String::from_utf8_lossy(PLAIN)
    );
    assert!(c.cmd(&append).await.contains("A1 OK"));
    // Пустая папка — без записи: писем не удалено.
    assert!(c.cmd("D1 DELETE Empty").await.contains("D1 OK"));
    assert!(env.audit.entries().is_empty());
    assert!(c.cmd("D2 DELETE Work").await.contains("D2 OK"));
    let e = env.audit.entries();
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(e[0].action, AuditAction::Delete);
    assert_eq!(e[0].actor_kind, AuditActorKind::Owner);
    assert!(e[0].object.mailbox.is_some());
    assert_eq!(e[0].object.message, None);
}
