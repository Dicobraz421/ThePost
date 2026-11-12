//! Тесты расширений (SIEVE-05) на корпусе testkit: `body` (RFC 5173) по многочастным письмам и кодировкам,
//! `envelope` и `subaddress` (RFC 5233) по конверту с подадресом, `relational` (RFC 5231).

use tp_testkit::corpus::corpus_mail;

use super::execution::{exec_raw, on_mail, on_text, summary};
use crate::Limits;

const REQ: &str = "require [\"body\", \"envelope\", \"subaddress\", \"relational\", \"comparator-i;ascii-numeric\"];\n";

/// `discard`, если условие истинно.
fn hit(mail: &str, cond: &str) -> bool {
    let script = format!("{REQ}if {cond} {{ discard; }}");
    match on_mail(&script, mail).as_str() {
        "discard" => true,
        "implicit-keep" => false,
        other => panic!("{mail}: {cond}: {other}"),
    }
}

#[test]
fn body_table() {
    let cases: &[(&str, &str, bool)] = &[
        // По умолчанию — :text, сравнение без учёта регистра ASCII.
        ("plain-ascii", r#"body :contains "QUICK BROWN FOX""#, true),
        ("plain-ascii", r#"body :comparator "i;octet" :contains "QUICK""#, false),
        ("plain-ascii", r#"body :contains "Subject""#, false),
        ("plain-empty-body", r#"body :contains "a""#, false),
        ("headers-only", r#"body :raw :contains "a""#, false),
        // Кодировки: текст декодируется из charset и transfer-encoding.
        ("plain-utf8-qp", r#"body :text :contains "тестовое письмо""#, true),
        ("plain-utf8-base64", r#"body :text :contains "тестовое письмо""#, true),
        ("plain-koi8r-8bit", r#"body :contains "мягких французских булок""#, true),
        ("plain-koi8r-qp", r#"body :contains "мягких французских булок""#, true),
        ("plain-cp1251-8bit", r#"body :contains "проверки разбора""#, true),
        ("plain-cp1251-base64", r#"body :contains "проверки разбора""#, true),
        ("plain-cp1251-base64", r#"body :matches "Привет! * чаю.*""#, true),
        // :raw — тело как есть, без декодирования.
        ("plain-cp1251-base64", r#"body :raw :contains "z/Do4uXyISDd""#, true),
        ("plain-cp1251-base64", r#"body :raw :contains "Привет""#, false),
        ("plain-utf8-qp", r#"body :raw :contains "=D0=""#, true),
        ("plain-utf8-qp", r#"body :text :contains "=D0=""#, false),
        ("alternative", r#"body :raw :contains "--alt-1""#, true),
        ("alternative", r#"body :raw :contains "Message-ID""#, false),
        // HTML: :text — без разметки, :content "text/html" — с разметкой.
        ("html-only", r#"body :text :contains "Hello world""#, true),
        ("html-only", r#"body :text :contains "<b>""#, false),
        ("html-only", r#"body :content "text/html" :contains "<b>world</b>""#, true),
        ("html-only", r#"body :content "text/plain" :contains "world""#, false),
        ("html-only", r#"body :text :contains "Съешь же ещё""#, true),
        // Многочастные письма.
        ("alternative", r#"body :contains "<p>""#, false),
        ("alternative", r#"body :content "text" :contains "<p>The quick""#, true),
        ("alternative", r#"body :content "text/plain" :contains "<p>""#, false),
        ("alternative-cyrillic", r#"body :content "text/html" :contains "<p>Привет""#, true),
        ("alternative-cyrillic", r#"body :contains "выпей чаю""#, true),
        ("mixed-pdf", r#"body :contains "Quarterly report attached""#, true),
        // Вложения: :text их не читает, :content — декодированное содержимое.
        ("mixed-pdf", r#"body :contains "%PDF""#, false),
        ("mixed-pdf", r#"body :content "application/pdf" :contains "%PDF-1.4""#, true),
        ("mixed-pdf", r#"body :content "application" :contains "/Type /Catalog""#, true),
        ("mixed-pdf", r#"body :content "" :contains "%PDF-1.4""#, true),
        ("mixed-pdf", r#"body :content "image" :contains "PDF""#, false),
        ("mixed-pdf", r#"body :content ["image", "text"] :contains "fox""#, true),
        ("mixed-pdf", r#"body :content "text/" :contains "fox""#, false),
        // Вложенное письмо: текст — как у внешнего, message/rfc822 — его заголовки.
        ("rfc822-attached", r#"body :contains "See attached message""#, true),
        ("rfc822-attached", r#"body :contains "мягких французских""#, true),
        ("rfc822-attached", r#"body :content "message/rfc822" :contains "corpus-033@example.com""#, true),
        ("rfc822-attached", r#"body :content "text" :contains "corpus-033@example.com""#, false),
        ("rfc822-base64", r#"body :contains "мягких французских""#, true),
        ("rfc822-base64", r#"body :content "message" :contains "corpus-033@example.com""#, true),
        // relational по значениям тела.
        ("alternative", r#"body :count "eq" :comparator "i;ascii-numeric" "2""#, true),
        ("alternative", r#"body :content "text/plain" :count "eq" :comparator "i;ascii-numeric" "1""#, true),
        ("headers-only", r#"body :count "eq" :comparator "i;ascii-numeric" "0""#, false),
    ];
    for (mail, cond, expected) in cases {
        assert_eq!(hit(mail, cond), *expected, "{mail}: {cond}");
    }
}

/// Сломанные письма корпуса не дают ошибки выполнения.
#[test]
fn body_on_whole_corpus() {
    let script = format!(
        "{REQ}if anyof (body :contains \"zzz-нет\", body :raw :matches \"*zzz-нет*\", body :content \"\" :contains \"zzz-нет\") {{ discard; }}"
    );
    for mail in tp_testkit::corpus().expect("corpus") {
        let o = exec_raw(&script, &mail.raw, &Limits::default());
        assert_eq!(summary(&o), "implicit-keep", "{}", mail.name);
    }
}

/// Тест `body` читает не больше `max_body_scan` байт.
#[test]
fn body_scan_limit() {
    let raw = corpus_mail("plain-ascii").expect("mail").raw;
    let script = format!("{REQ}if body :contains \"lazy dog\" {{ discard; }}");
    let small = Limits {
        max_body_scan: 16,
        ..Limits::default()
    };
    assert_eq!(summary(&exec_raw(&script, &raw, &Limits::default())), "discard");
    assert_eq!(summary(&exec_raw(&script, &raw, &small)), "implicit-keep");
    let raw_script = format!("{REQ}if body :raw :contains \"lazy dog\" {{ discard; }}");
    assert_eq!(summary(&exec_raw(&raw_script, &raw, &small)), "implicit-keep");
    // Граница лимита не рвёт символ UTF-8.
    let mail = "Subject: t\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nяяяя";
    let odd = Limits {
        max_body_scan: 3,
        ..Limits::default()
    };
    let s = format!("{REQ}if body :is \"я\" {{ discard; }}");
    assert_eq!(summary(&exec_raw(&s, mail.as_bytes(), &odd)), "discard");
}

/// Конверт теста: `MAIL FROM:<alice@example.com>`, `RCPT TO:<bob+lists@example.org>`.
#[test]
fn envelope_subaddress_table() {
    let cases: &[(&str, bool)] = &[
        (r#"envelope "from" "ALICE@example.com""#, true),
        (r#"envelope :is "to" "bob+lists@example.org""#, true),
        (r#"envelope "to" "bob@example.org""#, false),
        (r#"envelope :localpart "to" "bob+lists""#, true),
        (r#"envelope :domain ["from", "to"] "example.org""#, true),
        (r#"envelope :domain "from" "example.org""#, false),
        (r#"envelope :user "to" "bob""#, true),
        (r#"envelope :detail "to" "lists""#, true),
        (r#"envelope :detail :matches "to" "l*s""#, true),
        (r#"envelope :detail "to" "bob""#, false),
        // У отправителя подадреса нет: :detail не даёт значения, :user — вся локальная часть.
        (r#"envelope :detail :matches "from" "*""#, false),
        (r#"envelope :user "from" "alice""#, true),
        (r#"envelope :count "eq" :comparator "i;ascii-numeric" ["from", "to"] "2""#, true),
        (r#"envelope :detail :count "eq" :comparator "i;ascii-numeric" ["from", "to"] "1""#, true),
        // Заголовки: подадрес в To письма.
        (r#"address :user "to" "bob""#, true),
        (r#"address :detail "to" "x""#, false),
    ];
    for (cond, expected) in cases {
        assert_eq!(hit("plain-ascii", cond), *expected, "{cond}");
    }
    let mail = "From: a@x.example\r\nTo: Team <team+a+b@y.example>, plain@y.example\r\n\r\nx";
    let script = |cond: &str| format!("{REQ}if {cond} {{ discard; }}");
    for (cond, expected) in [
        // Разделитель — первый «+», как в доставке SMTP.
        (r#"address :user "to" "team""#, "discard"),
        (r#"address :detail "to" "a+b""#, "discard"),
        (r#"address :detail :count "eq" :comparator "i;ascii-numeric" "to" "1""#, "discard"),
        (r#"address :user :count "eq" :comparator "i;ascii-numeric" "to" "2""#, "discard"),
        (r#"address :detail "to" """#, "implicit-keep"),
    ] {
        assert_eq!(on_text(&script(cond), mail), expected, "{cond}");
    }
}

#[test]
fn relational_table() {
    let mail = "From: a@x.example\r\nTo: b@y.example, c@y.example\r\nCc: d@y.example\r\nReceived: one\r\n\
                Received: two\r\nX-Spam-Score: 7\r\nX-Priority: 010\r\nSubject: beta\r\n\r\nx";
    let cases: &[(&str, bool)] = &[
        (r#"header :count "eq" :comparator "i;ascii-numeric" "received" "2""#, true),
        (r#"header :count "gt" :comparator "i;ascii-numeric" "received" "1""#, true),
        (r#"header :count "lt" :comparator "i;ascii-numeric" "received" "2""#, false),
        (r#"header :count "eq" :comparator "i;ascii-numeric" "x-none" "0""#, true),
        (r#"address :count "ge" :comparator "i;ascii-numeric" ["to", "cc"] "3""#, true),
        (r#"address :count "ne" :comparator "i;ascii-numeric" ["to", "cc"] "3""#, false),
        (r#"header :value "ge" :comparator "i;ascii-numeric" "x-spam-score" "5""#, true),
        (r#"header :value "lt" :comparator "i;ascii-numeric" "x-spam-score" "10""#, true),
        (r#"header :value "gt" :comparator "i;ascii-numeric" "x-spam-score" "7""#, false),
        (r#"header :value "le" :comparator "i;ascii-numeric" "x-spam-score" "7""#, true),
        // Ведущие нули не влияют на число; строка без цифр — «бесконечность».
        (r#"header :value "eq" :comparator "i;ascii-numeric" "x-priority" "10""#, true),
        (r#"header :value "gt" :comparator "i;ascii-numeric" "subject" "999999""#, true),
        // Без числового компаратора — порядок строк.
        (r#"header :value "gt" "subject" "alpha""#, true),
        (r#"header :value "lt" "subject" "BETA2""#, true),
        (r#"header :value "ne" :comparator "i;octet" "subject" "Beta""#, true),
    ];
    for (cond, expected) in cases {
        let script = format!("{REQ}if {cond} {{ discard; }}");
        let got = on_text(&script, mail) == "discard";
        assert_eq!(got, *expected, "{cond}");
    }
}
