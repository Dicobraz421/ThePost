//! Волна smtp-16: приглашения из почтового клиента (решение 78, SMTP-36) на заглушке `FakeItip` — submission и
//! отправка вне сессии SMTP (JMAP).

use tp_contracts::DomainId;
use tp_contracts::auth::{Resource, Rights, SendRight};
use tp_contracts::meta::{AccountKind, AccountUpsert};
use tp_contracts::schedule::{ItipMethod, ItipSource};
use tp_testkit::FakeItip;

use super::*;

const ALICE: &str = "alice@example.org";
const BOB: &str = "bob@example.org";

/// Приглашение bob от `from`, организатор — `organizer`.
fn invite(from: &str, organizer: &str) -> String {
    format!(
        "From: <{from}>\r\nTo: {BOB}\r\nSubject: meeting\r\nMIME-Version: 1.0\r\n\
         Content-Type: multipart/alternative; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nmeeting\r\n\
         --b\r\nContent-Type: text/calendar; method=REQUEST; charset=utf-8\r\n\r\nBEGIN:VCALENDAR\r\n\
         VERSION:2.0\r\nMETHOD:REQUEST\r\nBEGIN:VEVENT\r\nUID:u1\r\nDTSTAMP:20261112T100000Z\r\n\
         DTSTART:20261113T100000Z\r\nORGANIZER:mailto:{organizer}\r\nATTENDEE:mailto:{BOB}\r\nEND:VEVENT\r\n\
         END:VCALENDAR\r\n--b--\r\n"
    )
}

/// Права отправки от чужого адреса: (имя входа, адрес `From`) → право.
#[derive(Default)]
pub(super) struct SendRights(Mutex<HashMap<(String, String), SendRight>>);

impl SendRights {
    pub(super) fn grant(&self, login: &str, from: &str, right: SendRight) {
        self.0
            .lock()
            .expect("lock")
            .insert((login.to_owned(), from.to_owned()), right);
    }
}

#[async_trait]
impl Authorizer for SendRights {
    async fn rights(&self, _: &Principal, _: &Resource) -> Result<Rights> {
        Ok(Rights::ALL)
    }

    async fn send_right(&self, p: &Principal, from: &str) -> Result<Option<SendRight>> {
        Ok(self
            .0
            .lock()
            .expect("lock")
            .get(&(p.login.to_lowercase(), from.to_lowercase()))
            .copied())
    }
}

/// Доставка с приёмом iTIP; `rights` — права отправки (`None` — без них).
fn delivery(env: &Env, rights: Option<Arc<SendRights>>) -> (Delivery, FakeItip) {
    let itip = FakeItip::new();
    let mut d = env.delivery(false).with_itip(Arc::new(itip.clone()));
    if let Some(r) = rights {
        d = d.with_authz(r);
    }
    (d, itip)
}

/// Письмо `raw` bob через submission: вошла alice, отправитель конверта — она же.
async fn submission(env: &Env, d: &Delivery, raw: &str) {
    let r = submission_reply(env, d, raw).await;
    assert_eq!(r.code, 250, "{r:?}");
}

/// Ответ на письмо `raw` bob через submission от alice.
async fn submission_reply(env: &Env, d: &Delivery, raw: &str) -> Reply {
    let mut e = envelope(Mode::Submission, &[BOB]);
    e.mail_from = ALICE.into();
    e.principal = Some(super::org_policy::principal(env).await);
    d.data(&e, Bytes::from(raw.to_owned())).await
}

/// Отправка вне сессии SMTP (JMAP `EmailSubmission`) от alice.
async fn submit(d: &Delivery, raw: &str) {
    let sent = d
        .submit(
            TenantId::DEFAULT,
            None,
            ALICE,
            &[BOB.to_owned()],
            Bytes::from(raw.to_owned()),
        )
        .await
        .expect("submit");
    assert_eq!(sent.rcpts, [(BOB.to_owned(), SubmitStatus::Delivered)]);
}

/// Адреса `From` дошедших сообщений iTIP; все — внутренние и bob.
async fn reached(env: &Env, itip: &FakeItip) -> Vec<String> {
    let bob = env.scope(BOB).await;
    itip.calls()
        .into_iter()
        .map(|c| {
            assert_eq!(c.recipient, bob);
            assert_eq!(c.to, BOB);
            assert_eq!(c.method, ItipMethod::Request);
            assert_eq!(c.source, ItipSource::Internal);
            assert!(c.ics.contains("UID:u1"));
            c.from
        })
        .collect()
}

#[tokio::test]
async fn own_address_and_alias_reach_calendar() {
    let env = Env::new(DecisionHooks::default()).await;
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
    let (d, itip) = delivery(&env, None);
    submission(&env, &d, &invite(ALICE, ALICE)).await;
    submission(
        &env,
        &d,
        &invite("a.smith@example.org", "a.smith@example.org"),
    )
    .await;
    // `From` свой, но организатор — другой: как у iMIP, не применяется.
    submission(&env, &d, &invite(ALICE, "carol@example.org")).await;
    assert_eq!(reached(&env, &itip).await, [ALICE, "a.smith@example.org"]);
    assert_eq!(env.inbox(BOB).await.len(), 3);
}

#[tokio::test]
async fn foreign_from_without_right_stays_mail() {
    let env = Env::new(DecisionHooks::default()).await;
    let raw = invite("carol@example.org", "carol@example.org");
    // Без прав отправки и с правами, где у alice нет права на адрес carol.
    let rights = Arc::new(SendRights::default());
    rights.grant("bob@example.org", "carol@example.org", SendRight::SendAs);
    for authz in [None, Some(rights)] {
        let (d, itip) = delivery(&env, authz);
        // Submission письмо от чужого имени не принимает (SMTP-45); JMAP проверяет `From` сам.
        let r = submission_reply(&env, &d, &raw).await;
        assert_eq!((r.code, r.esc), (550, [5, 7, 1]), "{r:?}");
        submit(&d, &raw).await;
        assert!(itip.calls().is_empty());
    }
    assert_eq!(env.inbox(BOB).await.len(), 2);
}

#[tokio::test]
async fn send_as_and_on_behalf_reach_calendar() {
    let env = Env::new(DecisionHooks::default()).await;
    let rights = Arc::new(SendRights::default());
    rights.grant(ALICE, "carol@example.org", SendRight::SendAs);
    rights.grant(ALICE, "boss@example.org", SendRight::SendOnBehalf);
    let (d, itip) = delivery(&env, Some(rights));
    submission(&env, &d, &invite("carol@example.org", "carol@example.org")).await;
    submission(&env, &d, &invite("boss@example.org", "boss@example.org")).await;
    assert_eq!(
        reached(&env, &itip).await,
        ["carol@example.org", "boss@example.org"]
    );
    assert_eq!(env.inbox(BOB).await.len(), 2);
}

#[tokio::test]
async fn submit_checks_envelope_sender_account() {
    let env = Env::new(DecisionHooks::default()).await;
    let rights = Arc::new(SendRights::default());
    rights.grant(ALICE, "carol@example.org", SendRight::SendOnBehalf);
    let (d, itip) = delivery(&env, Some(rights));
    submit(&d, &invite(ALICE, ALICE)).await;
    submit(&d, &invite("carol@example.org", "carol@example.org")).await;
    submit(&d, &invite("dave@example.org", "dave@example.org")).await;
    assert_eq!(reached(&env, &itip).await, [ALICE, "carol@example.org"]);
    assert_eq!(env.inbox(BOB).await.len(), 3);
}

#[tokio::test]
async fn external_sender_of_submit_is_not_internal() {
    let env = Env::new(DecisionHooks::default()).await;
    let (d, itip) = delivery(&env, None);
    // Отправитель конверта — не учётная запись организации: приглашение остаётся письмом.
    let raw = invite("sender@example.net", "sender@example.net");
    let sent = d
        .submit(
            TenantId::DEFAULT,
            None,
            "sender@example.net",
            &[BOB.to_owned()],
            Bytes::from(raw),
        )
        .await
        .expect("submit");
    assert_eq!(sent.rcpts, [(BOB.to_owned(), SubmitStatus::Delivered)]);
    assert!(itip.calls().is_empty());
    assert_eq!(env.inbox(BOB).await.len(), 1);
}

/// SMTP-38: приглашение представителя — `From` alice совпал с `SENT-BY` организатора; сообщение — от имени
/// организатора при праве alice на его адрес, иначе только письмо.
#[tokio::test]
async fn sent_by_delegate_needs_right_on_owner() {
    let env = Env::new(DecisionHooks::default()).await;
    let on_behalf = |organizer: &str| {
        invite(ALICE, organizer).replace(
            "ORGANIZER:mailto:",
            &format!("ORGANIZER;CN=Boss;SENT-BY=\"mailto:{ALICE}\":mailto:"),
        )
    };
    let rights = Arc::new(SendRights::default());
    rights.grant(ALICE, "boss@example.org", SendRight::SendOnBehalf);
    let (d, itip) = delivery(&env, Some(rights));
    submission(&env, &d, &on_behalf("boss@example.org")).await;
    submit(&d, &on_behalf("boss@example.org")).await;
    // Тот же `SENT-BY`, но у alice нет права на адрес организатора.
    submission(&env, &d, &on_behalf("carol@example.org")).await;
    submit(&d, &on_behalf("carol@example.org")).await;
    assert_eq!(
        reached(&env, &itip).await,
        ["boss@example.org", "boss@example.org"]
    );
    // Без прав отправки представителю не верят.
    let (d, itip) = delivery(&env, None);
    submission(&env, &d, &on_behalf("boss@example.org")).await;
    assert!(itip.calls().is_empty());
    assert_eq!(env.inbox(BOB).await.len(), 5);
}
