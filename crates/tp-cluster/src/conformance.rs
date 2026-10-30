//! Общий набор тестов поведения [`EventBus`]: шина в процессе и Valkey проходят одни и те же проверки.
//!
//! Проверки не полагаются на отсутствие уведомлений по таймауту: «чужое не пришло» проверяется тем,
//! что первым пришло своё уведомление, опубликованное после чужого.

use std::time::Duration;

use futures::StreamExt;
use tp_contracts::bus::{Event, EventBus, EventKind, EventStream};
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, MailboxId, ModSeq, TenantId};

/// Сколько ждать уведомления, прошедшего через внешнюю шину.
const WAIT: Duration = Duration::from_secs(5);

pub(crate) fn scope() -> AccountScope {
    AccountScope {
        tenant: TenantId::new(),
        account: AccountId::new(),
    }
}

pub(crate) fn mailbox_changed(scope: AccountScope, modseq: u64) -> Event {
    Event {
        tenant: scope.tenant,
        account: Some(scope.account),
        kind: EventKind::MailboxChanged {
            mailbox: MailboxId::from_uuid(*scope.account.as_uuid()),
            modseq: ModSeq(modseq),
        },
    }
}

pub(crate) fn system(kind: EventKind) -> Event {
    Event {
        tenant: TenantId::DEFAULT,
        account: None,
        kind,
    }
}

pub(crate) async fn next(stream: &mut EventStream) -> Option<Event> {
    tokio::time::timeout(WAIT, stream.next())
        .await
        .expect("уведомление не пришло")
}

/// Прогоняет все проверки на шине. Системный канал общий для шины — шина должна быть свежей.
pub(crate) async fn run(bus: &dyn EventBus) {
    delivers_to_account(bus).await;
    isolates_accounts_and_tenants(bus).await;
    fans_out_to_all_subscribers(bus).await;
    keeps_order(bus).await;
    separates_system_events(bus).await;
    publish_without_subscribers(bus).await;
    dropped_subscriber_does_not_block(bus).await;
}

async fn delivers_to_account(bus: &dyn EventBus) {
    let scope = scope();
    let mut sub = bus.subscribe(scope).await.expect("subscribe");
    let event = mailbox_changed(scope, 7);
    bus.publish(event.clone()).await.expect("publish");
    assert_eq!(next(&mut sub).await, Some(event));
    let event = Event {
        kind: EventKind::MailboxListChanged,
        ..mailbox_changed(scope, 0)
    };
    bus.publish(event.clone()).await.expect("publish");
    assert_eq!(next(&mut sub).await, Some(event));
}

async fn isolates_accounts_and_tenants(bus: &dyn EventBus) {
    let a = scope();
    let b = AccountScope {
        tenant: a.tenant,
        account: AccountId::new(),
    };
    // Та же учётная запись в другой организации — другой адресат.
    let c = AccountScope {
        tenant: TenantId::new(),
        account: a.account,
    };
    let mut sub_b = bus.subscribe(b).await.expect("subscribe");
    let mut sub_c = bus.subscribe(c).await.expect("subscribe");
    bus.publish(mailbox_changed(a, 1)).await.expect("publish");
    bus.publish(mailbox_changed(b, 2)).await.expect("publish");
    bus.publish(mailbox_changed(c, 3)).await.expect("publish");
    assert_eq!(next(&mut sub_b).await, Some(mailbox_changed(b, 2)));
    assert_eq!(next(&mut sub_c).await, Some(mailbox_changed(c, 3)));
}

async fn fans_out_to_all_subscribers(bus: &dyn EventBus) {
    let scope = scope();
    let mut one = bus.subscribe(scope).await.expect("subscribe");
    let mut two = bus.subscribe(scope).await.expect("subscribe");
    bus.publish(mailbox_changed(scope, 1)).await.expect("publish");
    assert_eq!(next(&mut one).await, Some(mailbox_changed(scope, 1)));
    assert_eq!(next(&mut two).await, Some(mailbox_changed(scope, 1)));
}

async fn keeps_order(bus: &dyn EventBus) {
    let scope = scope();
    let mut sub = bus.subscribe(scope).await.expect("subscribe");
    for i in 1..=20 {
        bus.publish(mailbox_changed(scope, i)).await.expect("publish");
    }
    for i in 1..=20 {
        assert_eq!(next(&mut sub).await, Some(mailbox_changed(scope, i)));
    }
}

async fn separates_system_events(bus: &dyn EventBus) {
    let scope = scope();
    let mut sys = bus.subscribe_system().await.expect("subscribe");
    let mut acc = bus.subscribe(scope).await.expect("subscribe");
    bus.publish(mailbox_changed(scope, 1)).await.expect("publish");
    for kind in [EventKind::QueueReady, EventKind::ConfigReloaded] {
        bus.publish(system(kind)).await.expect("publish");
    }
    let tenant_event = Event {
        tenant: scope.tenant,
        ..system(EventKind::QueueReady)
    };
    bus.publish(tenant_event.clone()).await.expect("publish");
    bus.publish(mailbox_changed(scope, 2)).await.expect("publish");

    assert_eq!(next(&mut sys).await, Some(system(EventKind::QueueReady)));
    assert_eq!(next(&mut sys).await, Some(system(EventKind::ConfigReloaded)));
    assert_eq!(next(&mut sys).await, Some(tenant_event));
    assert_eq!(next(&mut acc).await, Some(mailbox_changed(scope, 1)));
    assert_eq!(next(&mut acc).await, Some(mailbox_changed(scope, 2)));
}

async fn publish_without_subscribers(bus: &dyn EventBus) {
    bus.publish(mailbox_changed(scope(), 1))
        .await
        .expect("publish");
}

async fn dropped_subscriber_does_not_block(bus: &dyn EventBus) {
    let scope = scope();
    let gone = bus.subscribe(scope).await.expect("subscribe");
    let mut live = bus.subscribe(scope).await.expect("subscribe");
    drop(gone);
    bus.publish(mailbox_changed(scope, 1)).await.expect("publish");
    assert_eq!(next(&mut live).await, Some(mailbox_changed(scope, 1)));
    drop(live);
    // Канал отпущен и создаётся заново следующей подпиской.
    let mut again = bus.subscribe(scope).await.expect("subscribe");
    bus.publish(mailbox_changed(scope, 2)).await.expect("publish");
    assert_eq!(next(&mut again).await, Some(mailbox_changed(scope, 2)));
}
