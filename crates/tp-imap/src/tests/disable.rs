//! Заблокированная учётная запись (IMAP-22): `AccountDisabled` закрывает сессии IMAP (в том числе в `IDLE`) и
//! POP3, новый вход отклонён; после потери сигналов — сверка с хранилищем учётных записей.

use std::sync::Arc;
use std::time::Duration;

use tp_contracts::bus::EventBus;
use tp_contracts::meta::{
    AccountKind, AccountStore, AccountUpsert, MessageQuery, MessageStore, SpecialUse,
    account_disabled,
};
use tp_contracts::{AccountId, DomainId};

use super::{Client, Env, LOGIN, PASSWORD, TestConn};
use crate::disable::Sessions;
use crate::pop3::Pop3Session;
use crate::session::Session;

const PLAIN: &[u8] = b"From: a@example.org\r\nSubject: hi\r\n\r\nHello\r\n";

/// Строка ответа с ограничением времени.
async fn line(c: &mut Client) -> String {
    tokio::time::timeout(Duration::from_secs(5), c.line())
        .await
        .expect("no response in time")
}

/// Открытые сессии со слушателем сигналов шины стенда (подписка уже действует).
async fn sessions(env: &Env, accounts: Option<Arc<dyn AccountStore>>) -> Sessions {
    let sessions = Sessions::default();
    let before = env.bus.subscribers();
    sessions.start(
        Arc::new(env.bus.clone()),
        accounts,
        Duration::from_millis(10),
        env.shutdown.clone(),
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while env.bus.subscribers() <= before {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("watcher subscribed");
    sessions
}

/// Сессия IMAP после `LOGIN`, зарегистрированная в `sessions`.
async fn imap(env: &Env, sessions: &Sessions) -> Client {
    let (server, client) = tokio::io::duplex(1 << 20);
    let session = Session::new(
        env.ctx(),
        TestConn {
            io: server,
            tls: true,
        },
    )
    .with_sessions(sessions.clone());
    let mut c = Env::spawn(session, client).await;
    let r = c.cmd(&format!("L1 LOGIN {LOGIN} {PASSWORD}")).await;
    assert!(r.contains("L1 OK"), "{r}");
    c
}

#[tokio::test]
async fn idle_session_gets_bye_and_new_login_rejected() {
    let env = Env::new().await;
    let sessions = sessions(&env, None).await;
    let mut idle = imap(&env, &sessions).await;
    assert!(idle.cmd("S1 SELECT INBOX").await.contains("S1 OK"));
    let mut waiting = imap(&env, &sessions).await;
    idle.send("I1 IDLE\r\n").await;
    assert_eq!(line(&mut idle).await, "+ idling\r\n");

    // Сигнал другой учётной записи сессии не закрывает.
    env.bus
        .publish(account_disabled(tp_contracts::meta::AccountScope {
            tenant: env.scope().tenant,
            account: AccountId::new(),
        }))
        .await
        .expect("publish");
    // Вход запрещён: пароль отозван, сигнал опубликован после фиксации.
    env.auth.disable(LOGIN);
    env.bus
        .publish(account_disabled(env.scope()))
        .await
        .expect("publish");

    assert_eq!(line(&mut idle).await, "* BYE Account disabled\r\n");
    assert_eq!(idle.rest().await, "");
    idle.task.await.expect("join").expect("session");
    assert_eq!(line(&mut waiting).await, "* BYE Account disabled\r\n");
    assert_eq!(waiting.rest().await, "");

    let mut fresh = env.client(true).await;
    let r = fresh.cmd(&format!("L2 LOGIN {LOGIN} {PASSWORD}")).await;
    assert!(r.starts_with("L2 NO"), "{r}");
}

#[tokio::test]
async fn lost_signal_reconciled_with_account_store() {
    let env = Env::new().await;
    let sessions = sessions(&env, Some(Arc::new(env.meta.clone()))).await;
    let mut c = imap(&env, &sessions).await;
    // Вход разрешён — переподписка сессию не трогает.
    env.bus.disconnect();
    assert!(c.cmd("N1 NOOP").await.contains("N1 OK"));
    // Вход запрещён, сигнал потерян (обрыв потока): после переподписки — сверка с хранилищем.
    env.meta
        .upsert(
            env.scope().tenant,
            AccountUpsert {
                domain: DomainId::new(),
                login: LOGIN.into(),
                display_name: "User".into(),
                external_id: None,
                enabled: false,
                aliases: Vec::new(),
                kind: AccountKind::User,
            },
        )
        .await
        .expect("disable");
    env.bus.disconnect();
    assert_eq!(line(&mut c).await, "* BYE Account disabled\r\n");
    assert_eq!(c.rest().await, "");
}

#[tokio::test]
async fn pop3_session_closed_without_update() {
    let env = Env::new().await;
    let inbox = env.mailbox(SpecialUse::Inbox);
    env.deliver(inbox, PLAIN, Default::default()).await;
    let sessions = sessions(&env, None).await;
    let (server, client) = tokio::io::duplex(1 << 20);
    let session = Pop3Session::new(
        env.ctx(),
        TestConn {
            io: server,
            tls: true,
        },
    )
    .with_sessions(sessions.clone());
    let mut c = Client {
        io: tokio::io::BufReader::new(client),
        task: tokio::spawn(session.run()),
    };
    assert_eq!(line(&mut c).await, "+OK The Post POP3 ready\r\n");
    c.send(&format!("USER {LOGIN}\r\n")).await;
    assert!(line(&mut c).await.starts_with("+OK"));
    c.send(&format!("PASS {PASSWORD}\r\n")).await;
    assert_eq!(line(&mut c).await, "+OK Logged in, 1 messages\r\n");
    c.send("DELE 1\r\n").await;
    assert!(line(&mut c).await.starts_with("+OK"));

    env.bus
        .publish(account_disabled(env.scope()))
        .await
        .expect("publish");
    assert_eq!(line(&mut c).await, "-ERR [SYS/PERM] Account disabled\r\n");
    assert_eq!(c.rest().await, "");
    c.task.await.expect("join").expect("session");
    // Без QUIT помеченное письмо не удалено.
    let left = env
        .meta
        .list(env.scope(), inbox, MessageQuery::All, 10)
        .await
        .expect("list");
    assert_eq!(left.len(), 1);
}
