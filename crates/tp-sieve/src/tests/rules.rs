//! Правила доставки (SIEVE-07) на in-memory хранилище: порядок скриптов, `stop` организации, автоответ из
//! настройки, кэш программ, защита `redirect` от петли, ошибки скриптов, цели fuzz на примерах.

use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use tp_contracts::meta::{AccountScope, Flag};
use tp_contracts::sieve::{
    DeliveryRules, MailboxRef, ReplyBody, RulesInput, RulesOutcome, SieveStore, Vacation,
};
use tp_contracts::{AccountId, TenantId};
use tp_testkit::{Clock, ManualClock, MemSieve, MemVacationLog};

use super::mailboxes;
use crate::{CacheConfig, Rules, SieveConfig, fuzz};

const MAIL: &str = "Received: from mx.example.com ([192.0.2.1])\r\n\tby mail.example.org (The Post) with ESMTPS id 1\r\n\
                    From: Alice <alice@example.com>\r\nTo: Bob <bob@example.org>\r\nSubject: Report\r\n\
                    Message-ID: <m1@example.com>\r\n\r\nThe quick brown fox";

struct Fixture {
    store: MemSieve,
    clock: ManualClock,
    rules: Rules,
    scope: AccountScope,
    input: RulesInput,
}

impl Fixture {
    fn new() -> Self {
        Self::with(SieveConfig::default())
    }

    fn with(config: SieveConfig) -> Self {
        let store = MemSieve::new();
        let clock = ManualClock::new(OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000));
        let log = MemVacationLog::new(clock.shared());
        let shared = clock.shared();
        let rules = Rules::new(Arc::new(store.clone()), Arc::new(log), config)
            .with_node("mail.example.org")
            .with_clock(move || shared.now());
        let scope = AccountScope {
            tenant: TenantId::new(),
            account: AccountId::new(),
        };
        let input = RulesInput {
            scope,
            recipient: "bob@example.org".into(),
            mail_from: "alice@example.com".into(),
            message: MAIL.as_bytes().to_vec().into(),
            mailboxes: mailboxes(),
        };
        Self {
            store,
            clock,
            rules,
            scope,
            input,
        }
    }

    async fn user(&self, text: &str) {
        self.store.put(self.scope, "main", text.to_owned()).await.expect("put");
        self.store.activate(self.scope, Some("main")).await.expect("activate");
    }

    async fn org(&self, text: &str) {
        self.store
            .set_org_script(self.scope.tenant, Some(text.to_owned()))
            .await
            .expect("org");
    }

    async fn eval(&self) -> RulesOutcome {
        self.rules.evaluate(&self.input).await.expect("rules")
    }

    /// Папки доставки по именам (папки набора `mailboxes`).
    async fn folders(&self) -> Vec<String> {
        self.eval()
            .await
            .keep
            .iter()
            .map(|k| match &k.mailbox {
                MailboxRef::Inbox => "INBOX".to_owned(),
                MailboxRef::Id(id) => self
                    .input
                    .mailboxes
                    .iter()
                    .find(|m| m.id == *id)
                    .map(|m| m.name.clone())
                    .expect("папка"),
                MailboxRef::Name { name, .. } => format!("new:{name}"),
            })
            .collect()
    }
}

#[tokio::test]
async fn no_rules_is_implicit_keep() {
    let f = Fixture::new();
    assert_eq!(f.eval().await, RulesOutcome::implicit_keep());
    assert_eq!(f.rules.compilations(), 0);
    // Неактивный скрипт не выполняется.
    f.store
        .put(f.scope, "main", "discard;".into())
        .await
        .expect("put");
    assert_eq!(f.eval().await, RulesOutcome::implicit_keep());
}

/// Порядок: скрипт организации, затем скрипт пользователя; действия складываются.
#[tokio::test]
async fn script_order() {
    let f = Fixture::new();
    f.user("require \"fileinto\"; fileinto \"Work\";").await;
    assert_eq!(f.folders().await, ["Work"]);

    // Скрипт организации добавляет копию и флаг; implicit keep отменён скриптом пользователя.
    f.org("require [\"fileinto\", \"copy\", \"imap4flags\"]; fileinto :copy :flags \"$org\" \"Архив\";")
        .await;
    let o = f.eval().await;
    assert_eq!(f.folders().await, ["Архив", "Work"]);
    assert!(o.keep[0].flags.contains(&Flag::Keyword("$org".into())));
    assert!(o.errors.is_empty() && !o.discard);

    // Скрипт организации отменил implicit keep, скрипт пользователя ничего не сделал — доставка только по нему.
    f.org("require \"fileinto\"; fileinto \"Архив\";").await;
    f.user("if false { discard; }").await;
    assert_eq!(f.folders().await, ["Архив"]);

    // discard организации без stop: скрипт пользователя выполняется и сохраняет письмо.
    f.org("discard;").await;
    f.user("keep;").await;
    let o = f.eval().await;
    assert_eq!(f.folders().await, ["INBOX"]);
    assert!(!o.discard);
    f.user("if false { keep; }").await;
    let o = f.eval().await;
    assert!(o.keep.is_empty() && o.discard);
}

/// `stop` в скрипте организации останавливает всё: скрипт пользователя и автоответ не выполняются.
#[tokio::test]
async fn org_stop() {
    let f = Fixture::new();
    f.user("require [\"fileinto\", \"vacation\"]; vacation \"x\"; fileinto \"Work\";").await;
    f.store
        .set_vacation(
            f.scope,
            Vacation {
                enabled: true,
                text_body: Some("нет на месте".into()),
                ..Vacation::default()
            },
        )
        .await
        .expect("vacation");
    f.org("if header :contains \"subject\" \"report\" { stop; }").await;
    assert_eq!(f.eval().await, RulesOutcome::implicit_keep());

    f.org("if header :contains \"subject\" \"report\" { discard; stop; }").await;
    let o = f.eval().await;
    assert!(o.keep.is_empty() && o.discard && o.vacation.is_none());

    // Условие не сработало — stop не выполнен: дальше автоответ и скрипт пользователя.
    f.org("if header :contains \"subject\" \"invoice\" { discard; stop; }").await;
    let o = f.eval().await;
    assert_eq!(f.folders().await, ["Work"]);
    assert!(o.vacation.is_some());
}

/// Автоответ из настройки `Vacation`: вне дат не срабатывает; раньше `vacation` скрипта.
#[tokio::test]
async fn vacation_setting() {
    let f = Fixture::new();
    let now = f.clock.shared().now();
    let setting = Vacation {
        enabled: true,
        from: Some(now + time::Duration::days(1)),
        to: Some(now + time::Duration::days(2)),
        subject: None,
        text_body: Some("из настройки".into()),
        html_body: None,
    };
    f.store.set_vacation(f.scope, setting).await.expect("vacation");
    // До начала: правил нет вовсе.
    assert_eq!(f.eval().await, RulesOutcome::implicit_keep());
    f.clock.advance(Duration::from_secs(24 * 3600));
    let o = f.eval().await;
    let reply = o.vacation.expect("reply");
    assert_eq!(reply.body, ReplyBody::Text("из настройки".into()));
    assert_eq!(reply.subject, "Автоответ: Report");
    assert_eq!(o.keep, RulesOutcome::implicit_keep().keep);
    // Повторно тому же отправителю — нет.
    assert_eq!(f.eval().await.vacation, None);
    // После конца — нет и новому отправителю.
    f.clock.advance(Duration::from_secs(24 * 3600));
    let mut other = f.input.clone();
    other.mail_from = "carol@example.com".into();
    assert_eq!(f.rules.evaluate(&other).await.expect("rules").vacation, None);

    // Настройка и vacation скрипта вместе: ответ один, из настройки; без настройки — из скрипта.
    f.user("require \"vacation\"; vacation \"из скрипта\";").await;
    let on = Vacation {
        enabled: true,
        text_body: Some("из настройки".into()),
        ..Vacation::default()
    };
    f.store.set_vacation(f.scope, on).await.expect("vacation");
    let o = f.rules.evaluate(&other).await.expect("rules");
    assert_eq!(o.vacation.expect("reply").body, ReplyBody::Text("из настройки".into()));
    f.store.set_vacation(f.scope, Vacation::default()).await.expect("vacation");
    other.mail_from = "dave@example.com".into();
    let o = f.rules.evaluate(&other).await.expect("rules");
    assert_eq!(o.vacation.expect("reply").body, ReplyBody::Text("из скрипта".into()));
}

/// Кэш: программа компилируется один раз на версию; `put`, смена активного и скрипта организации сбрасывают её.
#[tokio::test]
async fn cache() {
    let f = Fixture::new();
    f.user("require \"fileinto\"; fileinto \"Work\";").await;
    for _ in 0..3 {
        assert_eq!(f.folders().await, ["Work"]);
    }
    assert_eq!(f.rules.compilations(), 1);

    f.user("require \"fileinto\"; fileinto \"Архив\";").await;
    assert_eq!(f.folders().await, ["Архив"]);
    assert_eq!(f.folders().await, ["Архив"]);
    assert_eq!(f.rules.compilations(), 2);

    f.store.activate(f.scope, None).await.expect("activate");
    assert_eq!(f.folders().await, ["INBOX"]);
    assert_eq!(f.rules.compilations(), 2);

    // Скрипт организации сверяется по тексту (сигнала об изменении у него нет).
    f.org("discard;").await;
    assert!(f.eval().await.discard);
    assert!(f.eval().await.discard);
    assert_eq!(f.rules.compilations(), 3);
    f.org("keep;").await;
    assert!(!f.eval().await.discard);
    assert_eq!(f.rules.compilations(), 4);
    f.store.set_org_script(f.scope.tenant, None).await.expect("org");
    assert_eq!(f.eval().await, RulesOutcome::implicit_keep());

    // Изменение настройки автоответа тоже меняет состояние: кэш не отдаёт старую настройку.
    let on = Vacation {
        enabled: true,
        text_body: Some("x".into()),
        ..Vacation::default()
    };
    f.store.set_vacation(f.scope, on).await.expect("vacation");
    assert!(f.eval().await.vacation.is_some());
}

/// Объём кэша: при одной записи вторая учётная запись вытесняет первую; 0 — без кэша.
#[tokio::test]
async fn cache_capacity() {
    let config = |programs| SieveConfig {
        cache: CacheConfig { programs },
        ..SieveConfig::default()
    };
    let f = Fixture::with(config(1));
    f.user("keep;").await;
    let mut peer = f.input.clone();
    peer.scope.account = AccountId::new();
    f.store.put(peer.scope, "main", "keep;".into()).await.expect("put");
    f.store.activate(peer.scope, Some("main")).await.expect("activate");
    f.eval().await;
    f.eval().await;
    assert_eq!(f.rules.compilations(), 1);
    f.rules.evaluate(&peer).await.expect("rules");
    f.eval().await;
    assert_eq!(f.rules.compilations(), 3);

    let f = Fixture::with(config(0));
    f.user("keep;").await;
    f.eval().await;
    f.eval().await;
    assert_eq!(f.rules.compilations(), 2);
}

/// Ошибка выполнения и скрипт, не прошедший проверку, — implicit keep и запись без содержимого письма.
#[tokio::test]
async fn script_errors() {
    let f = Fixture::with(SieveConfig {
        max_redirects: 1,
        ..SieveConfig::default()
    });
    f.user("redirect \"a@example.net\"; redirect \"b@example.net\";").await;
    let o = f.eval().await;
    assert_eq!(o.keep, RulesOutcome::implicit_keep().keep);
    assert!(o.redirect.is_empty());
    assert_eq!(o.errors.len(), 1);
    assert!(o.errors[0].starts_with("скрипт пользователя: "), "{o:?}");
    assert!(!o.errors[0].contains("fox") && !o.errors[0].contains("Report"));

    // Действия скрипта организации остаются, ошибка пользователя оставляет письмо во «Входящих».
    f.org("require [\"fileinto\"]; fileinto \"Архив\";").await;
    assert_eq!(f.folders().await, ["Архив", "INBOX"]);

    // Сохранённый скрипт с ошибкой (хранилище текст не проверяет).
    f.user("unknown;").await;
    f.store.set_org_script(f.scope.tenant, None).await.expect("org");
    let o = f.eval().await;
    assert_eq!(o.keep, RulesOutcome::implicit_keep().keep);
    assert!(o.errors[0].contains("неизвестная команда"), "{o:?}");
    f.org("if {").await;
    let o = f.eval().await;
    assert_eq!(o.errors.len(), 2);
    assert!(o.errors[0].starts_with("скрипт организации: "), "{o:?}");
}

/// Защита `redirect` от петли: по числу `Received` и по метке узла.
#[tokio::test]
async fn redirect_loop_protection() {
    let mut f = Fixture::new();
    f.user("redirect \"a@example.net\";").await;
    let o = f.eval().await;
    assert_eq!(o.redirect, ["a@example.net"]);
    assert!(o.keep.is_empty() && o.errors.is_empty());

    // Письмо уже проходило этот узел трижды (sieve.max_node_hops).
    let hop = "Received: from x.example.net ([198.51.100.7])\r\n\tby MAIL.example.org (The Post) with ESMTP id 2\r\n";
    f.input.message = format!("{hop}{hop}{MAIL}").into_bytes().into();
    let o = f.eval().await;
    assert!(o.redirect.is_empty());
    assert_eq!(o.keep, RulesOutcome::implicit_keep().keep);
    assert!(o.errors[0].contains("петля"), "{o:?}");

    // Чужие узлы не считаются, пока Received не больше sieve.max_received.
    let foreign = "Received: from a.example.net by b.example.net with ESMTP id 3\r\n";
    f.input.message = format!("{}{MAIL}", foreign.repeat(49)).into_bytes().into();
    assert_eq!(f.eval().await.redirect, ["a@example.net"]);
    f.input.message = format!("{}{MAIL}", foreign.repeat(50)).into_bytes().into();
    let o = f.eval().await;
    assert!(o.redirect.is_empty() && o.errors[0].contains("Received"), "{o:?}");

    // redirect :copy при петле: письмо остаётся во «Входящих» один раз. Без redirect петля не проверяется.
    f.user("require \"copy\"; redirect :copy \"a@example.net\";").await;
    let o = f.eval().await;
    assert_eq!(o.keep.len(), 1);
    f.user("discard;").await;
    let o = f.eval().await;
    assert!(o.discard && o.errors.is_empty());
}

/// Цели fuzz на примерах: скрипты и письма корпуса проходят проверки инвариантов.
#[test]
fn fuzz_examples() {
    let scripts: &[&str] = &[
        "",
        "keep;",
        "if {",
        "require \"fileinto\"; fileinto \"Work\"; fileinto :copy \"x\";",
        "require [\"body\", \"vacation\"]; if body :contains \"fox\" { vacation \"x\"; discard; }",
        "redirect \"a@example.net\"; redirect \"b@example.net\"; redirect \"c@example.net\";",
        "if header :matches \"subject\" \"*a*a*a*a*b\" { stop; } keep; keep; keep;",
        "\u{0}\u{1}require",
    ];
    for s in scripts {
        fuzz::parse(s.as_bytes());
    }
    fuzz::parse(&[0xff, 0xfe, b'"', b'{']);
    for mail in tp_testkit::corpus().expect("corpus") {
        for s in scripts {
            let mut data = s.as_bytes().to_vec();
            data.push(0);
            data.extend_from_slice(&mail.raw);
            fuzz::run(&data);
        }
    }
}

#[test]
fn config_defaults() {
    let c = SieveConfig::default();
    assert_eq!(c.limits(), crate::Limits::default());
    assert!(c.vacation.min_days <= c.vacation.default_days);
    assert!(c.vacation.default_days <= c.vacation.max_days);
    assert!(c.managesieve.max_literal >= c.max_script_size);
}
