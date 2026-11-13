//! Наборы проверок жизненного цикла ящика и очереди без опроса (CTR-73, CTR-76…78): ближайший срок и сигнал
//! очереди, архив ящика (выборка «пора в архив», раздельная квота), сигнал и атомарная отметка удаления отключённого
//! ящика. In-memory хранилища `tp-testkit` и `PgMeta` проходят одни и те же проверки.

use std::time::Duration;

use time::OffsetDateTime;
use tp_contracts::archive::ArchiveStore;
use tp_contracts::bus::{Event, EventBus, EventKind, EventStream};
use tp_contracts::meta::{
    AccountKind, AccountScope, AccountState, AccountStore, AccountUpsert, Envelope, MailboxStore,
    MessageMeta, MessageStore, NewMailbox, NewMessage, Policy, PolicyStore, PolicyTarget,
    SpecialUse, disabled_mailbox_changed,
};
use tp_contracts::queue::{DsnParams, OutMessage, OutRcpt, QueueStore, RcptStatus, queue_ready};
use tp_contracts::retention::Quota;
use tp_contracts::{AccountId, BlobHash, BlobKey, Error, Gen, MailboxId, MessageId, TenantId};

use super::{AccountStateFixture, fresh_hash, next_signal, role_folder};

/// Следующий сигнал вида `want` (прочие виды пропускаются: хранилище может публиковать и другие сигналы).
async fn next_kind(stream: &mut EventStream, want: impl Fn(&EventKind) -> bool) -> Event {
    loop {
        let e = next_signal(stream).await.expect("подписка завершена");
        if want(&e.kind) {
            return e;
        }
    }
}

// ---------- QueueStore: ближайший срок и сигнал (CTR-73) ----------

/// Набор проверок ближайшего срока и сигнала пополнения `QueueStore` (CTR-73): `next_due` — наименьший срок попытки,
/// у записи под лизой — не раньше её конца, пустая очередь — `None`; `enqueue`, `reschedule` и `postpone` публикуют
/// в `bus` (шину своей реализации) `QueueReady` организации записи. Очередь перед набором пуста (своя БД).
pub async fn queue_next_due(queue: &dyn QueueStore, bus: &dyn EventBus, tenant: TenantId) {
    let t0 = OffsetDateTime::UNIX_EPOCH + time::Duration::days(200);
    let sec = time::Duration::seconds;
    let lease = Duration::from_secs(30);
    let mut sys = bus.subscribe_system().await.expect("subscribe_system");
    let ready = async |sys: &mut EventStream, what: &str| {
        let e = next_kind(sys, |k| *k == EventKind::QueueReady).await;
        assert_eq!(e, queue_ready(tenant), "{what}");
    };
    let message = |rcpt: &str, at: OffsetDateTime| OutMessage {
        tenant,
        blob: BlobKey {
            tenant,
            hash: fresh_hash(),
            generation: Gen::INITIAL,
        },
        size: 100,
        from: "a@example.org".into(),
        dsn: DsnParams::default(),
        rcpts: vec![OutRcpt::new(rcpt)],
        next_attempt: at,
    };
    let delivered = || RcptStatus::Delivered {
        reply: "250 ok".into(),
    };
    assert_eq!(queue.next_due().await, Ok(None), "пустая очередь");

    let late = queue
        .enqueue(None, message("late@example.net", t0 + sec(60)))
        .await
        .expect("enqueue late");
    ready(&mut sys, "сигнал после постановки").await;
    assert_eq!(queue.next_due().await, Ok(Some(t0 + sec(60))));
    let early = queue
        .enqueue(None, message("early@example.net", t0 + sec(10)))
        .await
        .expect("enqueue early");
    ready(&mut sys, "сигнал после второй постановки").await;
    assert_eq!(
        queue.next_due().await,
        Ok(Some(t0 + sec(10))),
        "ближайший срок"
    );

    let got = queue.claim(t0 + sec(10), 10, lease).await.expect("claim");
    assert_eq!(got.iter().map(|c| c.claim.id).collect::<Vec<_>>(), [early]);
    assert_eq!(
        queue.next_due().await,
        Ok(Some(t0 + sec(40))),
        "запись под лизой — не раньше конца лизы"
    );
    queue
        .reschedule(
            &got[0].claim,
            "early@example.net",
            t0 + sec(100),
            "451 later",
        )
        .await
        .expect("reschedule");
    ready(&mut sys, "сигнал после переноса").await;
    assert_eq!(queue.next_due().await, Ok(Some(t0 + sec(60))));

    let got = queue.claim(t0 + sec(60), 10, lease).await.expect("claim");
    assert_eq!(got.iter().map(|c| c.claim.id).collect::<Vec<_>>(), [late]);
    assert_eq!(
        queue
            .complete(&got[0].claim, "late@example.net", delivered())
            .await,
        Ok(true)
    );
    assert_eq!(queue.next_due().await, Ok(Some(t0 + sec(100))));

    let got = queue.claim(t0 + sec(100), 10, lease).await.expect("claim");
    assert_eq!(got.iter().map(|c| c.claim.id).collect::<Vec<_>>(), [early]);
    queue
        .postpone(&got[0].claim, "early@example.net", t0 + sec(200))
        .await
        .expect("postpone");
    ready(&mut sys, "сигнал после переноса планировщиком").await;
    assert_eq!(queue.next_due().await, Ok(Some(t0 + sec(200))));
    let got = queue.claim(t0 + sec(200), 10, lease).await.expect("claim");
    assert_eq!(
        queue
            .complete(&got[0].claim, "early@example.net", delivered())
            .await,
        Ok(true)
    );
    assert_eq!(queue.next_due().await, Ok(None), "очередь снова пуста");
}

// ---------- Отключённые ящики (CTR-77, CTR-78) ----------

fn upsert(fx: &AccountStateFixture, name: &str) -> AccountUpsert {
    let run = AccountId::new();
    AccountUpsert {
        domain: fx.domain,
        login: format!("{name}-{run}@{}", fx.domain_name),
        display_name: name.to_owned(),
        external_id: Some(format!("{name}-{run}")),
        enabled: true,
        aliases: Vec::new(),
        kind: AccountKind::User,
        hidden_from_gal: false,
    }
}

fn whole_seconds(t: OffsetDateTime) -> OffsetDateTime {
    t.replace_nanosecond(0).expect("time")
}

/// Набор проверок сигнала `DisabledMailboxChanged` `AccountStore` (CTR-77): `accounts` публикует в `bus` (шину своей
/// реализации) сигнал ящика после `disable_account` (отключение и смена срока), `restore_account` отключённого и
/// `attach_account`; восстановление действующего — без сигнала. «Не пришло» проверяется тем, что первым пришёл
/// сигнал, опубликованный позже.
pub async fn disabled_mailbox_signals(
    accounts: &dyn AccountStore,
    bus: &dyn EventBus,
    fx: &AccountStateFixture,
) {
    let t = fx.tenant;
    let scope = |account| AccountScope { tenant: t, account };
    let until = whole_seconds(OffsetDateTime::now_utc()) + time::Duration::days(30);
    let mut sys = bus.subscribe_system().await.expect("subscribe_system");
    let changed = async |sys: &mut EventStream| {
        next_kind(sys, |k| {
            matches!(k, EventKind::DisabledMailboxChanged { .. })
        })
        .await
    };

    let a = accounts.upsert(t, upsert(fx, "leaver")).await.expect("a");
    accounts
        .disable_account(t, a.id, until)
        .await
        .expect("disable");
    assert_eq!(
        changed(&mut sys).await,
        disabled_mailbox_changed(scope(a.id)),
        "отключение"
    );
    accounts
        .disable_account(t, a.id, until + time::Duration::days(1))
        .await
        .expect("new term");
    assert_eq!(
        changed(&mut sys).await,
        disabled_mailbox_changed(scope(a.id)),
        "смена срока"
    );
    accounts.restore_account(t, a.id).await.expect("restore");
    assert_eq!(
        changed(&mut sys).await,
        disabled_mailbox_changed(scope(a.id)),
        "восстановление"
    );
    accounts
        .restore_account(t, a.id)
        .await
        .expect("restore active");

    let b = accounts.upsert(t, upsert(fx, "other")).await.expect("b");
    let to = accounts
        .upsert(t, upsert(fx, "successor"))
        .await
        .expect("to");
    accounts
        .disable_account(t, b.id, until)
        .await
        .expect("disable b");
    assert_eq!(
        changed(&mut sys).await,
        disabled_mailbox_changed(scope(b.id)),
        "восстановление действующего — без сигнала"
    );
    accounts
        .attach_account(t, b.id, to.id, None)
        .await
        .expect("attach");
    assert_eq!(
        changed(&mut sys).await,
        disabled_mailbox_changed(scope(b.id)),
        "подключение"
    );
}

/// Набор проверок атомарной отметки удаления `AccountStore` (CTR-78): `begin_removal` переводит отключённый ящик с
/// истёкшим сроком в `Removing` (повтор — без изменений), действующий и с будущим сроком — `Conflict`; удаляемый не
/// восстанавливается, не подключается, срок не переносится (`Conflict`) и остаётся в `expired_accounts`;
/// `remove_disabled` — только удаляемого; параллельные восстановление и отметка — успешна ровно одна.
pub async fn account_removal(accounts: &dyn AccountStore, fx: &AccountStateFixture) {
    let t = fx.tenant;
    let scope = |account| AccountScope { tenant: t, account };
    let until = whole_seconds(OffsetDateTime::now_utc()) + time::Duration::days(30);
    let sec = time::Duration::seconds;

    let a = accounts.upsert(t, upsert(fx, "leaver")).await.expect("a");
    assert_eq!(
        accounts.begin_removal(t, a.id, until).await,
        Err(Error::Conflict),
        "действующий"
    );
    assert_eq!(
        accounts.begin_removal(t, AccountId::new(), until).await,
        Err(Error::NotFound)
    );
    accounts
        .disable_account(t, a.id, until)
        .await
        .expect("disable");
    assert_eq!(
        accounts.begin_removal(t, a.id, until - sec(1)).await,
        Err(Error::Conflict),
        "срок не истёк"
    );
    assert!(
        matches!(
            accounts.remove_disabled(t, a.id).await,
            Err(Error::Invalid(_))
        ),
        "без отметки удаления"
    );
    accounts
        .begin_removal(t, a.id, until)
        .await
        .expect("begin_removal");
    accounts
        .begin_removal(t, a.id, until)
        .await
        .expect("повтор — продолжение удаления");
    assert_eq!(
        accounts.account_state(t, a.id).await,
        Ok(AccountState::Removing { until })
    );
    assert_eq!(
        accounts.restore_account(t, a.id).await,
        Err(Error::Conflict)
    );
    assert_eq!(
        accounts
            .disable_account(t, a.id, until + time::Duration::days(1))
            .await,
        Err(Error::Conflict)
    );
    let to = accounts
        .upsert(t, upsert(fx, "successor"))
        .await
        .expect("to");
    assert_eq!(
        accounts.attach_account(t, a.id, to.id, None).await,
        Err(Error::Conflict)
    );
    assert!(
        accounts
            .expired_accounts(until, 10_000)
            .await
            .expect("expired")
            .contains(&scope(a.id)),
        "прерванное удаление продолжается"
    );
    assert!(
        accounts
            .disabled_accounts(t)
            .await
            .expect("disabled")
            .iter()
            .any(|d| d.account.id == a.id && d.until == until)
    );
    accounts
        .remove_disabled(t, a.id)
        .await
        .expect("remove_disabled");
    assert_eq!(accounts.get(t, a.id).await, Ok(None));

    // Гонка «восстановление против удаления»: успешна ровно одна операция, состояние — её.
    for n in 0..8 {
        let x = accounts
            .upsert(t, upsert(fx, &format!("race{n}")))
            .await
            .expect("race account");
        accounts
            .disable_account(t, x.id, until)
            .await
            .expect("disable race");
        let (restored, marked) = futures::join!(
            accounts.restore_account(t, x.id),
            accounts.begin_removal(t, x.id, until)
        );
        let state = accounts.account_state(t, x.id).await.expect("state");
        match (restored, marked) {
            (Ok(()), Err(Error::Conflict)) => assert_eq!(state, AccountState::Active),
            (Err(Error::Conflict), Ok(())) => assert_eq!(state, AccountState::Removing { until }),
            other => panic!("гонка {n}: успешна должна быть ровно одна операция, {other:?}"),
        }
    }
}

// ---------- Архив (CTR-76) ----------

/// Момент приёма «старых» писем набора архива (целые секунды).
fn received() -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("ts")
}

async fn put_at(
    messages: &dyn MessageStore,
    scope: AccountScope,
    mailbox: MailboxId,
    size: u64,
    received_at: OffsetDateTime,
) -> MessageId {
    let mut hash = [0u8; 32];
    hash[..16].copy_from_slice(MessageId::new().as_uuid().as_bytes());
    let msg = NewMessage {
        skeleton: BlobKey {
            tenant: scope.tenant,
            hash: BlobHash(hash),
            generation: Gen::INITIAL,
        },
        parts: Vec::new(),
        size,
        received_at,
        flags: Default::default(),
        meta: MessageMeta {
            envelope: Envelope {
                subject: Some(format!("архив {size}")),
                ..Default::default()
            },
            ..Default::default()
        },
    };
    messages
        .insert(scope, mailbox, msg)
        .await
        .expect("insert")
        .id
}

/// Хранилища набора [`archive`].
pub struct ArchiveStores<'a> {
    /// Архив.
    pub archive: &'a dyn ArchiveStore,
    /// Основная квота.
    pub quota: &'a dyn Quota,
    /// Учётные записи.
    pub accounts: &'a dyn AccountStore,
    /// Папки.
    pub mailboxes: &'a dyn MailboxStore,
    /// Письма.
    pub messages: &'a dyn MessageStore,
    /// Уровни политик (запись видна `PolicyResolver` архива и квоты).
    pub policies: &'a dyn PolicyStore,
}

impl std::fmt::Debug for ArchiveStores<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ArchiveStores").finish_non_exhaustive()
    }
}

/// Набор проверок архива ящика (CTR-76): без `archive_after` писем «пора в архив» нет; с ним — письма вне ветки
/// архива и корзины со сроком `received_at + archive_after` не позже момента запроса, по сроку; письма ветки
/// архива (папка `\Archive` и вложенные) — в квоте архива с лимитом `archive_quota_bytes`, остальные — в основной
/// квоте (`Quota::usage`, `AccountStore::quota`); отключённый ящик в выборку не попадает. Выборка общая для
/// организаций — проверяются письма своей учётной записи.
pub async fn archive(s: &ArchiveStores<'_>, fx: &AccountStateFixture) {
    let t = fx.tenant;
    let day = time::Duration::days;
    let a = s.accounts.upsert(t, upsert(fx, "keeper")).await.expect("a");
    let scope = AccountScope {
        tenant: t,
        account: a.id,
    };
    let own = async |now: OffsetDateTime| -> Vec<(MessageId, MailboxId, OffsetDateTime)> {
        s.archive
            .archive_due(now, 10_000)
            .await
            .expect("archive_due")
            .into_iter()
            .filter(|i| i.scope == scope)
            .map(|i| (i.message, i.mailbox, i.due))
            .collect()
    };
    let inbox = role_folder(s.mailboxes, scope, SpecialUse::Inbox).await;
    let archive = role_folder(s.mailboxes, scope, SpecialUse::Archive).await;
    let nested = s
        .mailboxes
        .create(
            scope,
            NewMailbox {
                parent: Some(archive),
                name: "2023".to_owned(),
                role: None,
            },
        )
        .await
        .expect("nested archive folder")
        .id;
    let old = put_at(s.messages, scope, inbox, 100, received()).await;
    let newer = put_at(s.messages, scope, inbox, 200, received() + day(10)).await;
    let trashed = put_at(s.messages, scope, inbox, 400, received()).await;
    s.messages
        .to_trash(scope, &[trashed])
        .await
        .expect("to_trash");
    put_at(s.messages, scope, archive, 1000, received()).await;
    put_at(s.messages, scope, nested, 2000, received()).await;

    let far = received() + day(36_500);
    assert_eq!(
        own(far).await,
        Vec::new(),
        "без политики — автопереноса нет"
    );
    let target = PolicyTarget::Account(a.id);
    s.policies
        .set_policy(
            t,
            target,
            &Policy {
                archive_after: Some(Duration::from_secs(30 * 86_400)),
                archive_quota_bytes: Some(5000),
                ..Policy::default()
            },
        )
        .await
        .expect("set_policy");

    assert_eq!(
        own(received() + day(30) - time::Duration::seconds(1)).await,
        Vec::new()
    );
    assert_eq!(
        own(received() + day(30)).await,
        vec![(old, inbox, received() + day(30))],
        "вне ветки архива и корзины, срок наступил"
    );
    assert_eq!(
        own(far).await,
        vec![
            (old, inbox, received() + day(30)),
            (newer, inbox, received() + day(40))
        ],
        "по сроку"
    );

    let archived = s.archive.archive_usage(scope).await.expect("archive_usage");
    assert_eq!(
        (archived.used_bytes, archived.messages, archived.limit_bytes),
        (3000, 2, Some(5000)),
        "квота архива — ветка архива"
    );
    let main = s.quota.usage(scope).await.expect("usage");
    assert_eq!(
        (main.used_bytes, main.messages),
        (700, 3),
        "основная квота — без ветки архива, с корзиной"
    );
    let catalog = s.accounts.quota(t, a.id).await.expect("quota");
    assert_eq!((catalog.used_bytes, catalog.messages), (700, 3));

    // Перенос в архив освобождает основную квоту.
    s.messages
        .move_to(scope, &[old], archive)
        .await
        .expect("move to archive");
    assert_eq!(own(far).await, vec![(newer, inbox, received() + day(40))]);
    assert_eq!(
        s.archive
            .archive_usage(scope)
            .await
            .expect("archive_usage")
            .used_bytes,
        3100
    );
    assert_eq!(s.quota.usage(scope).await.expect("usage").used_bytes, 600);

    let until = whole_seconds(OffsetDateTime::now_utc()) + day(30);
    s.accounts
        .disable_account(t, a.id, until)
        .await
        .expect("disable");
    assert_eq!(own(far).await, Vec::new(), "отключённый ящик");
    s.accounts.restore_account(t, a.id).await.expect("restore");
    assert_eq!(
        s.archive
            .archive_usage(AccountScope {
                tenant: t,
                account: AccountId::new()
            })
            .await,
        Err(Error::NotFound)
    );
    s.policies
        .set_policy(t, target, &Policy::default())
        .await
        .expect("reset policy");
}
