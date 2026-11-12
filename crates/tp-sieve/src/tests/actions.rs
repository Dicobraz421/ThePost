//! Действия и их слияние в `RulesOutcome` (SIEVE-04): `fileinto` (`:copy`, `:create`), `imap4flags`, `redirect`,
//! `mailboxexists`, повторы, `discard`.

use tp_contracts::meta::{Flag, Mailbox};
use tp_contracts::sieve::{MailboxRef, RulesOutcome};
use tp_testkit::corpus::corpus_mail;

use super::mailboxes;
use crate::outcome::{mailbox_paths, merge};
use crate::{Context, Limits, check, run};

fn outcome(script: &str, list: &[Mailbox], limits: &Limits) -> RulesOutcome {
    let program = match check(script) {
        Ok(p) => p,
        Err(d) => panic!("{script}: {d:?}"),
    };
    let raw = corpus_mail("plain-ascii").expect("mail").raw;
    let paths = mailbox_paths(list);
    let ctx = Context {
        message: &raw,
        envelope_from: "alice@example.com",
        envelope_to: "bob@example.org",
        mailboxes: &paths,
    };
    merge(run(&program, &ctx, limits), list)
}

/// Кратко: папки с флагами, перенаправления, `discard`, число ошибок.
fn summary(o: &RulesOutcome, list: &[Mailbox]) -> String {
    let mut v: Vec<String> = o
        .keep
        .iter()
        .map(|k| {
            let name = match &k.mailbox {
                MailboxRef::Inbox => "INBOX".to_owned(),
                MailboxRef::Id(id) => {
                    let i = list.iter().position(|m| m.id == *id).expect("папка");
                    mailbox_paths(list)[i].clone()
                }
                MailboxRef::Name { name, create } => format!("new:{name}:{create}"),
            };
            let flags: Vec<String> = k
                .flags
                .iter()
                .map(|f| match f {
                    Flag::Keyword(k) => k.clone(),
                    other => format!("{other:?}"),
                })
                .collect();
            if flags.is_empty() {
                name
            } else {
                format!("{name}[{}]", flags.join(" "))
            }
        })
        .collect();
    v.extend(o.redirect.iter().map(|r| format!("->{r}")));
    if o.discard {
        v.push("discard".to_owned());
    }
    if !o.errors.is_empty() {
        v.push(format!("errors:{}", o.errors.len()));
    }
    v.join(", ")
}

const REQ: &str =
    "require [\"fileinto\", \"copy\", \"mailbox\", \"imap4flags\"];\n";

#[test]
fn actions_table() {
    let list = mailboxes();
    let cases: &[(&str, &str)] = &[
        ("", "INBOX"),
        ("keep;", "INBOX"),
        ("discard;", "discard"),
        // discard отменяет только implicit keep (RFC 5228 §2.10.1): явные действия остаются.
        ("discard; keep;", "INBOX"),
        ("fileinto \"Work\"; discard;", "Work"),
        ("fileinto \"Work\";", "Work"),
        ("fileinto \"Work/Reports\";", "Work/Reports"),
        ("fileinto \"Архив\";", "Архив"),
        ("fileinto :copy \"Work\";", "Work, INBOX"),
        // Имя папки — как в IMAP: INBOX без учёта регистра, остальные — побайтно.
        ("fileinto \"inbox\";", "INBOX"),
        ("fileinto \"work\";", "INBOX, errors:1"),
        ("fileinto \"/Work//Reports/\";", "Work/Reports"),
        // Повтор папки — одна копия, флаги объединяются.
        ("fileinto \"Work\"; fileinto \"Work\";", "Work"),
        (
            "fileinto :flags \"\\\\Seen\" \"Work\"; fileinto :flags \"$label\" \"Work\"; keep; keep;",
            "Work[Seen $label], INBOX",
        ),
        // Папки нет: без :create — implicit keep и диагностика, с :create — создать.
        ("fileinto \"Nope\";", "INBOX, errors:1"),
        ("fileinto :create \"Nope/Sub\";", "new:Nope/Sub:true"),
        ("fileinto :create \"Work\";", "Work"),
        (
            "fileinto :create \"New\"; fileinto :create \"New\";",
            "new:New:true",
        ),
        (
            "if mailboxexists \"Work/Reports\" { fileinto \"Work/Reports\"; }",
            "Work/Reports",
        ),
        (
            "if mailboxexists [\"Work\", \"Nope\"] { discard; }",
            "INBOX",
        ),
        // imap4flags: переменная, :flags, implicit keep с флагами.
        (
            "setflag \"\\\\Flagged\"; addflag [\"$work todo\", \"\\\\seen\"]; removeflag \"TODO\";",
            "INBOX[Seen Flagged $work]",
        ),
        (
            "addflag \"\\\\Seen\"; if hasflag :contains \"seen\" { fileinto \"Work\"; } keep :flags \"\\\\Answered\";",
            "Work[Seen], INBOX[Answered]",
        ),
        // Неизвестный системный флаг отбрасывается.
        ("keep :flags [\"\\\\Recent\", \"ok\"];", "INBOX[ok]"),
        // redirect: отменяет implicit keep, :copy — нет; повтор адреса — одно перенаправление.
        ("redirect \"a@example.net\";", "->a@example.net"),
        ("redirect :copy \"a@example.net\";", "INBOX, ->a@example.net"),
        (
            "redirect \"a@example.net\"; redirect \"A@EXAMPLE.net\"; fileinto \"Work\";",
            "Work, ->a@example.net",
        ),
        ("redirect \"a@example.net\"; discard;", "->a@example.net, discard"),
    ];
    for (script, expected) in cases {
        let o = outcome(&format!("{REQ}{script}"), &list, &Limits::default());
        assert_eq!(summary(&o, &list), *expected, "{script}");
    }
}

#[test]
fn missing_mailbox_diagnostic() {
    let list = mailboxes();
    let o = outcome(
        "require \"fileinto\"; fileinto \"Nope\";",
        &list,
        &Limits::default(),
    );
    assert_eq!(o, {
        let mut x = RulesOutcome::implicit_keep();
        x.errors = o.errors.clone();
        x
    });
    assert!(o.errors[0].contains("fileinto") && o.errors[0].contains("Nope"));
}

/// Лимит перенаправлений: превышение — ошибка выполнения, действия отменены, письмо во «Входящих».
#[test]
fn redirect_limit() {
    let list = mailboxes();
    let limits = Limits {
        max_redirects: 2,
        ..Limits::default()
    };
    let two = "redirect \"a@example.net\"; redirect \"b@example.net\";";
    let o = outcome(two, &list, &limits);
    assert_eq!(summary(&o, &list), "->a@example.net, ->b@example.net");
    let o = outcome(&format!("{two} redirect \"c@example.net\";"), &list, &limits);
    assert_eq!(summary(&o, &list), "INBOX, errors:1");
    assert!(o.errors[0].contains("перенаправлений"), "{o:?}");
}

/// Папка «Входящие» под своим именем и без списка папок — всегда `MailboxRef::Inbox`.
#[test]
fn inbox_without_list() {
    let o = outcome(
        "require \"fileinto\"; fileinto \"INBOX\"; keep;",
        &[],
        &Limits::default(),
    );
    assert_eq!(o, RulesOutcome::implicit_keep());
}
