//! Направление Milter исходящей почты (SMTP-27) на заглушке фильтра: отказ и изменение письма при отправке —
//! submission и отправка вне сессии SMTP (JMAP EmailSubmission).

use super::*;
use crate::config::{MilterConfig, MilterDirection, MilterFilter, MilterOnFailure};
use crate::milter::proto::{self, Packet};
use crate::milter::tests as mt;

fn outbound(mock: &mt::Mock, name: &str) -> MilterFilter {
    MilterFilter {
        direction: MilterDirection::Outbound,
        ..mock.filter(name)
    }
}

fn with_filters(env: &Env, filters: Vec<MilterFilter>) -> Delivery {
    let mut d = env.delivery(true);
    d.deps.milter = Some(Arc::new(Milters::new(
        MilterConfig {
            filters,
            ..MilterConfig::default()
        },
        "mx.example.org",
    )));
    d
}

fn reject() -> mt::Script {
    mt::on_eob(vec![Packet::strings(
        proto::R_REPLYCODE,
        &[b"550 5.7.1 Confidential data"],
    )])
}

#[tokio::test]
async fn submit_rejected_by_outbound_filter() {
    let env = Env::new(DecisionHooks::default()).await;
    let dlp = mt::Mock::start(0, reject()).await;
    // Фильтр приёма отправку не проверяет.
    let spam = mt::Mock::start(0, mt::on_eob(vec![mt::reply_of(proto::R_ACCEPT)])).await;
    let d = with_filters(&env, vec![spam.filter("rspamd"), outbound(&dlp, "dlp")]);
    let list = rcpts(&["bob@example.org", "x@example.net"]);
    let sent = d
        .submit(
            TenantId::DEFAULT,
            None,
            "alice@example.org",
            &list,
            Bytes::from_static(MAIL),
        )
        .await
        .expect("submit");
    for (_, status) in &sent.rcpts {
        assert_eq!(
            *status,
            SubmitStatus::Failed("550 5.7.1 Confidential data".into())
        );
    }
    assert!(env.inbox("bob@example.org").await.is_empty());
    assert!(env.queue.0.lock().expect("lock").is_empty());
    assert_eq!(spam.codes().await, "");
    // Фильтр видел конверт отправки.
    assert_eq!(
        dlp.packets(proto::C_MAIL)[0].split(),
        [&b"<alice@example.org>"[..]]
    );
    assert_eq!(dlp.packets(proto::C_RCPT).len(), 2);
}

#[tokio::test]
async fn submit_message_changed_by_outbound_filter() {
    let env = Env::new(DecisionHooks::default()).await;
    let dlp = mt::Mock::start(
        0,
        mt::on_eob(vec![
            mt::header_mod(proto::R_ADDHEADER, None, "X-DLP", "checked"),
            mt::reply_of(proto::R_ACCEPT),
        ]),
    )
    .await;
    let d = with_filters(&env, vec![outbound(&dlp, "dlp")]);
    let list = rcpts(&["bob@example.org", "x@example.net"]);
    let sent = d
        .submit(
            TenantId::DEFAULT,
            None,
            "alice@example.org",
            &list,
            Bytes::from_static(MAIL),
        )
        .await
        .expect("submit");
    assert_eq!(sent.rcpts[0].1, SubmitStatus::Delivered);
    assert_eq!(sent.rcpts[1].1, SubmitStatus::Queued);
    // Изменённое письмо — и в ящик, и в очередь.
    let inbox = env.inbox("bob@example.org").await;
    assert!(inbox[0].contains("\r\nX-DLP: checked\r\n"), "{}", inbox[0]);
    let queued = env.queue.0.lock().expect("lock").clone();
    assert_eq!(queued[0].1, ["x@example.net"]);
    assert!(queued[0].2.windows(16).any(|w| w == b"X-DLP: checked\r\n"));
}

#[tokio::test]
async fn submission_session_uses_outbound_direction() {
    let env = Env::new(DecisionHooks::default()).await;
    let dlp = mt::Mock::start(0, reject()).await;
    let d = with_filters(&env, vec![outbound(&dlp, "dlp")]);
    // Submission — отказ фильтра ответом на DATA.
    let e = envelope(Mode::Submission, &["bob@example.org"]);
    let r = d.data(&e, Bytes::from_static(MAIL)).await;
    assert_eq!((r.code, r.esc), (550, [5, 7, 1]));
    assert!(env.inbox("bob@example.org").await.is_empty());
    // Приём извне фильтры исходящей почты не проходит.
    let before = dlp.codes().await.len();
    let e = envelope(Mode::Inbound, &["bob@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    assert_eq!(dlp.codes().await.len(), before);
}

#[tokio::test]
async fn unavailable_outbound_filter_has_own_policy() {
    let env = Env::new(DecisionHooks::default()).await;
    let closed = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = closed.local_addr().expect("addr").to_string();
    drop(closed);
    let filter = |direction, on_failure| MilterFilter {
        name: "ext".into(),
        address: address.clone(),
        direction,
        on_failure,
        connect_timeout: std::time::Duration::from_secs(2),
        ..MilterFilter::default()
    };
    // Один и тот же недоступный фильтр: приём пропускает с пометкой, отправка — временный отказ.
    let d = with_filters(
        &env,
        vec![
            filter(MilterDirection::Inbound, Some(MilterOnFailure::Accept)),
            filter(MilterDirection::Outbound, None),
        ],
    );
    let e = envelope(Mode::Inbound, &["bob@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    let sent = d
        .submit(
            TenantId::DEFAULT,
            None,
            "alice@example.org",
            &rcpts(&["bob@example.org"]),
            Bytes::from_static(MAIL),
        )
        .await
        .expect("submit");
    assert!(matches!(&sent.rcpts[0].1, SubmitStatus::Failed(r) if r.starts_with("451 4.7.1")));
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    // Отложенная отправка с тем же отказом — в очередь не ставится (повтор безопасен).
    let at = time::OffsetDateTime::now_utc() + std::time::Duration::from_secs(60);
    let r = d
        .schedule(
            TenantId::DEFAULT,
            None,
            "alice@example.org",
            &rcpts(&["bob@example.org"]),
            Bytes::from_static(MAIL),
            at,
        )
        .await;
    assert!(matches!(r, Err(Error::Unavailable { .. })), "{r:?}");
}
