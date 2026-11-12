//! Проверка скрипта (SIEVE-02): require, аргументы, компараторы, сообщения об ошибках с позицией;
//! скрипты Roundcube и дополнения Sieve для Thunderbird.

use super::assert_error;
use crate::compile::{Cmd, Comparator, MatchKind, Part, Rel, Test, decode_encoded};
use crate::{EXTENSIONS, Limits, Program, compile};

fn ok(text: &str) -> Program {
    match compile(text.as_bytes(), &Limits::default()) {
        Ok(p) => p,
        Err(d) => panic!(
            "{text}\n{}",
            d.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

/// Скрипт в стиле Roundcube managesieve (правила с комментариями `# rule:[…]`, табуляция, `text:`).
const ROUNDCUBE: &str = "require [\"fileinto\",\"vacation\",\"copy\",\"imap4flags\",\"relational\",\"comparator-i;ascii-numeric\",\"body\",\"envelope\",\"subaddress\",\"mailbox\"];\r
# rule:[Спам]\r
if anyof (header :contains \"subject\" \"[SPAM]\", header :value \"ge\" :comparator \"i;ascii-numeric\" \"X-Spam-Score\" \"5\")\r
{\r
\tfileinto :create \"Junk\";\r
\tstop;\r
}\r
# rule:[Отпуск]\r
if true\r
{\r
\tvacation :days 7 :addresses [\"anna@example.org\",\"a.ivanova@example.org\"] :subject \"Я в отпуске\" :from \"anna@example.org\" text:\r
Вернусь 20 июля.\r
.\r
;\r
}\r
# rule:[Партнёры]\r
if allof (address :domain :is \"from\" \"partner.example\", not header :contains \"subject\" \"реклама\")\r
{\r
\tredirect :copy \"boss@example.org\";\r
\taddflag \"\\\\Flagged\";\r
\tfileinto :copy \"Partners\";\r
}\r
# rule:[Большие]\r
if size :over 10M\r
{\r
\tfileinto \"Big\";\r
}\r
# rule:[Рассылки]\r
if envelope :detail :is \"to\" \"lists\"\r
{\r
\tfileinto \"Lists\";\r
}\r
# rule:[Тело]\r
if body :text :contains \"unsubscribe\"\r
{\r
\tsetflag \"\\\\Seen\";\r
\tkeep;\r
}\r
";

/// Скрипт в стиле дополнения Sieve для Thunderbird.
const THUNDERBIRD: &str = r#"require ["fileinto","imap4flags","envelope"];

# Рассылка разработчиков
if header :contains "List-Id" "<dev.lists.example.org>"
{
  fileinto "INBOX.dev";
  stop;
}

if anyof (header :is "X-Spam-Flag" "YES", header :contains "Subject" "***SPAM***")
{
  setflag "\\Seen";
  fileinto "Junk";
  stop;
}

if not exists "Message-ID"
{
  discard;
  stop;
}

if envelope :all :is "from" "noreply@example.org"
{
  fileinto :flags ["\\Seen"] "Notifications";
}
elsif header :matches "Subject" "Re: *"
{
  keep;
}
else
{
  keep :flags "\\Flagged";
}
"#;

#[test]
fn client_scripts() {
    let rc = ok(ROUNDCUBE);
    assert_eq!(
        rc.extensions(),
        [
            "body",
            "comparator-i;ascii-numeric",
            "copy",
            "envelope",
            "fileinto",
            "imap4flags",
            "mailbox",
            "relational",
            "subaddress",
            "vacation"
        ]
    );
    let Cmd::If { branches, .. } = &rc.commands[1] else {
        panic!("{:?}", rc.commands[1]);
    };
    let Cmd::Vacation(v) = &branches[0].1[0] else {
        panic!("{branches:?}");
    };
    assert_eq!(v.days, Some(7));
    assert_eq!(v.subject.as_deref(), Some("Я в отпуске"));
    assert_eq!(v.addresses, ["anna@example.org", "a.ivanova@example.org"]);
    assert_eq!(v.reason, "Вернусь 20 июля.\r\n");
    ok(THUNDERBIRD);
}

#[test]
fn supported_extensions() {
    let all = format!(
        "require [{}, \"comparator-i;octet\", \"comparator-i;ascii-casemap\"];",
        EXTENSIONS
            .iter()
            .map(|e| format!("{e:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    assert_eq!(ok(&all).extensions().len(), EXTENSIONS.len());
    // Регистр имени расширения не важен.
    assert_eq!(
        ok("require \"FileInto\"; fileinto \"x\";").extensions(),
        ["fileinto"]
    );
}

/// Каждое расширение без `require` — ошибка с позицией (RFC 5228 §3.2).
#[test]
fn require_each_extension() {
    let cases: &[(&str, u32, &str)] = &[
        ("fileinto \"x\";", 1, "\"fileinto\""),
        ("if envelope \"to\" \"a\" {}", 4, "\"envelope\""),
        ("addflag \"\\\\Seen\";", 1, "\"imap4flags\""),
        ("if hasflag \"x\" {}", 4, "\"imap4flags\""),
        ("keep :flags \"x\";", 6, "\"imap4flags\""),
        ("vacation \"x\";", 1, "\"vacation\""),
        ("if body \"x\" {}", 4, "\"body\""),
        (
            "if header :count \"gt\" \"to\" \"1\" {}",
            11,
            "\"relational\"",
        ),
        (
            "if header :value \"gt\" \"to\" \"1\" {}",
            11,
            "\"relational\"",
        ),
        ("if address :user \"to\" \"a\" {}", 12, "\"subaddress\""),
        ("if address :detail \"to\" \"a\" {}", 12, "\"subaddress\""),
        ("redirect :copy \"a@b.example\";", 10, "\"copy\""),
        ("if mailboxexists \"x\" {}", 4, "\"mailbox\""),
        (
            "if header :comparator \"i;ascii-numeric\" \"x\" \"1\" {}",
            11,
            "\"comparator-i;ascii-numeric\"",
        ),
    ];
    for (text, column, ext) in cases {
        assert_error(text, 1, *column, ext);
        assert_error(text, 1, *column, "не объявлено в require");
    }
    assert_error(
        "require \"fileinto\";\nfileinto :create \"x\";",
        2,
        10,
        "\"mailbox\" не объявлено",
    );
}

/// Каждый вид ошибки проверки — с позицией.
#[test]
fn errors_with_position() {
    let cases: &[(&str, u32, u32, &str)] = &[
        (
            "require \"variables\";",
            1,
            9,
            "\"variables\" не поддерживается",
        ),
        (
            "keep;\nrequire \"fileinto\";",
            2,
            1,
            "require допустим только в начале",
        ),
        (
            "if true { require \"fileinto\"; }",
            1,
            11,
            "require допустим только в начале",
        ),
        ("frobnicate;", 1, 1, "неизвестная команда frobnicate"),
        ("if frob {}", 1, 4, "неизвестный тест frob"),
        ("keep :frob;", 1, 6, "неизвестный тег :frob"),
        (
            "if header :is :is \"a\" \"b\" {}",
            1,
            15,
            "тег :is указан дважды",
        ),
        (
            "if header :is :contains \"a\" \"b\" {}",
            1,
            15,
            "несколько типов сравнения",
        ),
        (
            "if header :comparator {}",
            1,
            11,
            "после :comparator ожидается строка",
        ),
        (
            "if header :comparator \"i;frob\" \"a\" \"b\" {}",
            1,
            11,
            "неизвестный компаратор",
        ),
        (
            "if header \"a\" {}",
            1,
            4,
            "ожидается позиционных аргументов: 2, указано: 1",
        ),
        (
            "if header \"a\" \"b\" \"c\" {}",
            1,
            19,
            "ожидается позиционных аргументов: 2, указано: 3",
        ),
        ("if size :over \"10\" {}", 1, 15, "ожидается число"),
        ("if size 10 {}", 1, 4, "ровно один из :over и :under"),
        (
            "if exists \"Bad Name\" {}",
            1,
            11,
            "недопустимое имя заголовка",
        ),
        ("if exists \"\" {}", 1, 11, "недопустимое имя заголовка"),
        ("if true;", 1, 1, "if ожидает блок"),
        ("if { keep; }", 1, 1, "if ожидает один тест"),
        ("if (true) { keep; }", 1, 1, "if ожидает один тест"),
        ("if \"x\" true { keep; }", 1, 4, "if: ожидается тест"),
        ("else { keep; }", 1, 1, "else без предшествующего if"),
        (
            "keep;\nelsif true { keep; }",
            2,
            1,
            "elsif без предшествующего if",
        ),
        (
            "if true {} else {} else {}",
            1,
            20,
            "else без предшествующего if",
        ),
        ("else true { keep; }", 1, 6, "у else нет теста"),
        ("keep { stop; }", 1, 1, "у команды keep не может быть блока"),
        ("keep true;", 1, 6, "у команды keep не может быть теста"),
        (
            "stop 1;",
            1,
            6,
            "ожидается позиционных аргументов: 0, указано: 1",
        ),
        ("if not (true) {}", 1, 4, "not ожидает один тест"),
        ("if anyof true {}", 1, 4, "anyof ожидает список тестов"),
        (
            "if true false {}",
            1,
            9,
            "у теста true не может быть вложенного теста",
        ),
        (
            "if anyof :x (true) {}",
            1,
            10,
            "у теста anyof нет аргументов",
        ),
        (
            "if header \"a\" \"b\" true {}",
            1,
            19,
            "у теста header не может быть вложенного теста",
        ),
        ("redirect \"no-at-sign\";", 1, 10, "некорректный адрес"),
        ("redirect [\"a@b.example\"];", 1, 10, "ожидается строка"),
        (
            "require \"fileinto\"; fileinto \"\";",
            1,
            30,
            "пустое имя папки",
        ),
        (
            "require \"relational\";\nif header :value \"zz\" \"a\" \"b\" {}",
            2,
            11,
            "оператор сравнения",
        ),
        (
            "require \"comparator-i;ascii-numeric\";\nif header :contains :comparator \"i;ascii-numeric\" \"a\" \"1\" {}",
            2,
            21,
            "не поддерживает :contains и :matches",
        ),
        (
            "if address :all :domain \"to\" \"a\" {}",
            1,
            17,
            "несколько частей адреса",
        ),
        (
            "require \"envelope\";\nif envelope \"cc\" \"a\" {}",
            2,
            13,
            "неизвестная часть конверта",
        ),
        (
            "require \"body\";\nif body :raw :text \"a\" {}",
            2,
            14,
            "несколько преобразований",
        ),
        (
            "require \"imap4flags\";\nif hasflag \"var\" \"x\" {}",
            2,
            18,
            "ожидается позиционных аргументов: 1, указано: 2",
        ),
        (
            "require \"vacation\";\nvacation :days \"7\" \"x\";",
            2,
            10,
            "после :days ожидается число",
        ),
        (
            "require \"encoded-character\";\nredirect \"${unicode:110000}@b.example\";",
            2,
            10,
            "недопустимый код Unicode",
        ),
    ];
    for (text, line, column, message) in cases {
        assert_error(text, *line, *column, message);
    }
}

/// Все ошибки скрипта одним списком (ManageSieve отдаёт их в ответе `CHECKSCRIPT`).
#[test]
fn all_diagnostics() {
    let text = "require [\"fileinto\", \"variables\"];
if header :is \"Subject\" {
  fileinto \"x\";
}
redirect \"not-an-address\";
fileinto :copy \"Archive\";
elsif true { keep; }
if size :over :under 2M { stop; }
frobnicate;
";
    let diags = compile(text.as_bytes(), &Limits::default()).expect_err("ошибки");
    let text = diags
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("all_diagnostics", text);
}

#[test]
fn compiled_tests() {
    let p = ok(
        r#"require ["relational", "comparator-i;ascii-numeric", "subaddress"];
if allof (header :count "ge" :comparator "i;ascii-numeric" ["To", "Cc"] "3",
          address :detail :matches :comparator "i;octet" "to" "news*",
          not size :under 1K) { keep; }
"#,
    );
    let Cmd::If {
        branches,
        otherwise,
    } = &p.commands[0]
    else {
        panic!("{:?}", p.commands);
    };
    assert!(otherwise.is_none());
    let Test::AllOf(tests) = &branches[0].0 else {
        panic!("{branches:?}");
    };
    let Test::Header { m, names, keys } = &tests[0] else {
        panic!("{tests:?}");
    };
    assert_eq!(m.kind, MatchKind::Count(Rel::Ge));
    assert_eq!(m.cmp, Comparator::AsciiNumeric);
    assert_eq!(names, &["To", "Cc"]);
    assert_eq!(keys, &["3"]);
    let Test::Address { m, part, .. } = &tests[1] else {
        panic!("{tests:?}");
    };
    assert_eq!(
        (m.kind, m.cmp, *part),
        (MatchKind::Matches, Comparator::Octet, Part::Detail)
    );
    assert_eq!(
        tests[2],
        Test::Not(Box::new(Test::Size {
            over: false,
            limit: 1024
        }))
    );
    // Тип сравнения и компаратор по умолчанию — :is и i;ascii-casemap.
    let p = ok("if header \"a\" \"b\" {}");
    let Cmd::If { branches, .. } = &p.commands[0] else {
        panic!("{:?}", p.commands);
    };
    assert!(matches!(
        &branches[0].0,
        Test::Header { m, .. } if m.kind == MatchKind::Is && m.cmp == Comparator::AsciiCasemap
    ));
}

#[test]
fn encoded_character() {
    assert_eq!(decode_encoded("${hex:41 42}c").as_deref(), Ok("ABc"));
    assert_eq!(decode_encoded("${HEX: d0 b0 }").as_deref(), Ok("а"));
    assert_eq!(
        decode_encoded("${unicode:1F600 0041}").as_deref(),
        Ok("😀A")
    );
    assert_eq!(decode_encoded("${unicode:0}").as_deref(), Ok("\0"));
    // Не по грамматике — остаётся как есть (RFC 5228 §2.4.2.4).
    for s in [
        "${hex:}",
        "${hex:4G}",
        "${hex:414}",
        "${frob:41}",
        "${hex:41",
        "$${hex:41}",
    ] {
        let expected = if s == "$${hex:41}" { "$A" } else { s };
        assert_eq!(decode_encoded(s).as_deref(), Ok(expected), "{s}");
    }
    assert!(decode_encoded("${hex:ff}").is_err());
    assert!(decode_encoded("${unicode:D800}").is_err());
    assert!(decode_encoded("${unicode:1234567}").is_err());
    // Без require строки не декодируются.
    let p = ok("redirect \"${hex:61}@b.example\";");
    assert_eq!(
        p.commands[0],
        Cmd::Redirect {
            address: "${hex:61}@b.example".into(),
            copy: false
        }
    );
    let p = ok("require \"encoded-character\"; redirect \"${hex:61}@b.example\";");
    assert_eq!(
        p.commands[0],
        Cmd::Redirect {
            address: "a@b.example".into(),
            copy: false
        }
    );
}
