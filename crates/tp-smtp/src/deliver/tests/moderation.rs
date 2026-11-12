//! FIX-01: решения модератора (`impl Moderation for Delivery`, CTR-31, CTR-43) — одобренное письмо участникам группы
//! ровно один раз, письмо правила потока — исходным получателям, отказ — уведомление отправителю через очередь.

use tp_contracts::groups::{
    DistributionGroup, GroupMember, GroupStore, HeldMessage, HoldSource, Moderation, Moderator,
    SenderRestriction,
};
use tp_contracts::{Error, GroupId, HeldId, TenantId};
use tp_testkit::suite::{self, ModerationFixture};

use super::*;
use crate::groups::Groups;

const T: TenantId = TenantId::DEFAULT;

impl Env {
    fn moderating(&self, outbound: bool) -> Delivery {
        self.delivery(outbound).with_groups(Groups::new(
            Arc::new(self.groups.clone()),
            Arc::new(self.blobs.clone()),
            Arc::new(self.index.clone()),
        ))
    }

    /// Группа `address` с участниками `members` и модератором carol.
    async fn moderated(&self, address: &str, members: Vec<GroupMember>) -> DistributionGroup {
        let g = DistributionGroup {
            id: GroupId::new(),
            tenant: T,
            name: address.into(),
            address: Some(address.into()),
            external_id: None,
            members,
            senders: SenderRestriction::Anyone,
            moderators: vec![self.scope("carol@example.org").await.account],
            hidden_from_gal: false,
        };
        self.groups.put(g.clone()).await.expect("put group");
        g
    }

    async fn member(&self, login: &str) -> GroupMember {
        GroupMember::Account(self.scope(login).await.account)
    }

    /// Письмо внешнего отправителя в группу через smtp-in: ждёт модератора; возвращает его, забранное из очереди
    /// (`GroupStore::take_held`, как Admin API).
    async fn take(&self, d: &Delivery, group: &DistributionGroup, tag: &str) -> HeldMessage {
        let address = group.address.clone().expect("address");
        let e = envelope(Mode::Inbound, &[address.as_str()]);
        let raw = format!(
            "From: sender@example.net\r\nTo: {address}\r\nSubject: {tag}\r\nMessage-ID: <{tag}@example.net>\r\n\r\nbody\r\n"
        );
        assert_eq!(d.data(&e, Bytes::from(raw)).await.code, 250);
        let held = self
            .groups
            .held(T, Some(group.id), 100)
            .await
            .expect("held")
            .into_iter()
            .find(|h| h.message_id.as_deref() == Some(format!("{tag}@example.net").as_str()))
            .expect("письмо в очереди модерации");
        self.groups.take_held(T, held.id).await.expect("take")
    }

    fn refs(&self, h: &HeldMessage) -> u32 {
        self.index.row(T, &h.blob.hash).map_or(0, |r| r.refcnt)
    }

    fn queued(&self) -> Vec<(String, Vec<String>, Bytes)> {
        self.queue.0.lock().expect("lock").clone()
    }
}

/// Одобрено: каждый участник (с вложенной группой, в которой часть участников повторяется) получает письмо один раз,
/// без повторной модерации; повторное решение по тому же письму — `NotFound` очереди.
#[tokio::test]
async fn approved_message_delivered_to_each_member_once() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.moderating(true);
    let team = DistributionGroup {
        id: GroupId::new(),
        tenant: T,
        name: "team".into(),
        address: None,
        external_id: None,
        members: vec![
            env.member("alice@example.org").await,
            env.member("carol@example.org").await,
        ],
        senders: SenderRestriction::Anyone,
        moderators: Vec::new(),
        hidden_from_gal: false,
    };
    env.groups.put(team.clone()).await.expect("team");
    let g = env
        .moderated(
            "news@example.org",
            vec![
                env.member("alice@example.org").await,
                env.member("bob@example.org").await,
                GroupMember::Group(team.id),
            ],
        )
        .await;
    let h = env.take(&d, &g, "approve-1").await;
    for login in ["alice@example.org", "bob@example.org", "carol@example.org"] {
        assert!(env.inbox(login).await.is_empty(), "{login}: до решения");
    }

    d.deliver(T, Some(&g), &h).await.expect("deliver");
    for login in ["alice@example.org", "bob@example.org", "carol@example.org"] {
        let inbox = env.inbox(login).await;
        assert_eq!(inbox.len(), 1, "{login}: {inbox:?}");
        assert!(inbox[0].contains("Subject: approve-1"), "{}", inbox[0]);
    }
    // Ничего наружу и ни одного уведомления отправителю.
    assert!(env.queued().is_empty(), "{:?}", env.queued());
    assert!(
        env.groups
            .held(T, Some(g.id), 100)
            .await
            .expect("held")
            .is_empty()
    );
    assert_eq!(env.groups.take_held(T, h.id).await, Err(Error::NotFound));
}

/// Отклонено: отправителю — DSN (RFC 3464) с причиной модератора и адресом группы через очередь, участникам ничего;
/// ссылка письма очереди на объект снята.
#[tokio::test]
async fn rejected_message_bounces_to_sender() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.moderating(true);
    let g = env
        .moderated(
            "news@example.org",
            vec![env.member("alice@example.org").await],
        )
        .await;
    let h = env.take(&d, &g, "reject-1").await;
    assert_eq!(env.refs(&h), 1);

    for bad in ["", "a\nb"] {
        assert!(matches!(d.reject(T, &h, bad).await, Err(Error::Invalid(_))));
    }
    assert!(env.queued().is_empty());
    d.reject(T, &h, "off-topic").await.expect("reject");
    let queued = env.queued();
    assert_eq!(queued.len(), 1, "{queued:?}");
    let (from, rcpts, raw) = &queued[0];
    assert_eq!(from, "");
    assert_eq!(rcpts, &vec!["sender@example.net".to_owned()]);
    let dsn = String::from_utf8_lossy(raw);
    assert!(dsn.contains("multipart/report"), "{dsn}");
    assert!(
        dsn.contains("Final-Recipient: rfc822; news@example.org"),
        "{dsn}"
    );
    assert!(dsn.contains("Status: 5.7.1"), "{dsn}");
    assert!(dsn.contains("off-topic"), "{dsn}");
    assert!(dsn.contains("Subject: reject-1"), "{dsn}");
    assert!(env.inbox("alice@example.org").await.is_empty());
    assert_eq!(env.refs(&h), 0);
    assert_eq!(env.groups.take_held(T, h.id).await, Err(Error::NotFound));
}

/// Общий набор `Moderation` tp-testkit на реальных письмах очереди.
#[tokio::test]
async fn moderation_suite() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.moderating(true);
    let g = env
        .moderated(
            "news@example.org",
            vec![env.member("bob@example.org").await],
        )
        .await;
    let fx = ModerationFixture {
        tenant: T,
        group: g.clone(),
        approve: env.take(&d, &g, "suite-a").await,
        decline: env.take(&d, &g, "suite-d").await,
    };
    suite::moderation(&d, &fx).await;
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    // Решение по письму другой организации — `Invalid`.
    assert!(matches!(
        d.reject(TenantId::new(), &fx.decline, "off-topic").await,
        Err(Error::Invalid(_))
    ));
}

/// Письмо правила потока `n` (объект — в хранилище, одна ссылка): bob и внешний получатель, модератор carol.
async fn rule_held(env: &Env, raw: &Bytes, n: u8) -> HeldMessage {
    let blob = crate::queue::store_blob(&env.blobs, &env.index, T, raw.clone())
        .await
        .expect("blob");
    HeldMessage {
        id: HeldId::new(),
        group: None,
        source: HoldSource::FlowRule {
            rule: "Внешним — с одобрения".into(),
        },
        moderators: vec![Moderator::Account(
            env.scope("carol@example.org").await.account,
        )],
        recipients: vec!["bob@example.org".into(), "x@remote.example".into()],
        sender: "s@example.net".into(),
        sender_account: None,
        message_id: Some(format!("rule-{n}@example.net")),
        blob,
        size: raw.len() as u64,
        held_at: OffsetDateTime::now_utc(),
    }
}

/// Письмо правила потока (CTR-43, без группы): одобрено — локальному получателю в ящик, внешнему — в очередь
/// от исходного отправителя; отказ — DSN с исходными получателями.
#[tokio::test]
async fn flow_rule_message_delivered_to_original_recipients() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.moderating(true);
    let raw = Bytes::from_static(b"From: s@example.net\r\nSubject: rule\r\n\r\nbody\r\n");
    let approve = rule_held(&env, &raw, 1).await;
    env.groups.hold(T, approve.clone()).await.expect("hold");
    let approve = env.groups.take_held(T, approve.id).await.expect("take");
    d.deliver(T, None, &approve).await.expect("deliver");
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    let queued = env.queued();
    assert_eq!(queued.len(), 1, "{queued:?}");
    assert_eq!(queued[0].0, "s@example.net");
    assert_eq!(queued[0].1, vec!["x@remote.example".to_owned()]);
    assert_eq!(&queued[0].2[..], &raw[..]);

    let decline = rule_held(&env, &raw, 2).await;
    d.reject(T, &decline, "not allowed").await.expect("reject");
    let queued = env.queued();
    assert_eq!(queued.len(), 2, "{queued:?}");
    let dsn = String::from_utf8_lossy(&queued[1].2);
    assert_eq!(queued[1].1, vec!["s@example.net".to_owned()]);
    assert!(
        dsn.contains("Final-Recipient: rfc822; bob@example.org"),
        "{dsn}"
    );
    assert!(
        dsn.contains("Final-Recipient: rfc822; x@remote.example"),
        "{dsn}"
    );
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
}
