//! Личные списки отправителей при доставке (SMTP-25) на in-memory списках `MemSenderLists`.

use tp_contracts::senders::{SenderList, SenderLists};
use tp_testkit::MemSenderLists;

use super::*;
use crate::config::MilterConfig;
use crate::milter::proto::{self, Packet};
use crate::milter::tests as mt;

async fn lists(env: &Env, login: &str, safe: &[&str], blocked: &[&str]) -> Arc<MemSenderLists> {
    let lists = Arc::new(MemSenderLists::new());
    let own = |l: &[&str]| l.iter().map(|e| (*e).to_owned()).collect();
    lists
        .put_sender_list(
            env.scope(login).await,
            SenderList {
                safe: own(safe),
                blocked: own(blocked),
            },
        )
        .await
        .expect("put");
    lists
}

#[tokio::test]
async fn blocked_sender_goes_to_junk_only_for_list_owner() {
    let env = Env::new(DecisionHooks::default()).await;
    // Адрес `From` письма (MAIL: sender@example.net) — в заблокированных у bob.
    let l = lists(&env, "bob@example.org", &[], &["Sender@Example.net"]).await;
    let d = env.delivery(false).with_sender_lists(l);
    let mut e = envelope(Mode::Inbound, &["bob@example.org", "carol@example.org"]);
    e.mail_from = "bounce-17@lists.example.net".into();
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.junk("bob@example.org").await, 1);
    assert!(env.inbox("bob@example.org").await.is_empty());
    // У carol списка нет: письмо во «Входящих».
    assert_eq!(env.inbox("carol@example.org").await.len(), 1);
    assert_eq!(env.junk("carol@example.org").await, 0);
}

#[tokio::test]
async fn blocked_domain_of_envelope_sender_and_internal_sender() {
    let env = Env::new(DecisionHooks::default()).await;
    let l = lists(
        &env,
        "bob@example.org",
        &[],
        &["@lists.example.net", "alice@example.org"],
    )
    .await;
    let d = env.delivery(false).with_sender_lists(l);
    // Домен отправителя конверта; `From` письма в списках не значится.
    let mut e = envelope(Mode::Inbound, &["bob@example.org"]);
    e.mail_from = "bounce-17@lists.example.net".into();
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.junk("bob@example.org").await, 1);
    // Отправка вне сессии SMTP (JMAP) от заблокированного коллеги — тоже в «Спам».
    let sent = d
        .submit(
            TenantId::DEFAULT,
            "alice@example.org",
            &rcpts(&["bob@example.org"]),
            Bytes::from_static(b"From: alice@example.org\r\nSubject: hi\r\n\r\nbody\r\n"),
        )
        .await
        .expect("submit");
    assert_eq!(sent.rcpts[0].1, SubmitStatus::Delivered);
    assert_eq!(env.junk("bob@example.org").await, 2);
    assert!(env.inbox("bob@example.org").await.is_empty());
}

#[tokio::test]
async fn safe_sender_overrides_filter_verdict() {
    let env = Env::new(DecisionHooks::default()).await;
    let filter = mt::Mock::start(
        0,
        mt::on_eob(vec![
            Packet::strings(proto::R_QUARANTINE, &[b"spam"]),
            mt::reply_of(proto::R_CONTINUE),
        ]),
    )
    .await;
    let l = lists(&env, "bob@example.org", &["example.net"], &[]).await;
    let mut d = env.delivery(false).with_sender_lists(l);
    d.deps.milter = Some(Arc::new(Milters::new(
        MilterConfig {
            filters: vec![filter.filter("rspamd")],
            ..MilterConfig::default()
        },
        "mx.example.org",
    )));
    let e = envelope(Mode::Inbound, &["bob@example.org", "carol@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    // Надёжный отправитель: у bob — во «Входящих», у carol — вердикт фильтра.
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    assert_eq!(env.junk("bob@example.org").await, 0);
    assert_eq!(env.junk("carol@example.org").await, 1);
    assert!(env.inbox("carol@example.org").await.is_empty());
}

#[tokio::test]
async fn safe_sender_does_not_override_dmarc_failure() {
    use crate::authcheck::tests as fx;
    let env = Env::new(DecisionHooks::default()).await;
    let check = fx::checker();
    let signer = fx::Signer::new("example.com", "s1");
    signer.publish(check.cache());
    fx::publish_sender(check.cache(), "quarantine");
    let l = lists(&env, "bob@example.org", &["sender@example.com"], &[]).await;
    let mut d = env.delivery(false).with_sender_lists(l);
    d.deps.authcheck = Some(Arc::new(check));
    // Подделка адреса надёжного отправителя: DMARC не пройден — в «Спам».
    let forged = signer.sign(fx::MAIL).replace("body", "forged");
    let r = d
        .data(&fx::envelope("203.0.113.5:1000"), Bytes::from(forged))
        .await;
    assert_eq!(r.code, 250);
    assert_eq!(env.junk("bob@example.org").await, 1);
    assert!(env.inbox("bob@example.org").await.is_empty());
    // Настоящее письмо надёжного отправителя — во «Входящие».
    let r = d
        .data(
            &fx::envelope("192.0.2.10:1000"),
            Bytes::from(signer.sign(fx::MAIL)),
        )
        .await;
    assert_eq!(r.code, 250);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
}
