//! Волна smtp-06: доверенные источники-приложения (SMTP-20), принятые домены кластера (SMTP-21), точки вызова
//! правил потока почты (SMTP-22), SRS и пересылка (SMTP-23).

use tp_contracts::membership::NodeId;
use tp_contracts::secrets::SecretRef;
use tp_contracts::transport::{AddressPattern, SourceMatch, TransportStore, TrustedSource};
use tp_testkit::{MemDeliveryLog, MemSecretStore};

use super::*;
use crate::config::{ForwardConfig, SrsConfig};
use crate::forward::Forwarding;

// ---------- SMTP-21: принятые домены ----------

/// Принятые домены `domains` сверх `example.org` окружения.
async fn with_domains(env: &Env, domains: &[(&str, TenantId, DomainKind)]) -> Delivery {
    let store = env.transport.clone();
    let mut v = store.cluster().await.expect("cluster").version;
    for (name, tenant, kind) in domains {
        v = store
            .put_domain(
                v,
                &AcceptedDomain {
                    id: tp_contracts::DomainId::new(),
                    tenant: *tenant,
                    name: (*name).into(),
                    kind: kind.clone(),
                },
            )
            .await
            .expect("domain");
    }
    env.delivery(true)
}

#[tokio::test]
async fn accepted_domains_authoritative_relay_and_foreign() {
    let env = Env::new(DecisionHooks::default()).await;
    let relay = DomainKind::InternalRelay {
        next_hop: "old-mail.corp.example:25".into(),
    };
    let d = with_domains(
        &env,
        &[
            ("corp.example", TenantId::DEFAULT, DomainKind::Authoritative),
            ("legacy.example", TenantId::DEFAULT, relay),
            ("other.example", TenantId::new(), DomainKind::Authoritative),
        ],
    )
    .await;
    let e = envelope(Mode::Inbound, &[]);
    // Полномочный домен (только в настройках транспорта): неизвестный адрес — 550 5.1.1.
    let r = d.rcpt(&e, "nobody@corp.example").await;
    assert_eq!((r.code, r.esc), (550, [5, 1, 1]));
    // Принятый домен окружения — тоже полномочный.
    assert_eq!(d.rcpt(&e, "nobody@example.org").await.code, 550);
    // Внутренняя ретрансляция: адрес вне каталога принимается и ставится в очередь (smtp-out — на next_hop).
    assert_eq!(d.rcpt(&e, "old.user@legacy.example").await.code, 250);
    let e = envelope(
        Mode::Inbound,
        &["old.user@legacy.example", "bob@example.org"],
    );
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    let queued = env.queue.0.lock().expect("lock").clone();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].0, "sender@example.net");
    assert_eq!(queued[0].1, ["old.user@legacy.example"]);
    // Домен другой организации — временный отказ (узел обслуживает одну организацию).
    let r = d
        .rcpt(&envelope(Mode::Inbound, &[]), "x@other.example")
        .await;
    assert_eq!(r.code, 451);
    // Узел без очереди ретранслируемых адресов не принимает.
    let no_queue = Delivery::new(
        Deps {
            outbound: None,
            ..d.deps.clone()
        },
        &SmtpConfig::default(),
    )
    .with_transport(d.transport.clone().expect("routes"));
    assert_eq!(
        no_queue
            .rcpt(&envelope(Mode::Inbound, &[]), "old.user@legacy.example")
            .await
            .code,
        451
    );
}

// ---------- SMTP-20: приложение ----------

fn app(external: bool) -> Arc<TrustedSource> {
    Arc::new(TrustedSource {
        name: "mfp".into(),
        tenant: Some(TenantId::DEFAULT),
        matcher: SourceMatch::Networks(vec!["192.0.2.0/24".parse().expect("net")]),
        kind: tp_contracts::transport::SourceKind::Application {
            senders: vec![AddressPattern::new("scanner@example.org").expect("pattern")],
            external,
        },
    })
}

#[tokio::test]
async fn application_external_recipients_only_when_allowed() {
    let env = Env::new(DecisionHooks::default()).await;
    let d = env.delivery(true);
    let mut e = envelope(Mode::Inbound, &[]);
    e.mail_from = "scanner@example.org".into();
    e.source = Some(app(false));
    let r = d.rcpt(&e, "partner@example.net").await;
    assert_eq!((r.code, r.esc), (550, [5, 7, 1]));
    assert_eq!(d.rcpt(&e, "bob@example.org").await.code, 250);
    e.source = Some(app(true));
    assert_eq!(d.rcpt(&e, "partner@example.net").await.code, 250);
    e.rcpts = vec!["partner@example.net".into(), "bob@example.org".into()];
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    let queued = env.queue.0.lock().expect("lock").clone();
    // Письмо приложения — как есть, без SRS: отправитель — разрешённый адрес организации.
    assert_eq!(queued[0].0, "scanner@example.org");
    assert_eq!(queued[0].1, ["partner@example.net"]);
    // Источник-приложение другой организации — временный отказ.
    let mut foreign = (*app(true)).clone();
    foreign.tenant = Some(TenantId::new());
    e.source = Some(Arc::new(foreign));
    assert_eq!(d.rcpt(&e, "bob@example.org").await.code, 451);
}

// ---------- SMTP-22: правила потока почты ----------

/// Заглушка `MailFlow`: запоминает вызовы и тело, отвечает заданным итогом.
struct StubFlow {
    blobs: MemBlobStore,
    index: MemBlobIndex,
    calls: Mutex<Vec<(FlowInput, Bytes)>>,
    verdict: Mutex<FlowVerdict>,
    rcpts: Mutex<Option<Vec<String>>>,
    body: Mutex<Option<&'static [u8]>>,
}

impl StubFlow {
    fn new(env: &Env) -> Arc<Self> {
        Arc::new(Self {
            blobs: env.blobs.clone(),
            index: env.index.clone(),
            calls: Mutex::default(),
            verdict: Mutex::default(),
            rcpts: Mutex::default(),
            body: Mutex::default(),
        })
    }

    fn calls(&self) -> Vec<(FlowInput, Bytes)> {
        self.calls.lock().expect("lock").clone()
    }
}

#[async_trait]
impl MailFlow for StubFlow {
    async fn apply(&self, input: &FlowInput) -> Result<FlowOutcome> {
        let body = read_blob(&self.blobs, &input.message.key).await?;
        assert_eq!(body.len() as u64, input.message.size);
        self.calls.lock().expect("lock").push((input.clone(), body));
        let mut out = FlowOutcome::unchanged(input);
        out.verdict = self.verdict.lock().expect("lock").clone();
        if let Some(r) = self.rcpts.lock().expect("lock").clone() {
            out.envelope.rcpt_to = r;
        }
        let new_body = *self.body.lock().expect("lock");
        if let Some(b) = new_body {
            let key = crate::queue::store_blob(
                &self.blobs,
                &self.index,
                input.tenant,
                None,
                Bytes::from_static(b),
            )
            .await?;
            out.message = Some(BlobRef {
                key,
                size: b.len() as u64,
            });
        }
        out.matched = vec!["rule-1".into()];
        out.route = Some("*".into());
        Ok(out)
    }
}

fn flow_delivery(env: &Env, flow: &Arc<StubFlow>) -> Delivery {
    env.delivery(true).with_mail_flow(
        flow.clone(),
        Arc::new(env.blobs.clone()),
        Arc::new(env.index.clone()),
    )
}

#[tokio::test]
async fn mail_flow_reject_drop_moderate() {
    let env = Env::new(DecisionHooks::default()).await;
    let flow = StubFlow::new(&env);
    let d = flow_delivery(&env, &flow);
    let e = envelope(Mode::Inbound, &["bob@example.org"]);
    *flow.verdict.lock().expect("lock") = FlowVerdict::Reject("Blocked by rule\r\nX: y".into());
    let r = d.data(&e, Bytes::from_static(MAIL)).await;
    assert_eq!((r.code, r.esc), (550, [5, 7, 1]));
    assert_eq!(r.message, "Blocked by ruleX: y");
    *flow.verdict.lock().expect("lock") = FlowVerdict::Drop;
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    *flow.verdict.lock().expect("lock") = FlowVerdict::Moderate {
        moderators: vec![AccountId::new()],
    };
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 451);
    // Accept с пустым конвертом — как Drop.
    *flow.verdict.lock().expect("lock") = FlowVerdict::Accept;
    *flow.rcpts.lock().expect("lock") = Some(Vec::new());
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert!(env.inbox("bob@example.org").await.is_empty());
    let calls = flow.calls();
    assert_eq!(calls.len(), 4, "один вызов на письмо");
    let (input, body) = &calls[0];
    assert_eq!(input.direction, FlowDirection::Inbound);
    assert_eq!(input.envelope.rcpt_to, ["bob@example.org"]);
    // Тело — после заголовков трассировки (smtp-in после Milter).
    assert!(body.starts_with(b"Return-Path: <sender@example.net>\r\n"));
    // Ссылки на объекты тела вызова сняты.
    assert_released(&env, &calls);
}

#[tokio::test]
async fn mail_flow_changes_envelope_and_body_and_logs_matched() {
    let env = Env::new(DecisionHooks::default()).await;
    let flow = StubFlow::new(&env);
    let log = MemDeliveryLog::new();
    let d = flow_delivery(&env, &flow).with_tracking(Tracking::new(
        Arc::new(log.clone()),
        NodeId::new("node-1").expect("node"),
    ));
    // Перенаправление: вместо Боба — Кэрол и внешний адрес; тело — с дисклеймером.
    *flow.rcpts.lock().expect("lock") =
        Some(vec!["carol@example.org".into(), "audit@example.com".into()]);
    *flow.body.lock().expect("lock") = Some(b"Subject: hello\r\n\r\nbody\r\n-- disclaimer\r\n");
    let e = envelope(Mode::Inbound, &["bob@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert!(env.inbox("bob@example.org").await.is_empty());
    let carol = env.inbox("carol@example.org").await;
    assert_eq!(carol.len(), 1);
    assert!(carol[0].ends_with("-- disclaimer\r\n"), "{}", carol[0]);
    let queued = env.queue.0.lock().expect("lock").clone();
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].1, ["audit@example.com"]);
    let matched = log
        .events()
        .into_iter()
        .any(|ev| ev.reason.as_deref() == Some("mail flow rules: rule-1"));
    assert!(matched, "{:?}", log.events());
    assert_released(&env, &flow.calls());
}

/// Ссылки на объекты тел, переданных модулю, сняты.
fn assert_released(env: &Env, calls: &[(FlowInput, Bytes)]) {
    for (input, _) in calls {
        let key = input.message.key;
        let row = env.index.row(key.tenant, &key.hash).expect("row");
        assert_eq!(row.refcnt, 0, "{row:?}");
    }
}

#[tokio::test]
async fn mail_flow_once_for_submission_to_internal_recipient() {
    let env = Env::new(DecisionHooks::default()).await;
    let flow = StubFlow::new(&env);
    let d = flow_delivery(&env, &flow);
    let mut e = envelope(Mode::Submission, &["bob@example.org", "x@example.net"]);
    e.mail_from = "alice@example.org".into();
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    assert_eq!(env.queue.0.lock().expect("lock").len(), 1);
    // Внутреннему адресату письмо доставлено сразу — повторного приёма (и вызова правил) нет.
    let calls = flow.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0.direction, FlowDirection::Submission);
    // Отправка вне сессии (JMAP) — тоже один вызов; отказ правила — статус всем получателям.
    *flow.verdict.lock().expect("lock") = FlowVerdict::Reject("no".into());
    let out = d
        .submit(
            TenantId::DEFAULT,
            None,
            "alice@example.org",
            &["bob@example.org".to_owned()],
            Bytes::from_static(MAIL),
        )
        .await
        .expect("submit");
    assert!(
        matches!(&out.rcpts[0].1, SubmitStatus::Failed(r) if r == "550 5.7.1 no"),
        "{out:?}"
    );
    assert_eq!(flow.calls().len(), 2);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
}

#[tokio::test]
async fn without_mail_flow_module_nothing_is_called() {
    let env = Env::new(DecisionHooks::default()).await;
    let flow = StubFlow::new(&env);
    // Модуль не зарегистрирован: доставка без вызова.
    let d = env.delivery(true);
    let e = envelope(Mode::Inbound, &["bob@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    assert_eq!(env.inbox("bob@example.org").await.len(), 1);
    assert!(flow.calls().is_empty());
}

// ---------- SMTP-23: SRS при пересылке ----------

fn forwarding() -> Arc<Forwarding> {
    let secrets = MemSecretStore::new(Arc::new(SystemClock));
    let key: SecretRef = "thepost/srs#key".parse().expect("ref");
    secrets.set(&key, "srs-secret");
    Arc::new(Forwarding::new(
        Arc::new(secrets),
        ForwardConfig {
            srs: Some(SrsConfig {
                domain: "example.org".into(),
                key,
                previous_keys: Vec::new(),
                max_age_days: 21,
            }),
            ..ForwardConfig::default()
        },
    ))
}

#[tokio::test]
async fn forward_uses_srs_and_bounce_returns_to_sender() {
    let hook = Arc::new(RecordingDecisionHook::new("redirect"));
    hook.set_delivery(Decision::Augment(Augment {
        headers: vec![],
        copies: vec![CopyTo::Address("carol@partner.example".into())],
    }));
    let env = Env::new(DecisionHooks::new(vec![hook])).await;
    let d = env.delivery(true).with_forwarding(forwarding());
    let e = envelope(Mode::Inbound, &["bob@example.org"]);
    assert_eq!(d.data(&e, Bytes::from_static(MAIL)).await.code, 250);
    let queued = env.queue.0.lock().expect("lock").clone();
    assert_eq!(queued.len(), 1);
    let srs = queued[0].0.clone();
    assert!(srs.starts_with("SRS0="), "{srs}");
    assert!(srs.ends_with("@example.org"), "{srs}");
    assert_eq!(queued[0].1, ["carol@partner.example"]);

    // Уведомление о недоставке на адрес SRS — исходному отправителю.
    let mut dsn = envelope(Mode::Inbound, &[]);
    dsn.mail_from = String::new();
    assert_eq!(d.rcpt(&dsn, &srs).await.code, 250);
    dsn.rcpts = vec![srs.clone()];
    assert_eq!(d.data(&dsn, Bytes::from_static(MAIL)).await.code, 250);
    let queued = env.queue.0.lock().expect("lock").clone();
    assert_eq!(queued.len(), 2);
    assert_eq!(queued[1].0, "");
    assert_eq!(queued[1].1, ["sender@example.net"]);

    // Подделанный адрес — 550.
    let forged = srs.replace("=sender@", "=mallory@");
    let r = d.rcpt(&dsn, &forged).await;
    assert_eq!((r.code, r.esc), (550, [5, 1, 1]));
    // Отправитель организации при пересылке не переписывается (SPF домена — наш).
    let mut own = envelope(Mode::Inbound, &["bob@example.org"]);
    own.mail_from = "alice@example.org".into();
    assert_eq!(d.data(&own, Bytes::from_static(MAIL)).await.code, 250);
    let queued = env.queue.0.lock().expect("lock").clone();
    assert_eq!(queued[2].0, "alice@example.org");
}
