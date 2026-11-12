//! Выполнение (SIEVE-03): письма корпуса testkit × скрипты → действия; управление, implicit keep, компараторы,
//! лимиты шагов и действий.

use proptest::prelude::*;
use tp_testkit::corpus::corpus_mail;

use crate::{Action, Context, Limits, Outcome, check, run};

const ENV_FROM: &str = "alice@example.com";
const ENV_TO: &str = "bob+lists@example.org";

fn exec_raw(script: &str, message: &[u8], limits: &Limits) -> Outcome {
    let program = match check(script) {
        Ok(p) => p,
        Err(d) => panic!("{script}: {d:?}"),
    };
    let mailboxes = ["INBOX".to_owned(), "Work".to_owned()];
    let ctx = Context {
        message,
        envelope_from: ENV_FROM,
        envelope_to: ENV_TO,
        mailboxes: &mailboxes,
    };
    run(&program, &ctx, limits)
}

/// Кратко: действия через запятую и `implicit-keep` в конце; ошибка — `error`.
fn summary(o: &Outcome) -> String {
    let mut v: Vec<String> = o
        .actions
        .iter()
        .map(|a| match a {
            Action::Keep { flags } if flags.is_empty() => "keep".to_owned(),
            Action::Keep { flags } => format!("keep {}", flags.join(" ")),
            Action::Discard => "discard".to_owned(),
            Action::FileInto { mailbox, copy, .. } => {
                format!("fileinto{} {mailbox}", if *copy { " :copy" } else { "" })
            }
            Action::Redirect { address, copy } => {
                format!("redirect{} {address}", if *copy { " :copy" } else { "" })
            }
            Action::Vacation(_) => "vacation".to_owned(),
        })
        .collect();
    if o.error.is_some() {
        v.push("error".to_owned());
    }
    if o.implicit_keep {
        v.push("implicit-keep".to_owned());
    }
    v.join(", ")
}

fn on_mail(script: &str, mail: &str) -> String {
    let raw = corpus_mail(mail).expect(mail).raw;
    summary(&exec_raw(script, &raw, &Limits::default()))
}

fn on_text(script: &str, message: &str) -> String {
    summary(&exec_raw(script, message.as_bytes(), &Limits::default()))
}

/// Условие верно на письме — `fileinto "yes"`, иначе implicit keep.
fn holds(cond: &str, mail: &str) -> bool {
    let script = format!(
        "require [\"fileinto\", \"relational\", \"comparator-i;ascii-numeric\", \"envelope\", \"subaddress\", \"mailbox\"];\nif {cond} {{ fileinto \"yes\"; }}"
    );
    match on_mail(&script, mail).as_str() {
        "fileinto yes" => true,
        "implicit-keep" => false,
        other => panic!("{cond}: {other}"),
    }
}

/// Таблица: письмо корпуса × условие → ожидаемый результат.
#[test]
fn corpus_table() {
    let table: &[(&str, &str, bool)] = &[
        // header: :is, :contains, :matches, компараторы
        ("plain-ascii", r#"header :is "subject" "Plain ASCII""#, true),
        ("plain-ascii", r#"header :is "Subject" "plain ascii""#, true),
        (
            "plain-ascii",
            r#"header :is :comparator "i;octet" "subject" "plain ascii""#,
            false,
        ),
        ("plain-ascii", r#"header :contains "subject" "ASC""#, true),
        ("plain-ascii", r#"header :contains "subject" """#, true),
        ("plain-ascii", r#"header :contains "x-nope" """#, false),
        (
            "plain-ascii",
            r#"header :matches "subject" "Plain ?SCII""#,
            true,
        ),
        ("plain-ascii", r#"header :matches "subject" "*ASC*""#, true),
        (
            "plain-ascii",
            r#"header :matches "subject" "Plain\\*""#,
            false,
        ),
        ("plain-ascii", r#"header :matches "subject" "P*n*I""#, true),
        ("plain-ascii", r#"header :matches "subject" "P*n*X""#, false),
        (
            "plain-ascii",
            r#"header :is ["X-A", "To"] ["nobody", "Bob <bob@example.org>"]"#,
            true,
        ),
        // address: части адреса
        (
            "plain-ascii",
            r#"address :is "from" "alice@example.com""#,
            true,
        ),
        (
            "plain-ascii",
            r#"address :all :is "from" "Alice Example <alice@example.com>""#,
            false,
        ),
        (
            "plain-ascii",
            r#"address :localpart :is "from" "alice""#,
            true,
        ),
        (
            "plain-ascii",
            r#"address :domain :is ["from", "to"] "EXAMPLE.org""#,
            true,
        ),
        (
            "plain-ascii",
            r#"address :domain :is "from" "example.org""#,
            false,
        ),
        // exists, size, true/false, allof/anyof/not
        ("plain-ascii", r#"exists ["From", "message-id"]"#, true),
        ("plain-ascii", r#"exists ["From", "X-Nope"]"#, false),
        ("plain-ascii", "size :over 300", true),
        ("plain-ascii", "size :over 334", false),
        ("plain-ascii", "size :under 335", true),
        ("plain-ascii", "size :under 1K", true),
        ("plain-ascii", "true", true),
        ("plain-ascii", "false", false),
        ("plain-ascii", "not false", true),
        ("plain-ascii", "allof (true, exists \"To\")", true),
        ("plain-ascii", "allof (true, false)", false),
        ("plain-ascii", "anyof (false, false)", false),
        ("plain-ascii", "anyof (false, not false)", true),
        // RFC 2047 и разбор адресов RFC 5322
        (
            "encoded-addresses",
            r#"header :is "subject" "Адреса""#,
            true,
        ),
        (
            "encoded-addresses",
            r#"header :contains "from" "Иван Петров""#,
            true,
        ),
        (
            "encoded-addresses",
            r#"header :contains "to" "Отдел продаж""#,
            true,
        ),
        (
            "encoded-addresses",
            r#"address :domain :is "from" "example.ru""#,
            true,
        ),
        (
            "encoded-addresses",
            r#"address :localpart :is "to" "john""#,
            true,
        ),
        (
            "encoded-addresses",
            r#"address :is "to" "sales@example.ru""#,
            true,
        ),
        (
            "encoded-addresses",
            r#"address :is "reply-to" "reply@example.com""#,
            true,
        ),
        (
            "encoded-addresses",
            r#"address :count "eq" :comparator "i;ascii-numeric" "cc" "2""#,
            true,
        ),
        (
            "encoded-addresses",
            r#"address :count "ge" :comparator "i;ascii-numeric" ["to", "cc"] "4""#,
            true,
        ),
        (
            "plain-koi8r-8bit",
            r#"header :is "subject" "Письмо в КОИ-8""#,
            true,
        ),
        (
            "raw-8bit-headers",
            r#"header :is "subject" "Тема без кодирования""#,
            true,
        ),
        (
            "long-subject-folded",
            r#"header :matches "subject" "A very long subject*by a wide margin""#,
            true,
        ),
        (
            "long-subject-folded",
            r#"header :contains "subject" "because it exceeds""#,
            true,
        ),
        (
            "long-subject-folded",
            r#"header :count "eq" :comparator "i;ascii-numeric" "subject" "1""#,
            true,
        ),
        // envelope и subaddress (конверт — из Context)
        (
            "plain-ascii",
            r#"envelope :is "from" "alice@example.com""#,
            true,
        ),
        ("plain-ascii", r#"envelope :detail "to" "lists""#, true),
        ("plain-ascii", r#"envelope :user "to" "bob""#, true),
        (
            "plain-ascii",
            r#"envelope :domain "to" "example.org""#,
            true,
        ),
        ("plain-ascii", r#"envelope :detail "from" "lists""#, false),
        ("plain-ascii", r#"address :detail "to" "lists""#, false),
        // mailbox
        ("plain-ascii", r#"mailboxexists ["inbox", "Work"]"#, true),
        ("plain-ascii", r#"mailboxexists ["Work", "Archive"]"#, false),
    ];
    for (mail, cond, expected) in table {
        assert_eq!(holds(cond, mail), *expected, "{mail}: {cond}");
    }
}

/// Управление: if/elsif/else, stop, keep, discard, implicit keep (RFC 5228 §2.10.2), :copy.
#[test]
fn control_and_actions() {
    let cases: &[(&str, &str)] = &[
        ("", "implicit-keep"),
        ("if false { discard; }", "implicit-keep"),
        ("keep;", "keep"),
        ("discard;", "discard"),
        ("stop; discard;", "implicit-keep"),
        (
            "require \"fileinto\"; fileinto \"A\"; stop; fileinto \"B\";",
            "fileinto A",
        ),
        (
            "require [\"fileinto\", \"copy\"]; fileinto :copy \"A\";",
            "fileinto :copy A, implicit-keep",
        ),
        ("redirect \"x@example.org\";", "redirect x@example.org"),
        (
            "require \"copy\"; redirect :copy \"x@example.org\";",
            "redirect :copy x@example.org, implicit-keep",
        ),
        (
            "require \"fileinto\";\nif header :is \"subject\" \"nope\" { fileinto \"A\"; }\n\
             elsif header :contains \"subject\" \"ASCII\" { fileinto \"B\"; }\nelse { fileinto \"C\"; }",
            "fileinto B",
        ),
        (
            "require \"fileinto\";\nif false { fileinto \"A\"; } elsif false { fileinto \"B\"; } else { fileinto \"C\"; }",
            "fileinto C",
        ),
        (
            "if true { if true { keep; stop; } discard; } discard;",
            "keep",
        ),
        (
            "require \"vacation\"; vacation :days 3 \"Я в отпуске\"; ",
            "vacation, implicit-keep",
        ),
        (
            "require \"vacation\"; vacation \"a\"; vacation \"b\";",
            "error, implicit-keep",
        ),
        (
            "require \"body\"; keep; if body :contains \"fox\" { discard; }",
            "error, implicit-keep",
        ),
    ];
    for (script, expected) in cases {
        assert_eq!(on_mail(script, "plain-ascii"), *expected, "{script}");
    }
}

/// imap4flags: внутренняя переменная, `:flags`, флаги implicit keep, `hasflag`.
#[test]
fn flags() {
    let raw = corpus_mail("plain-ascii").expect("mail").raw;
    let o = exec_raw(
        "require [\"imap4flags\", \"fileinto\"];\n\
         setflag \"\\\\Seen \\\\Flagged\";\naddflag [\"$Work\", \"\\\\seen\"];\nremoveflag \"\\\\Flagged\";\n\
         fileinto \"A\";\nkeep :flags \"\\\\Deleted\";\n\
         if hasflag :is \"$work\" { addflag \"Later\"; }",
        &raw,
        &Limits::default(),
    );
    assert_eq!(o.error, None);
    assert!(!o.implicit_keep);
    assert_eq!(
        o.actions,
        [
            Action::FileInto {
                mailbox: "A".into(),
                copy: false,
                create: false,
                flags: vec!["\\Seen".into(), "$Work".into()],
            },
            Action::Keep {
                flags: vec!["\\Deleted".into()],
            },
        ]
    );
    assert_eq!(o.flags, ["\\Seen", "$Work", "Later"]);
}

/// Компаратор i;ascii-numeric и relational :value (RFC 4790 §9.1, RFC 5231).
#[test]
fn numeric_and_relational() {
    let mail = "X-Spam-Score: 007\r\nX-Spam-Score: 3\r\nX-Text: abc\r\nSubject: t\r\n\r\nbody";
    let hit = |cond: &str| {
        let script = format!(
            "require [\"relational\", \"comparator-i;ascii-numeric\"];\nif {cond} {{ discard; }}"
        );
        on_text(&script, mail) == "discard"
    };
    let table: &[(&str, bool)] = &[
        (
            r#"header :is :comparator "i;ascii-numeric" "x-spam-score" "7""#,
            true,
        ),
        (
            r#"header :is :comparator "i;ascii-numeric" "x-spam-score" "07 штук""#,
            true,
        ),
        (
            r#"header :value "gt" :comparator "i;ascii-numeric" "x-spam-score" "6""#,
            true,
        ),
        (
            r#"header :value "gt" :comparator "i;ascii-numeric" "x-spam-score" "7""#,
            false,
        ),
        (
            r#"header :value "ge" :comparator "i;ascii-numeric" "x-spam-score" "7""#,
            true,
        ),
        (
            r#"header :value "lt" :comparator "i;ascii-numeric" "x-spam-score" "3""#,
            false,
        ),
        (
            r#"header :value "le" :comparator "i;ascii-numeric" "x-spam-score" "3""#,
            true,
        ),
        (
            r#"header :value "ne" :comparator "i;ascii-numeric" "x-spam-score" "7""#,
            true,
        ),
        (
            r#"header :value "eq" :comparator "i;ascii-numeric" "x-nope" "7""#,
            false,
        ),
        // Строка без цифр — «бесконечность»: больше любого числа.
        (
            r#"header :value "gt" :comparator "i;ascii-numeric" "x-text" "999999999999999999999""#,
            true,
        ),
        (
            r#"header :value "eq" :comparator "i;ascii-numeric" "x-text" "zzz""#,
            true,
        ),
        // :value с i;ascii-casemap — порядок строк.
        (r#"header :value "lt" "x-text" "abd""#, true),
        (r#"header :value "gt" "x-text" "ABC""#, false),
        (
            r#"header :count "eq" :comparator "i;ascii-numeric" "x-spam-score" "2""#,
            true,
        ),
        (
            r#"header :count "gt" :comparator "i;ascii-numeric" "x-nope" "0""#,
            false,
        ),
        (
            r#"header :count "eq" :comparator "i;ascii-numeric" "x-nope" "0""#,
            true,
        ),
    ];
    for (cond, expected) in table {
        assert_eq!(hit(cond), *expected, "{cond}");
    }
}

/// Превышение шагов и действий — ошибка выполнения и implicit keep без действий.
#[test]
fn limits() {
    let raw = corpus_mail("plain-ascii").expect("mail").raw;
    let few = Limits {
        max_steps: 5,
        ..Limits::default()
    };
    let o = exec_raw(&"keep;".repeat(10), &raw, &few);
    assert!(o.implicit_keep && o.actions.is_empty(), "{o:?}");
    assert!(
        o.error.as_deref().is_some_and(|e| e.contains("шагов")),
        "{o:?}"
    );
    assert_eq!(summary(&exec_raw("keep;", &raw, &few)), "keep");

    let actions = Limits {
        max_actions: 3,
        ..Limits::default()
    };
    let o = exec_raw(&"keep;".repeat(4), &raw, &actions);
    assert_eq!(summary(&o), "error, implicit-keep");
    assert!(o.error.as_deref().is_some_and(|e| e.contains("действий")));

    // Сопоставление :matches расходует шаги: тяжёлый шаблон на длинном заголовке упирается в лимит.
    let mail = format!("Subject: {}\r\n\r\nx", "a".repeat(5000));
    let steps = Limits {
        max_steps: 100_000,
        ..Limits::default()
    };
    let heavy = format!(
        r#"if header :matches "subject" "*{}b" {{ discard; }}"#,
        "a".repeat(100)
    );
    let o = exec_raw(&heavy, mail.as_bytes(), &steps);
    assert_eq!(summary(&o), "error, implicit-keep");
    let o = exec_raw(
        r#"if header :matches "subject" "a*a" { discard; }"#,
        mail.as_bytes(),
        &steps,
    );
    assert_eq!(summary(&o), "discard");
}

/// Письмо без заголовков и мусор вместо письма: тесты ложны, implicit keep.
#[test]
fn broken_messages() {
    for message in [&b""[..], b"\xff\xfe\x00", b"no colon here\r\n\r\n"] {
        let o = exec_raw(
            r#"if anyof (exists "From", header :contains "subject" "", address :all :contains "to" "") { discard; }"#,
            message,
            &Limits::default(),
        );
        assert_eq!(summary(&o), "implicit-keep", "{message:?}");
    }
    let raw = corpus_mail("broken-bad-encoded-word").expect("mail").raw;
    let o = exec_raw(r#"if exists "Subject" { keep; }"#, &raw, &Limits::default());
    assert_eq!(o.error, None);
}

const RICH: &str = r#"require ["fileinto", "envelope", "subaddress", "relational", "comparator-i;ascii-numeric", "imap4flags", "mailbox", "copy"];
if anyof (header :matches "subject" ["*a?b*", "\\*"], address :domain :contains ["from", "to", "cc"] "example",
          envelope :detail "to" "x", header :value "gt" :comparator "i;ascii-numeric" "x-score" "5",
          address :count "ge" :comparator "i;ascii-numeric" "to" "2", exists "Message-ID", size :over 1K) {
    addflag "\\Seen";
    fileinto :copy "A";
}
elsif not header :is "from" "" { if hasflag "x" { stop; } redirect "a@b.example"; }
else { discard; }
"#;

proptest! {
    /// Выполнение над произвольными байтами письма и над письмами корпуса с порчей не паникует.
    #[test]
    fn run_arbitrary_message(data in proptest::collection::vec(any::<u8>(), 0..4096)) {
        let o = exec_raw(RICH, &data, &Limits::default());
        prop_assert!(o.error.is_none(), "{o:?}");
    }
}
