//! Лимит «адресатов в час» (SMTP-43, решение 113): сумма получателей писем учётной записи за скользящий час —
//! `Policy::rcpts_per_hour`; сверх предела — `451 4.7.1` и событие отказа в журнале доставки, submission и JMAP
//! ([`Delivery::submit`]).

use tp_contracts::delivery::{DeliveryDecision, DeliveryEvent, DeliveryEventKind};
use tp_contracts::membership::NodeId;
use tp_contracts::meta::Policy;
use tp_testkit::MemDeliveryLog;

use super::org_policy::{ALICE, alice_policy, submission};
use super::*;
use crate::SendLimits;
use crate::tracking::Tracking;

/// Предел 2 000 адресатов в час; одно письмо — до 1 000 получателей.
fn policy() -> Policy {
    Policy {
        rcpts_per_hour: Some(2000),
        max_recipients: Some(1000),
        ..Policy::default()
    }
}

/// 900 внешних адресов письма `n`.
fn batch(n: usize) -> Vec<String> {
    (0..900)
        .map(|i| format!("r{n}-{i}@ext.example.net"))
        .collect()
}

fn tracking(log: &MemDeliveryLog) -> Tracking {
    Tracking::new(Arc::new(log.clone()), NodeId::new("node-1").expect("node"))
}

/// Единственный отказ журнала — отложенное письмо с `451 4.7.1` всем получателям `to`.
fn assert_limited(events: &[DeliveryEvent], to: &[String]) {
    let rejected: Vec<_> = events
        .iter()
        .filter(|e| e.kind == DeliveryEventKind::Reject)
        .collect();
    assert_eq!(rejected.len(), 1, "{rejected:?}");
    assert_eq!(rejected[0].decision, DeliveryDecision::Defer);
    assert_eq!(rejected[0].sender, ALICE);
    assert_eq!(rejected[0].recipients, to);
    let reason = rejected[0].reason.as_deref().unwrap_or_default();
    assert!(reason.starts_with("451 4.7.1 "), "{reason}");
}

/// Submission: 3 письма по 900 адресатов при пределе 2 000 — третье отклоняется на данных, `MAIL` третьего
/// принят (писем в час не превышено).
#[tokio::test]
async fn submission_third_message_over_limit() {
    let env = Env::new(DecisionHooks::default()).await;
    let policies = alice_policy(&env, policy()).await;
    let log = MemDeliveryLog::new();
    let d = env
        .delivery(true)
        .with_policies(Arc::new(policies))
        .with_tracking(tracking(&log));
    let (mut c, task) = submission(&env, d).await;
    for (n, expect) in [(0, 250), (1, 250), (2, 451)] {
        assert_eq!(c.cmd(&format!("MAIL FROM:<{ALICE}>")).await, 250, "{n}");
        for r in batch(n) {
            assert_eq!(c.cmd(&format!("RCPT TO:<{r}>")).await, 250, "{n} {r}");
        }
        assert_eq!(c.cmd("DATA").await, 354);
        c.send("Subject: hi\r\n\r\nhello\r\n.\r\n").await;
        assert_eq!(c.code().await, expect, "письмо {n}");
    }
    assert_eq!(c.cmd("QUIT").await, 221);
    task.await.expect("join").expect("io");
    assert_eq!(env.queue.0.lock().expect("lock").len(), 2);
    assert_limited(&log.events(), &batch(2));
}

/// JMAP (`Delivery::submit`): тот же предел и те же счётчики — третья отправка по 900 адресатов не уходит,
/// все получатели — `451 4.7.1`.
#[tokio::test]
async fn submit_third_message_over_limit() {
    let env = Env::new(DecisionHooks::default()).await;
    let policies = alice_policy(&env, policy()).await;
    let log = MemDeliveryLog::new();
    let d = env
        .delivery(true)
        .with_policies(Arc::new(policies))
        .with_tracking(tracking(&log))
        .with_send_limits(Arc::new(SendLimits::new()));
    for n in 0..2 {
        let sent = d
            .submit(
                TenantId::DEFAULT,
                ALICE,
                &batch(n),
                Bytes::from_static(MAIL),
            )
            .await
            .expect("submit");
        assert!(
            sent.rcpts.iter().all(|(_, s)| *s == SubmitStatus::Queued),
            "{n}"
        );
    }
    let third = batch(2);
    let sent = d
        .submit(TenantId::DEFAULT, ALICE, &third, Bytes::from_static(MAIL))
        .await
        .expect("submit");
    assert_eq!(sent.rcpts.len(), 900);
    assert!(
        sent.rcpts
            .iter()
            .all(|(_, s)| matches!(s, SubmitStatus::Failed(r) if r.starts_with("451 4.7.1 "))),
        "{:?}",
        sent.rcpts.first()
    );
    assert_eq!(env.queue.0.lock().expect("lock").len(), 2);
    assert_limited(&log.events(), &third);
}
