//! Волна smtp-13: приём iMIP (SMTP-33) на заглушке `FakeItip` — приглашения и ответы извне; событие `Itip` журнала
//! доставки (SMTP-37).

use tp_contracts::delivery::{DeliveryDecision, DeliveryEvent, DeliveryEventKind};
use tp_contracts::membership::NodeId;
use tp_contracts::schedule::{ItipMethod, ItipOutcome, ItipSource};
use tp_testkit::{FakeItip, MemDeliveryLog};

use super::*;
use crate::authcheck::tests as fx;
use crate::tracking::Tracking;

/// Письмо iMIP от `sender@example.com` получателю `bob@example.org`.
fn imip(method: &str, organizer: &str, attendee: &str) -> String {
    format!(
        "From: Sender <sender@example.com>\r\nTo: bob@example.org\r\nSubject: meeting\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nmeeting\r\n\
         --b\r\nContent-Type: text/calendar; method={method}; charset=utf-8\r\n\r\nBEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\nMETHOD:{method}\r\nBEGIN:VEVENT\r\nUID:u1\r\nDTSTAMP:20261112T100000Z\r\n\
         DTSTART:20261113T100000Z\r\nORGANIZER;CN=Org:mailto:{organizer}\r\n\
         ATTENDEE;PARTSTAT=ACCEPTED:mailto:{attendee}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n--b--\r\n"
    )
}

/// Доставка с проверками отправителя (`example.com`: SPF `192.0.2.0/24`, DMARC `none`) и приёмом iTIP.
fn delivery(env: &Env) -> (Delivery, FakeItip) {
    let check = fx::checker();
    fx::publish_sender(check.cache(), "none");
    let itip = FakeItip::new();
    let mut d = env.delivery(false).with_itip(Arc::new(itip.clone()));
    d.deps.authcheck = Some(Arc::new(check));
    (d, itip)
}

/// Письмо с адреса из SPF отправителя: DMARC пройден.
async fn send(d: &Delivery, raw: &str) -> u16 {
    d.data(
        &fx::envelope("192.0.2.10:1000"),
        Bytes::from(raw.to_owned()),
    )
    .await
    .code
}

#[tokio::test]
async fn request_reply_cancel_reach_itip_and_stay_in_inbox() {
    let env = Env::new(DecisionHooks::default()).await;
    let (d, itip) = delivery(&env);
    let request = imip("REQUEST", "sender@example.com", "bob@example.org");
    let reply = imip("REPLY", "bob@example.org", "Sender@Example.com");
    let cancel = imip("CANCEL", "sender@example.com", "bob@example.org");
    for raw in [&request, &reply, &cancel] {
        assert_eq!(send(&d, raw).await, 250);
    }
    let calls = itip.calls();
    assert_eq!(
        calls.iter().map(|c| c.method).collect::<Vec<_>>(),
        [ItipMethod::Request, ItipMethod::Reply, ItipMethod::Cancel]
    );
    let bob = env.scope("bob@example.org").await;
    for c in &calls {
        assert_eq!(c.recipient, bob);
        assert_eq!(c.to, "bob@example.org");
        assert_eq!(c.from, "sender@example.com");
        assert_eq!(c.source, ItipSource::Imip);
        assert!(c.ics.starts_with("BEGIN:VCALENDAR") && c.ics.contains("UID:u1"));
    }
    assert!(calls[1].ics.contains("METHOD:REPLY"));
    assert_eq!(env.inbox("bob@example.org").await.len(), 3);
}

#[tokio::test]
async fn mail_without_itip_does_not_call() {
    let env = Env::new(DecisionHooks::default()).await;
    let (d, itip) = delivery(&env);
    assert_eq!(send(&d, fx::MAIL).await, 250);
    // Календарь-вложение без параметра `method` — не iMIP.
    let plain =
        imip("REQUEST", "sender@example.com", "bob@example.org").replace(" method=REQUEST;", "");
    assert_eq!(send(&d, &plain).await, 250);
    assert!(itip.calls().is_empty());
    assert_eq!(env.inbox("bob@example.org").await.len(), 2);
}

#[tokio::test]
async fn forged_sender_is_ignored() {
    let env = Env::new(DecisionHooks::default()).await;
    let (d, itip) = delivery(&env);
    // Отправитель — не организатор приглашения и не участник ответа.
    let request = imip("REQUEST", "boss@example.com", "bob@example.org");
    let reply = imip("REPLY", "bob@example.org", "boss@example.com");
    assert_eq!(send(&d, &request).await, 250);
    assert_eq!(send(&d, &reply).await, 250);
    // Адрес организатора в `From`, но письмо не прошло DMARC (адрес вне SPF, подписи нет).
    let spoofed = imip("REQUEST", "sender@example.com", "bob@example.org");
    let r = d
        .data(
            &fx::envelope("203.0.113.5:1000"),
            Bytes::from(spoofed.clone()),
        )
        .await;
    assert_eq!(r.code, 250);
    // Submission без входа: ни DMARC, ни учётной записи отправителя — iTIP не применяется.
    let mut e = fx::envelope("192.0.2.10:1000");
    e.mode = Mode::Submission;
    assert_eq!(d.data(&e, Bytes::from(spoofed)).await.code, 250);
    assert!(itip.calls().is_empty());
    // Письма остаются в ящике.
    assert_eq!(env.inbox("bob@example.org").await.len(), 4);
}

#[tokio::test]
async fn itip_refusal_and_failure_do_not_affect_delivery() {
    let env = Env::new(DecisionHooks::default()).await;
    let (d, itip) = delivery(&env);
    let request = imip("REQUEST", "sender@example.com", "bob@example.org");
    itip.set("bob@example.org", ItipOutcome::Ignored("stale".into()));
    assert_eq!(send(&d, &request).await, 250);
    itip.fail_next(Error::UNAVAILABLE);
    assert_eq!(send(&d, &request).await, 250);
    assert_eq!(env.inbox("bob@example.org").await.len(), 2);
}

#[tokio::test]
async fn each_recipient_account_once_without_subaddress() {
    let env = Env::new(DecisionHooks::default()).await;
    let (d, itip) = delivery(&env);
    let request = imip("REQUEST", "sender@example.com", "bob@example.org");
    let mut e = fx::envelope("192.0.2.10:1000");
    e.rcpts = vec![
        "Bob+cal@example.org".into(),
        "bob@example.org".into(),
        "alice@example.org".into(),
    ];
    assert_eq!(d.data(&e, Bytes::from(request)).await.code, 250);
    let mut to: Vec<String> = itip.calls().into_iter().map(|c| c.to).collect();
    to.sort();
    assert_eq!(to, ["alice@example.org", "bob@example.org"]);
}

/// Доставка [`delivery`] с журналом доставки.
fn tracked(env: &Env) -> (Delivery, FakeItip, MemDeliveryLog) {
    let (d, itip) = delivery(env);
    let log = MemDeliveryLog::new();
    let node = NodeId::new("node-1").expect("node");
    (
        d.with_tracking(Tracking::new(Arc::new(log.clone()), node)),
        itip,
        log,
    )
}

/// События `Itip` журнала.
fn itip_events(log: &MemDeliveryLog) -> Vec<DeliveryEvent> {
    log.events()
        .into_iter()
        .filter(|e| e.kind == DeliveryEventKind::Itip)
        .collect()
}

/// SMTP-37: приём `REQUEST` — событие `Itip` получателю (метод, итог) рядом с доставкой в ящик.
#[tokio::test]
async fn request_is_tracked() {
    let env = Env::new(DecisionHooks::default()).await;
    let (d, _itip, log) = tracked(&env);
    let request = imip("REQUEST", "sender@example.com", "bob@example.org");
    assert_eq!(send(&d, &request).await, 250);
    let events = itip_events(&log);
    assert_eq!(events.len(), 1, "{events:?}");
    let e = &events[0];
    assert_eq!(e.decision, DeliveryDecision::Accept);
    assert_eq!(e.reason.as_deref(), Some("REQUEST"));
    assert_eq!(e.recipients, ["bob@example.org"]);
    assert!(
        log.events()
            .iter()
            .any(|e| e.kind == DeliveryEventKind::Deliver)
    );
}

/// SMTP-37: пропуск — `Discard` с причиной; сбой приёма и письмо без доверия к `From` — без события `Itip`.
#[tokio::test]
async fn ignored_is_tracked_with_reason() {
    let env = Env::new(DecisionHooks::default()).await;
    let (d, itip, log) = tracked(&env);
    let request = imip("REQUEST", "sender@example.com", "bob@example.org");
    itip.set("bob@example.org", ItipOutcome::Ignored("stale".into()));
    assert_eq!(send(&d, &request).await, 250);
    itip.fail_next(Error::UNAVAILABLE);
    assert_eq!(send(&d, &request).await, 250);
    let forged = imip("REQUEST", "boss@example.com", "bob@example.org");
    assert_eq!(send(&d, &forged).await, 250);
    let events = itip_events(&log);
    assert_eq!(events.len(), 1, "{events:?}");
    assert_eq!(events[0].decision, DeliveryDecision::Discard);
    assert_eq!(events[0].reason.as_deref(), Some("REQUEST: stale"));
}
