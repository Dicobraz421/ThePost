//! SMTP-45: submission после входа принимает только адреса отправителя вошедшего пользователя (аналог
//! `reject_sender_login_mismatch`): конверт `MAIL FROM` и заголовки `From`, `Sender`; smtp-in не затрагивается.

use tp_contracts::DomainId;
use tp_contracts::auth::SendRight;
use tp_contracts::meta::{AccountKind, AccountUpsert};

use super::invite::SendRights;
use super::org_policy::{ALICE, principal, submission};
use super::*;

const BOB: &str = "bob@example.org";

/// Доставка с правами отправки: у alice псевдоним a.smith@, право Send As на sales@ и Send on Behalf на boss@.
async fn delivery(env: &Env) -> Delivery {
    AccountStore::upsert(
        &env.meta,
        TenantId::DEFAULT,
        AccountUpsert {
            domain: DomainId::new(),
            login: ALICE.into(),
            display_name: ALICE.into(),
            external_id: None,
            enabled: true,
            aliases: vec!["A.Smith@example.org".into()],
            kind: AccountKind::User,
            hidden_from_gal: false,
            resource_type: None,
        },
    )
    .await
    .expect("alias");
    let rights = Arc::new(SendRights::default());
    rights.grant(ALICE, "sales@example.org", SendRight::SendAs);
    rights.grant(ALICE, "boss@example.org", SendRight::SendOnBehalf);
    env.delivery(false).with_authz(rights)
}

/// Письмо bob с `From: <from>`.
fn letter(from: &str) -> Bytes {
    Bytes::from(format!(
        "From: <{from}>\r\nTo: {BOB}\r\nSubject: hi\r\n\r\nbody\r\n"
    ))
}

/// Транзакция submission bob: вошла alice, отправитель конверта — `mail_from`.
async fn tx(env: &Env, mail_from: &str) -> Envelope {
    let mut e = envelope(Mode::Submission, &[BOB]);
    e.mail_from = mail_from.into();
    e.principal = Some(principal(env).await);
    e
}

#[tokio::test]
async fn own_address_alias_and_send_as_pass() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = delivery(&env).await;
    // Конверт: свой адрес, псевдоним (в любом регистре), Send As и пустой отправитель уведомления.
    for addr in [ALICE, "A.Smith@example.org", "sales@example.org", ""] {
        let r = d.sender(&tx(&env, addr).await).await;
        assert!(r.is_none(), "{addr}: {r:?}");
    }
    // Заголовок From: свой, псевдоним, Send As — без изменений.
    for from in [ALICE, "a.smith@example.org", "sales@example.org"] {
        let r = d.data(&tx(&env, ALICE).await, letter(from)).await;
        assert_eq!(r.code, 250, "{from}: {r:?}");
    }
    // Send on Behalf: From — владелец, конверт свой, Sender — вошедший пользователь (добавлен узлом).
    let r = d
        .data(&tx(&env, ALICE).await, letter("boss@example.org"))
        .await;
    assert_eq!(r.code, 250, "{r:?}");
    // Sender уже указан клиентом — второй не добавляется.
    let r = d
        .data(
            &tx(&env, ALICE).await,
            Bytes::from(format!(
                "From: <boss@example.org>\r\nSender: {ALICE}\r\n\r\nbody\r\n"
            )),
        )
        .await;
    assert_eq!(r.code, 250, "{r:?}");
    let inbox = env.inbox(BOB).await;
    assert_eq!(inbox.len(), 5);
    let with_sender: Vec<_> = inbox
        .iter()
        .filter(|m| m.contains(&format!("Sender: {ALICE}\r\n")))
        .collect();
    assert_eq!(with_sender.len(), 2, "{inbox:?}");
    assert!(
        with_sender
            .iter()
            .all(|m| m.contains("From: <boss@example.org>") && m.matches("Sender:").count() == 1),
        "{with_sender:?}"
    );
}

#[tokio::test]
async fn foreign_mail_from_refused() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = delivery(&env).await;
    // Коллега без права; адрес с правом только Send on Behalf — конверт остаётся своим.
    for addr in ["carol@example.org", "boss@example.org"] {
        let r = d.sender(&tx(&env, addr).await).await.expect("refused");
        assert_eq!((r.code, r.esc), (553, [5, 7, 1]), "{addr}");
    }
}

#[tokio::test]
async fn foreign_from_with_own_mail_from_refused() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = delivery(&env).await;
    let e = tx(&env, ALICE).await;
    let forged: [&[u8]; 6] = [
        b"From: <carol@example.org>\r\n\r\nbody\r\n",
        // Список адресов и второе поле From: каждый адрес — свой.
        b"From: alice@example.org, carol@example.org\r\n\r\nbody\r\n",
        b"From: alice@example.org\r\nFrom: carol@example.org\r\n\r\nbody\r\n",
        // Sender — тоже адрес отправителя: чужой и по праву Send on Behalf — отказ.
        b"From: alice@example.org\r\nSender: carol@example.org\r\n\r\nbody\r\n",
        b"From: alice@example.org\r\nSender: boss@example.org\r\n\r\nbody\r\n",
        // Поле без адреса не проверить.
        b"From: undisclosed:;\r\n\r\nbody\r\n",
    ];
    for raw in forged {
        let r = d.data(&e, Bytes::from_static(raw)).await;
        assert_eq!(
            (r.code, r.esc),
            (550, [5, 7, 1]),
            "{}: {r:?}",
            String::from_utf8_lossy(raw)
        );
    }
    assert!(env.inbox(BOB).await.is_empty());
}

#[tokio::test]
async fn external_domain_refused() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = delivery(&env).await;
    let r = d
        .sender(&tx(&env, "boss@other.test").await)
        .await
        .expect("refused");
    assert_eq!((r.code, r.esc), (553, [5, 7, 1]));
    let r = d
        .data(&tx(&env, ALICE).await, letter("boss@other.test"))
        .await;
    assert_eq!((r.code, r.esc), (550, [5, 7, 1]));
    assert!(env.inbox(BOB).await.is_empty());
}

/// Сеанс submission (отчёт приёмки CMP-01, ошибка 1): чужой `MAIL FROM` — `553`, чужой `From` при своём
/// конверте — `550` на данных; письмо не доставлено.
#[tokio::test]
async fn session_refuses_foreign_sender() {
    let env = Env::new(DecisionHooks::default()).await;
    let (mut c, task) = submission(&env, delivery(&env).await).await;
    assert_eq!(c.cmd("MAIL FROM:<carol@example.org>").await, 553);
    assert_eq!(c.cmd("MAIL FROM:<boss@other.test>").await, 553);
    assert_eq!(c.cmd(&format!("MAIL FROM:<{ALICE}>")).await, 250);
    assert_eq!(c.cmd(&format!("RCPT TO:<{BOB}>")).await, 250);
    assert_eq!(c.cmd("DATA").await, 354);
    c.send("From: carol@example.org\r\nSubject: hi\r\n\r\nhello\r\n.\r\n")
        .await;
    assert_eq!(c.code().await, 550);
    assert_eq!(c.cmd("QUIT").await, 221);
    task.await.expect("join").expect("io");
    assert!(env.inbox(BOB).await.is_empty());
}

/// Приём извне (25, без входа) проверка не затрагивает: внешний отправитель и его `From` — как раньше.
#[tokio::test]
async fn inbound_not_affected() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = delivery(&env).await;
    let mut e = envelope(Mode::Inbound, &[BOB]);
    e.mail_from = "boss@other.test".into();
    assert!(d.sender(&e).await.is_none());
    assert_eq!(d.data(&e, letter("boss@other.test")).await.code, 250);
    assert_eq!(env.inbox(BOB).await.len(), 1);
}
