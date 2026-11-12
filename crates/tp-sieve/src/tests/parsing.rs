//! Лексер и грамматика (SIEVE-01): примеры RFC 5228, 5229, 5230, 5232, 5233, ошибки с позицией, лимиты, proptest.

use proptest::prelude::*;

use super::assert_error;
use crate::{Arg, Limits, Pos, Script, parse};

fn parsed(text: &str) -> Script {
    match parse(text.as_bytes(), &Limits::default()) {
        Ok(s) => s,
        Err(d) => panic!("{text:?}: {d}"),
    }
}

fn first_arg(text: &str) -> Arg {
    parsed(text).commands[0].args[0].clone()
}

fn strings(a: &Arg) -> Vec<String> {
    match a {
        Arg::Strings { values, .. } => values.clone(),
        other => panic!("не строки: {other:?}"),
    }
}

/// Пример RFC 5228 §9 (комментарии свои): регистр идентификаторов и тегов не важен.
const RFC5228_EXAMPLE: &str = r#"#
# Пример фильтра: объявить расширения
#
require ["fileinto"];

# Рассылка рабочей группы
if header :is "Sender" "owner-ietf-mta-filters@imc.org"
        {
        fileinto "filter";  # в папку "filter"
        }
/* Свои — по домену */
elsif address :DOMAIN :is ["From", "To"] "example.com"
        {
        keep;
        }
elsif anyof (NOT address :all :contains
               ["To", "Cc", "Bcc"] "me@example.com",
             header :matches "subject"
               ["*make*money*fast*", "*university*dipl*mas*"])
        {
        fileinto "spam";
        }
else
        {
        fileinto "personal";
        }
"#;

#[test]
fn rfc5228_example() {
    let script = parsed(RFC5228_EXAMPLE);
    assert_eq!(script.commands.len(), 5);
    assert_eq!(script.commands[1].pos, Pos { line: 7, column: 1 });
    insta::assert_snapshot!("rfc5228_example", script.to_string());
}

/// RFC 5230: `vacation` с многострочным текстом; удвоенная точка снимается.
#[test]
fn rfc5230_vacation() {
    let text = "require [\"vacation\", \"fileinto\"];\n\
                if header :contains \"subject\" \"отпуск\" {\n\
                \x20   vacation :days 7 :subject \"Я в отпуске\" :from \"anna@example.org\"\n\
                \x20       :addresses [\"anna@example.org\", \"a.ivanova@example.org\"] :handle \"summer\"\n\
                \x20       text:\n\
                Здравствуйте!\n\
                ..Точка в начале строки удвоена.\n\
                Вернусь 20 июля.\n\
                .\n\
                ;\n\
                }\n";
    insta::assert_snapshot!("rfc5230_vacation", parsed(text).to_string());
}

/// RFC 5229 (variables): грамматика разбирается, хотя расширение не поддерживается проверкой.
#[test]
fn rfc5229_grammar() {
    let text = r#"require ["variables", "fileinto"];
set "honorific" "Mr";
set :lower :upperfirst "first_name" "JOE";
if header :matches "List-ID" "*<*@*" {
    fileinto "INBOX.lists.${2}"; stop;
}
if string :is "${honorific}" "Mr" { keep; }
"#;
    let script = parsed(text);
    assert_eq!(script.commands.len(), 5);
    assert_error(text, 1, 9, "\"variables\" не поддерживается");
}

/// RFC 5232 (imap4flags) и RFC 5233 (subaddress): разбор и проверка.
#[test]
fn rfc5232_rfc5233() {
    let flags = r#"require ["imap4flags", "fileinto"];
if header :contains "from" "boss@frobnitzm.example.edu" {
    setflag "\\Flagged";
    fileinto "INBOX.From Boss";
}
if header :contains "subject" "urgent" { addflag ["\\Answered", "$MDNSent"]; }
if hasflag :contains "Flagged" { removeflag "\\Flagged"; }
keep :flags "\\Seen \\Deleted";
"#;
    let sub = r#"require ["envelope", "subaddress", "fileinto"];
if envelope :user "to" "postmaster" { fileinto "inbox.postmaster"; stop; }
if envelope :detail :is "to" "mta-filters" { fileinto "inbox.ietf-mta-filters"; stop; }
if address :detail "to" "" { fileinto "inbox.empty-detail"; stop; }
if address :user :matches "to" "*-request" { discard; }
"#;
    for text in [flags, sub] {
        assert!(crate::check(text).is_ok(), "{text}");
    }
    let script = parsed(flags);
    assert_eq!(
        strings(&script.commands[1].block.as_ref().expect("блок")[0].args[0]),
        ["\\Flagged"]
    );
}

#[test]
fn numbers() {
    let n = |text: &str| match first_arg(text) {
        Arg::Number { value, .. } => value,
        other => panic!("{other:?}"),
    };
    assert_eq!(n("x 0;"), 0);
    assert_eq!(n("x 100;"), 100);
    assert_eq!(n("x 1K;"), 1024);
    assert_eq!(n("x 2m;"), 2 << 20);
    assert_eq!(n("x 3G;"), 3 << 30);
    assert_error("x 99999999999999999999;", 1, 3, "слишком большое число");
    assert_error("keep;\nx 17179869184G;", 2, 3, "слишком большое число");
}

#[test]
fn quoted_and_multiline() {
    assert_eq!(strings(&first_arg(r#"x "a\"b\\c\d";"#)), ["a\"b\\cd"]);
    assert_eq!(strings(&first_arg("x \"две\nстроки\";")), ["две\nстроки"]);
    assert_eq!(
        strings(&first_arg("x text:\r\nline1\r\n..dot\r\n.\r\n;")),
        ["line1\r\n.dot\r\n"]
    );
    assert_eq!(
        strings(&first_arg("x text: # комментарий\nx\n.\n;")),
        ["x\n"]
    );
    assert_eq!(strings(&first_arg("x text:\n.\n;")), [""]);
    // `text` без двоеточия — обычный идентификатор, `:text` — тег.
    assert!(matches!(first_arg("x :text;"), Arg::Tag { ref name, .. } if name == "text"));
    assert_eq!(parsed("text;").commands[0].name, "text");
}

#[test]
fn comments_and_case() {
    let script = parsed("/* много\nстрок */ KEEP; # хвост\nIf TRUE { Stop; }\n");
    assert_eq!(script.commands[0].name, "keep");
    assert_eq!(script.commands[1].name, "if");
    assert_eq!(script.commands[1].tests[0].name, "true");
    assert_eq!(script.commands[1].pos, Pos { line: 3, column: 1 });
    assert_eq!(parsed("keep;\n  # только комментарий").commands.len(), 1);
    assert!(parsed("").commands.is_empty());
}

#[test]
fn string_lists_and_tests() {
    let script = parsed("if anyof (true, not false, allof (true)) { keep; }");
    let anyof = &script.commands[0].tests[0];
    assert!(anyof.test_list);
    assert_eq!(anyof.tests.len(), 3);
    assert_eq!(anyof.tests[1].tests[0].name, "false");
    assert!(!anyof.tests[1].test_list);
    let a = first_arg(r#"x ["a", "b" , "c"];"#);
    assert_eq!(strings(&a), ["a", "b", "c"]);
    assert!(matches!(a, Arg::Strings { list: true, .. }));
    assert!(matches!(
        first_arg(r#"x "a";"#),
        Arg::Strings { list: false, .. }
    ));
}

/// Синтаксические ошибки — с позицией (строка, столбец в символах).
#[test]
fn syntax_errors() {
    assert_error("keep;\n  \"abc", 2, 3, "строка не закрыта");
    assert_error("keep;\n/* x", 2, 1, "комментарий «/*» не закрыт");
    assert_error(
        "keep;\nif true { stop; }\n  @",
        3,
        3,
        "неожиданный символ '@'",
    );
    assert_error("keep", 1, 5, "ожидается «;» или «{»");
    assert_error("if true { stop; ", 1, 17, "ожидается «}»");
    assert_error("keep; }", 1, 7, "лишняя «}»");
    assert_error("fileinto :;", 1, 10, "после «:» ожидается имя тега");
    assert_error("keep [\"a\" \"b\"];", 1, 11, "ожидается «,» или «]»");
    assert_error("keep [];", 1, 7, "ожидается строка");
    assert_error("if anyof (true; false) {}", 1, 15, "ожидается «,» или «)»");
    assert_error("if anyof () {}", 1, 11, "ожидается тест");
    assert_error(
        "x text:x\n.\n;",
        1,
        3,
        "после «text:» ожидается перевод строки",
    );
    assert_error("x text:\nabc\n", 1, 3, "не закрыта строкой «.»");
    assert_error("ёлка;", 1, 1, "неожиданный символ 'ё'");
    assert_error("keep;\nkeep 5 / 2;", 2, 8, "неожиданный символ '/'");
    assert_error("\"строка\";", 1, 1, "ожидается команда");
    let bad = parse(b"keep;\n\xff;", &Limits::default()).expect_err("не UTF-8");
    assert_eq!((bad.line, bad.column), (2, 1));
    assert!(bad.message.contains("UTF-8"), "{bad}");
}

#[test]
fn limits() {
    let small = Limits {
        max_script_size: 10,
        ..Limits::default()
    };
    let e = parse(b"keep; keep; keep;", &small).expect_err("размер");
    assert_eq!((e.line, e.column), (1, 1));

    // Вложенность блоков и тестов — ошибка, а не переполнение стека (в том числе на очень глубоком вводе).
    let big = Limits {
        max_script_size: 1 << 30,
        ..Limits::default()
    };
    let deep_blocks = format!("{}{}", "if true {".repeat(100_000), "}".repeat(100_000));
    let e = parse(deep_blocks.as_bytes(), &big).expect_err("блоки");
    assert!(e.message.contains("вложенность"), "{e}");
    // Тест 33-го `if` — на глубине 33: «true» в столбце 9·32 + 4.
    assert_eq!((e.line, e.column), (1, 292));
    let deep_tests = format!("if {}true {{}}", "not ".repeat(100_000));
    let e = parse(deep_tests.as_bytes(), &big).expect_err("тесты");
    assert!(e.message.contains("вложенность"), "{e}");
    let deep_lists = format!("if {}true{} {{}}", "anyof(".repeat(1000), ")".repeat(1000));
    assert!(parse(deep_lists.as_bytes(), &Limits::default()).is_err());
    let ok_nesting = format!("{}{}", "if true {".repeat(10), "}".repeat(10));
    assert!(parse(ok_nesting.as_bytes(), &Limits::default()).is_ok());

    let long_list = format!("require [{}];", ["\"x\""; 1025].join(","));
    let e = parse(long_list.as_bytes(), &Limits::default()).expect_err("список");
    assert!(e.message.contains("больше 1024 строк"), "{e}");
    let many_tests = format!("if anyof({}) {{}}", ["true"; 1025].join(","));
    let e = parse(many_tests.as_bytes(), &Limits::default()).expect_err("тесты");
    assert!(e.message.contains("больше 1024 тестов"), "{e}");

    let many = "keep;".repeat(4097);
    let e = parse(many.as_bytes(), &Limits::default()).expect_err("команды");
    assert!(e.message.contains("больше 4096 команд"), "{e}");
    assert!(parse("keep;".repeat(4096).as_bytes(), &Limits::default()).is_ok());
}

/// Канонический вывод разбирается в то же дерево (без позиций).
#[test]
fn display_round_trip() {
    let strip = |s: &Script| s.to_string();
    let first = parsed(RFC5228_EXAMPLE);
    let again = parsed(&first.to_string());
    assert_eq!(strip(&first), strip(&again));
}

const TOKENS: &[&str] = &[
    "require",
    "\"fileinto\"",
    "[\"envelope\", \"relational\", \"comparator-i;ascii-numeric\", \"encoded-character\"]",
    "[",
    "]",
    "(",
    ")",
    "{",
    "}",
    ",",
    ";",
    "if",
    "elsif",
    "else",
    "header",
    "address",
    "envelope",
    "exists",
    "size",
    "anyof",
    "allof",
    "not",
    "true",
    "false",
    ":is",
    ":contains",
    ":matches",
    ":value",
    ":count",
    "\"gt\"",
    ":comparator",
    "\"i;ascii-numeric\"",
    ":domain",
    ":localpart",
    ":over",
    ":under",
    "1K",
    "7",
    "\"subject\"",
    "\"From\"",
    "\"*a?b\\\\*\"",
    "\"${hex:41}${unicode:1F600}\"",
    "keep",
    "discard",
    "stop",
    "fileinto",
    "redirect",
    "\"a@b.example\"",
    "vacation",
    ":days",
    "text:\nстрока\n..\n.\n",
    "# комментарий\n",
    "/* c */",
    "\"",
    "/*",
    "\\",
    "ё",
];

proptest! {
    /// Произвольные байты: разбор и проверка не паникуют.
    #[test]
    fn arbitrary_bytes(data in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let _ = parse(&data, &Limits::default());
        let _ = crate::compile(&data, &Limits::default());
    }

    /// Смесь токенов Sieve: доходит до проверки и выполнения, ничего не паникует.
    #[test]
    fn token_soup(parts in proptest::collection::vec(proptest::sample::select(TOKENS), 0..120)) {
        let text = parts.join(" ");
        if let Ok(program) = crate::compile(text.as_bytes(), &Limits::default()) {
            let ctx = crate::Context {
                message: b"From: a@b.example\r\nSubject: ab\r\n\r\nbody",
                envelope_from: "a@b.example",
                envelope_to: "c+d@e.example",
                mailboxes: &[],
            };
            let _ = crate::run(&program, &ctx, &Limits::default());
        }
    }

    /// Строки и числа в произвольной записи: разбор не паникует, позиции в пределах текста.
    #[test]
    fn arbitrary_text(text in "\\PC{0,256}") {
        if let Err(d) = parse(text.as_bytes(), &Limits::default()) {
            prop_assert!(d.line >= 1 && d.column >= 1);
            prop_assert!(d.line as usize <= text.lines().count() + 1);
        }
    }
}
