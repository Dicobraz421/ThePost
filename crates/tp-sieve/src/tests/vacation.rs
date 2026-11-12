//! Автоответ (SIEVE-06): условия «не отвечать», период `:days`, `:handle`, ответ из настройки `Vacation`.

use std::time::Duration;

use time::OffsetDateTime;
use tp_contracts::AccountId;
use tp_contracts::TenantId;
use tp_contracts::meta::AccountScope;
use tp_contracts::sieve::{AutoReply, ReplyBody, RulesInput, Vacation};
use tp_testkit::{ManualClock, MemVacationLog};

use crate::run::parse_headers;
use crate::vacation::{Source, reply, suppressed};
use crate::{Action, Context, Limits, VacationAction, VacationConfig, compile, run};

const MAIL: &str = "From: Alice <alice@example.com>\r\nTo: Bob <bob@example.org>\r\nSubject: =?utf-8?b?0J7RgtGH0ZHRgg==?=\r\n\
                    Message-ID: <m1@example.com>\r\nReferences: <r1@example.com> <r2@example.com>\r\n\r\nтекст";

const DAY: Duration = Duration::from_secs(24 * 3600);

fn input(mail: &str, mail_from: &str, recipient: &str) -> RulesInput {
    RulesInput {
        scope: AccountScope {
            tenant: TenantId::new(),
            account: AccountId::new(),
        },
        recipient: recipient.to_owned(),
        mail_from: mail_from.to_owned(),
        message: mail.as_bytes().to_vec().into(),
        mailboxes: Vec::new(),
    }
}

/// Параметры `vacation` из скрипта.
fn action(script: &str) -> VacationAction {
    let program = compile(
        format!("require \"vacation\"; {script}").as_bytes(),
        &Limits::default(),
    )
    .expect("script");
    let o = run(&program, &Context::default(), &Limits::default());
    match o.actions.into_iter().next() {
        Some(Action::Vacation(v)) => v,
        other => panic!("{script}: {other:?}"),
    }
}

struct Fixture {
    clock: ManualClock,
    log: MemVacationLog,
    config: VacationConfig,
}

impl Fixture {
    fn new() -> Self {
        let clock = ManualClock::new(OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000));
        Self {
            log: MemVacationLog::new(clock.shared()),
            clock,
            config: VacationConfig::default(),
        }
    }

    async fn script(&self, v: &VacationAction, input: &RulesInput) -> Option<AutoReply> {
        let headers = parse_headers(&input.message);
        reply(Source::Script(v), input, &headers, &self.config, &self.log)
            .await
            .expect("log")
    }

    async fn setting(&self, v: &Vacation, input: &RulesInput) -> Option<AutoReply> {
        let headers = parse_headers(&input.message);
        let source = Source::Setting(v, self.clock.shared().now());
        reply(source, input, &headers, &self.config, &self.log)
            .await
            .expect("log")
    }
}

#[tokio::test]
async fn reply_from_script() {
    let f = Fixture::new();
    let i = input(MAIL, "alice@example.com", "bob@example.org");
    let r = f.script(&action("vacation \"Я в отпуске\";"), &i).await;
    assert_eq!(
        r,
        Some(AutoReply {
            to: "alice@example.com".into(),
            from: "bob@example.org".into(),
            subject: "Автоответ: Отчёт".into(),
            body: ReplyBody::Text("Я в отпуске".into()),
            in_reply_to: Some("<m1@example.com>".into()),
            references: vec![
                "<r1@example.com>".into(),
                "<r2@example.com>".into(),
                "<m1@example.com>".into()
            ],
        })
    );

    // :subject, :from (свой адрес с подадресом), :mime.
    let i = input(MAIL, "<carol@example.com>", "bob+x@example.org");
    let v = action(
        "vacation :subject \"Нет на месте\" :from \"Bob <bob+away@example.org>\" :mime \"Content-Type: text/plain\r\n\r\nx\";",
    );
    let r = f.script(&v, &i).await.expect("reply");
    assert_eq!(r.to, "carol@example.com");
    assert_eq!(r.from, "bob+away@example.org");
    assert_eq!(r.subject, "Нет на месте");
    assert!(matches!(r.body, ReplyBody::Mime(ref m) if m.starts_with("Content-Type")));

    // Чужой адрес в :from не принимается: ответ — от адреса получателя.
    let i = input(MAIL, "dave@example.com", "bob@example.org");
    let v = action("vacation :from \"ceo@example.org\" :addresses \"ceo@example.org\" \"x\";");
    assert_eq!(
        f.script(&v, &i).await.expect("reply").from,
        "bob@example.org"
    );

    // Без темы и Message-ID; References — из In-Reply-To.
    let mail =
        "From: a@example.com\r\nTo: bob@example.org\r\nIn-Reply-To: <p@example.com>\r\n\r\nx";
    let i = input(mail, "erin@example.com", "bob@example.org");
    let r = f
        .script(&action("vacation \"x\";"), &i)
        .await
        .expect("reply");
    assert_eq!(r.subject, "Автоответ");
    assert_eq!(r.in_reply_to, None);
    assert_eq!(r.references, ["<p@example.com>"]);
}

/// Каждое условие «не отвечать» (RFC 5230 §4.5, RFC 3834).
#[tokio::test]
async fn no_reply_conditions() {
    let f = Fixture::new();
    let v = action("vacation \"x\";");
    let with = |extra: &str| MAIL.replacen("Subject:", &format!("{extra}\r\nSubject:"), 1);
    let cases: Vec<(String, &str, &str, &str)> = vec![
        (MAIL.to_owned(), "", "bob@example.org", "пустой отправитель"),
        (
            MAIL.to_owned(),
            "<>",
            "bob@example.org",
            "пустой отправитель",
        ),
        (
            with("Auto-Submitted: auto-generated"),
            "a@example.com",
            "bob@example.org",
            "Auto-Submitted",
        ),
        (
            with("Auto-Submitted: auto-replied; x=y"),
            "a@example.com",
            "bob@example.org",
            "Auto-Submitted",
        ),
        (
            with("List-Id: <dev.example.com>"),
            "a@example.com",
            "bob@example.org",
            "List-",
        ),
        (
            with("List-Unsubscribe: <mailto:u@example.com>"),
            "a@example.com",
            "bob@example.org",
            "List-",
        ),
        (
            with("Precedence: bulk"),
            "a@example.com",
            "bob@example.org",
            "Precedence",
        ),
        (
            with("Precedence: List"),
            "a@example.com",
            "bob@example.org",
            "Precedence",
        ),
        (
            with("Precedence: junk"),
            "a@example.com",
            "bob@example.org",
            "Precedence",
        ),
        (
            MAIL.to_owned(),
            "a@example.com",
            "team@example.org",
            "среди получателей",
        ),
        (
            MAIL.to_owned(),
            "BOB+me@example.org",
            "bob@example.org",
            "собственного адреса",
        ),
    ];
    for (mail, from, rcpt, reason) in &cases {
        let i = input(mail, from, rcpt);
        let headers = parse_headers(&i.message);
        let got = suppressed(Source::Script(&v), &i, &headers);
        assert!(
            got.is_some_and(|r| r.contains(reason)),
            "{from} {rcpt} {reason}: {got:?}"
        );
        assert_eq!(f.script(&v, &i).await, None, "{reason}");
    }
    // Отвечать можно: Auto-Submitted: no, обычный Precedence, получатель с подадресом, адрес в Cc.
    for (mail, rcpt) in [
        (with("Auto-Submitted: no"), "bob@example.org"),
        (with("Precedence: first-class"), "bob@example.org"),
        (MAIL.to_owned(), "Bob+news@Example.org"),
        (
            MAIL.replace(
                "To: Bob <bob@example.org>",
                "To: x@example.org\r\nCc: bob+a@example.org",
            ),
            "bob@example.org",
        ),
    ] {
        let i = input(&mail, "a@example.com", rcpt);
        let headers = parse_headers(&i.message);
        assert_eq!(suppressed(Source::Script(&v), &i, &headers), None, "{mail}");
    }
    // :addresses — другие адреса пользователя: письмо на псевдоним и письмо от псевдонима.
    let alias = action("vacation :addresses [\"Robert@example.org\"] \"x\";");
    let mail = MAIL.replace("bob@example.org", "robert@example.org");
    let i = input(&mail, "a@example.com", "bob@example.org");
    let headers = parse_headers(&i.message);
    assert!(suppressed(Source::Script(&v), &i, &headers).is_some());
    assert_eq!(suppressed(Source::Script(&alias), &i, &headers), None);
    let i = input(MAIL, "robert@example.org", "bob@example.org");
    let headers = parse_headers(&i.message);
    assert!(suppressed(Source::Script(&alias), &i, &headers).is_some());
}

/// Период `:days`: повторный ответ тому же отправителю — не раньше; пределы — из конфигурации.
#[tokio::test]
async fn days_period() {
    let f = Fixture::new();
    let i = input(MAIL, "alice@example.com", "bob@example.org");
    let other = RulesInput {
        mail_from: "carol@example.com".into(),
        ..i.clone()
    };
    let v = action("vacation :days 3 \"x\";");
    assert!(f.script(&v, &i).await.is_some());
    assert!(f.script(&v, &i).await.is_none());
    // Отправитель сравнивается без учёта регистра; другой отправитель — свой период.
    let upper = RulesInput {
        mail_from: "ALICE@example.com".into(),
        ..i.clone()
    };
    assert!(f.script(&v, &upper).await.is_none());
    assert!(f.script(&v, &other).await.is_some());
    f.clock.advance(DAY * 3 - Duration::from_secs(1));
    assert!(f.script(&v, &i).await.is_none());
    f.clock.advance(Duration::from_secs(1));
    assert!(f.script(&v, &i).await.is_some());

    // Без :days — default_days (7).
    let d = action("vacation \"по умолчанию\";");
    assert!(f.script(&d, &i).await.is_some());
    f.clock.advance(DAY * 6);
    assert!(f.script(&d, &i).await.is_none());
    f.clock.advance(DAY);
    assert!(f.script(&d, &i).await.is_some());

    // :days 0 поднимается до min_days, :days 365 опускается до max_days (30).
    let zero = action("vacation :days 0 \"ноль\";");
    assert!(f.script(&zero, &i).await.is_some());
    f.clock.advance(DAY - Duration::from_secs(1));
    assert!(f.script(&zero, &i).await.is_none());
    f.clock.advance(Duration::from_secs(1));
    assert!(f.script(&zero, &i).await.is_some());
    let long = action("vacation :days 365 \"год\";");
    assert!(f.script(&long, &i).await.is_some());
    f.clock.advance(DAY * 29);
    assert!(f.script(&long, &i).await.is_none());
    f.clock.advance(DAY);
    assert!(f.script(&long, &i).await.is_some());
}

/// `:handle`: один handle — один период при разных текстах; без handle период — по теме и тексту.
#[tokio::test]
async fn handle() {
    let f = Fixture::new();
    let i = input(MAIL, "alice@example.com", "bob@example.org");
    assert!(
        f.script(&action("vacation :handle \"h\" \"первый\";"), &i)
            .await
            .is_some()
    );
    assert!(
        f.script(&action("vacation :handle \"h\" \"второй\";"), &i)
            .await
            .is_none()
    );
    assert!(
        f.script(&action("vacation :handle \"g\" \"второй\";"), &i)
            .await
            .is_some()
    );
    assert!(f.script(&action("vacation \"текст\";"), &i).await.is_some());
    assert!(f.script(&action("vacation \"текст\";"), &i).await.is_none());
    assert!(
        f.script(&action("vacation \"другой текст\";"), &i)
            .await
            .is_some()
    );
    assert!(
        f.script(&action("vacation :subject \"s\" \"текст\";"), &i)
            .await
            .is_some()
    );
}

/// Ответ из настройки `Vacation` (JMAP `VacationResponse`, Admin API) с датами `from`/`to`.
#[tokio::test]
async fn reply_from_setting() {
    let f = Fixture::new();
    let now = f.clock.shared().now();
    let i = input(MAIL, "alice@example.com", "bob+x@example.org");
    let v = Vacation {
        enabled: true,
        from: Some(now + time::Duration::days(1)),
        to: Some(now + time::Duration::days(3)),
        subject: None,
        text_body: Some("Вернусь в понедельник".into()),
        html_body: None,
    };
    // До начала — ответа нет.
    assert_eq!(f.setting(&v, &i).await, None);
    f.clock.advance(DAY);
    let r = f.setting(&v, &i).await.expect("reply");
    assert_eq!(r.to, "alice@example.com");
    assert_eq!(r.from, "bob+x@example.org");
    assert_eq!(r.subject, "Автоответ: Отчёт");
    assert_eq!(r.body, ReplyBody::Text("Вернусь в понедельник".into()));
    assert_eq!(r.in_reply_to.as_deref(), Some("<m1@example.com>"));
    // Повтор тому же отправителю — в пределах default_days нет.
    assert_eq!(f.setting(&v, &i).await, None);
    // После конца — ответа нет даже новому отправителю.
    let other = RulesInput {
        mail_from: "carol@example.com".into(),
        ..i.clone()
    };
    f.clock.advance(DAY * 2);
    assert_eq!(f.setting(&v, &other).await, None);

    // Тема и HTML из настройки; выключенный автоответ не отвечает; условия «не отвечать» действуют.
    let html = Vacation {
        enabled: true,
        from: None,
        to: None,
        subject: Some("В отпуске".into()),
        text_body: Some("т".into()),
        html_body: Some("<p>т</p>".into()),
    };
    let r = f.setting(&html, &other).await.expect("reply");
    assert_eq!(r.subject, "В отпуске");
    assert_eq!(
        r.body,
        ReplyBody::Html {
            text: Some("т".into()),
            html: "<p>т</p>".into()
        }
    );
    let dave = RulesInput {
        mail_from: "dave@example.com".into(),
        ..i.clone()
    };
    let off = Vacation {
        enabled: false,
        ..html.clone()
    };
    assert_eq!(f.setting(&off, &dave).await, None);
    let bounce = RulesInput {
        mail_from: String::new(),
        ..i.clone()
    };
    assert_eq!(f.setting(&html, &bounce).await, None);
    assert!(f.setting(&html, &dave).await.is_some());
}
