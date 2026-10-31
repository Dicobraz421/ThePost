//! Примеры RFC 9051 и расширений: разбор команд, литералы, сериализация ответов.

use proptest::prelude::*;

use super::types::command::CommandBody;
use super::types::fetch::{MacroOrMessageDataItemNames, MessageDataItemName, Section};
use super::types::flag::FlagNameAttribute;
use super::types::response::StatusKind;
use super::*;

fn read_all(reader: &mut Reader, expect: Expect) -> Vec<Input> {
    let mut out = Vec::new();
    while let Some(i) = reader.next(expect) {
        out.push(i);
    }
    out
}

fn one(input: &str) -> Input {
    let mut r = Reader::new(1024);
    r.push(input.as_bytes());
    let mut all = read_all(&mut r, Expect::Command);
    assert_eq!(all.len(), 1, "{input:?}: {all:?}");
    all.remove(0)
}

fn command(input: &str) -> Command {
    match one(input) {
        Input::Command(c) => c,
        other => panic!("{input:?}: {other:?}"),
    }
}

#[test]
fn rfc9051_commands() {
    let c = command("a001 LOGIN SMITH SESAME\r\n");
    assert_eq!(c.tag.inner(), "a001");
    assert!(matches!(c.body, Body::Std(CommandBody::Login { .. })));

    let c = command("A142 SELECT INBOX\r\n");
    assert!(matches!(c.body, Body::Std(CommandBody::Select { .. })));

    // RFC 9051, 6.4.5: частичная выборка и раздел заголовков.
    let c = command("A654 FETCH 2:4 (FLAGS BODY[HEADER.FIELDS (DATE FROM)])\r\n");
    let Body::Std(CommandBody::Fetch {
        macro_or_item_names: MacroOrMessageDataItemNames::MessageDataItemNames(items),
        uid: false,
        ..
    }) = c.body
    else {
        panic!("not fetch: {:?}", c.body)
    };
    assert_eq!(items[0], MessageDataItemName::Flags);
    assert!(matches!(
        &items[1],
        MessageDataItemName::BodyExt {
            section: Some(Section::HeaderFields(None, _)),
            peek: false,
            ..
        }
    ));

    let c = command("A999 UID FETCH 4827313:4828442 FLAGS\r\n");
    assert!(matches!(
        c.body,
        Body::Std(CommandBody::Fetch { uid: true, .. })
    ));

    let c = command("a ENABLE IMAP4rev2\r\n");
    assert!(matches!(c.body, Body::Std(CommandBody::Enable { .. })));

    let c = command("a023 ID (\"name\" \"sodr\" \"version\" \"19.34\")\r\n");
    assert!(matches!(c.body, Body::Std(CommandBody::Id { .. })));

    let c = command("a AUTHENTICATE PLAIN AGZvbwBiYXI=\r\n");
    let Body::Std(CommandBody::Authenticate {
        initial_response: Some(ir),
        ..
    }) = c.body
    else {
        panic!("not authenticate")
    };
    assert_eq!(ir.declassify().as_ref(), b"\0foo\0bar");
}

#[test]
fn list_lsub_create_status_normalized() {
    let c = command("A101 LIST \"\" \"\"\r\n");
    assert_eq!(
        c.body,
        Body::List(ListCommand {
            reference: Vec::new(),
            patterns: vec![Vec::new()],
            ..ListCommand::default()
        })
    );
    let c = command("A202 LIST ~/Mail/ %\r\n");
    let Body::List(l) = c.body else {
        panic!("not list")
    };
    assert_eq!(
        (l.reference, l.patterns),
        (b"~/Mail/".to_vec(), vec![b"%".to_vec()])
    );
    let c = command("A002 LSUB \"#news.\" \"comp.mail.*\"\r\n");
    assert!(matches!(c.body, Body::List(ListCommand { lsub: true, .. })));
    // Расширенный LIST — через rev2.
    let c = command("A01 LIST (SUBSCRIBED) \"\" \"*\" RETURN (CHILDREN)\r\n");
    assert!(matches!(
        c.body,
        Body::List(ListCommand {
            extended: true,
            select_subscribed: true,
            return_children: true,
            ..
        })
    ));
    let c = command("A003 CREATE owatagusiam/\r\n");
    assert_eq!(
        c.body,
        Body::Create {
            mailbox: b"owatagusiam/".to_vec(),
            uses: Vec::new()
        }
    );
    let c = command("A042 STATUS blurdybloop (UIDNEXT MESSAGES)\r\n");
    assert_eq!(
        c.body,
        Body::Status {
            mailbox: b"blurdybloop".to_vec(),
            items: vec![StatusItem::UidNext, StatusItem::Messages]
        }
    );
    let c = command("A043 STATUS inbox (SIZE)\r\n");
    assert_eq!(
        c.body,
        Body::Status {
            mailbox: b"inbox".to_vec(),
            items: vec![StatusItem::Size]
        }
    );
}

#[test]
fn synchronizing_literal() {
    // RFC 9051, 4.3: сервер отвечает «+» и получает данные литерала.
    let mut r = Reader::new(1024);
    r.push(b"A001 LOGIN {11}\r\n");
    let Some(Input::Literal { tag, length, sync }) = r.next(Expect::Command) else {
        panic!("no literal announcement")
    };
    assert_eq!(
        (tag.as_ref().map(|t| t.inner()), length, sync),
        (Some("A001"), 11, true)
    );
    assert_eq!(r.next(Expect::Command), None);
    r.push(b"FRED FOOBAR {7}\r\n");
    assert!(matches!(
        r.next(Expect::Command),
        Some(Input::Literal { length: 7, .. })
    ));
    r.push(b"fat man\r\n");
    let Some(Input::Command(c)) = r.next(Expect::Command) else {
        panic!("no command")
    };
    let Body::Std(CommandBody::Login { username, password }) = c.body else {
        panic!("not login")
    };
    assert_eq!(username.as_ref(), b"FRED FOOBAR");
    assert_eq!(password.declassify().as_ref(), b"fat man");
}

#[test]
fn literal_plus_and_rejection() {
    // RFC 7888: LITERAL+ без ожидания «+».
    let mut r = Reader::new(1024);
    r.push(b"A1 LOGIN {4+}\r\nuser {6+}\r\nsecret\r\nA2 NOOP\r\n");
    let all = read_all(&mut r, Expect::Command);
    assert!(matches!(all[0], Input::Literal { sync: false, .. }));
    assert!(matches!(all[1], Input::Literal { sync: false, .. }));
    assert!(matches!(&all[2], Input::Command(c) if c.tag.inner() == "A1"));
    assert!(matches!(&all[3], Input::Command(c) if c.tag.inner() == "A2"));

    // Отказ в синхронизирующем литерале: следующая строка — новая команда, не данные литерала.
    let mut r = Reader::new(1024);
    r.push(b"A3 APPEND INBOX {999999}\r\n");
    assert!(matches!(
        r.next(Expect::Command),
        Some(Input::Literal {
            length: 999999,
            sync: true,
            ..
        })
    ));
    r.reject_literal();
    r.push(b"A4 NOOP\r\n");
    assert!(matches!(r.next(Expect::Command), Some(Input::Command(c)) if c.tag.inner() == "A4"));
}

#[test]
fn malformed_and_limits() {
    assert!(matches!(
        one("A1 FOO\r\n"),
        Input::Bad { tag: Some(t), .. } if t.inner() == "A1"
    ));
    assert!(matches!(
        one("A1 NOOP\n"),
        Input::Bad {
            reason: "Expected CRLF",
            ..
        }
    ));
    assert!(matches!(one("\r\n"), Input::Bad { tag: None, .. }));
    // Литерал с командой внутри не исполняется: это данные.
    let mut r = Reader::new(1024);
    r.push(b"A1 FOO {9+}\r\nA2 NOOP\r\n\r\nA3 NOOP\r\n");
    let all = read_all(&mut r, Expect::Command);
    assert!(matches!(&all[1], Input::Bad { tag: Some(t), .. } if t.inner() == "A1"));
    assert!(matches!(&all[2], Input::Command(c) if c.tag.inner() == "A3"));
    assert_eq!(all.len(), 3);
    // Сообщение длиннее лимита.
    let mut r = Reader::new(32);
    r.push(format!("A5 LOGIN {{40+}}\r\n{}\r\nA6 NOOP\r\n", "x".repeat(40)).as_bytes());
    let all = read_all(&mut r, Expect::Command);
    assert!(matches!(&all[1], Input::TooLong { tag: Some(t) } if t.inner() == "A5"));
    assert!(matches!(&all[2], Input::Command(c) if c.tag.inner() == "A6"));
}

#[test]
fn authenticate_and_idle_data() {
    let mut r = Reader::new(1024);
    r.push(b"AGZvbwBiYXI=\r\n*\r\n");
    assert_eq!(
        r.next(Expect::AuthData),
        Some(Input::AuthData(Some(b"\0foo\0bar".to_vec())))
    );
    assert_eq!(r.next(Expect::AuthData), Some(Input::AuthData(None)));
    r.push(b"!!!\r\n");
    assert!(matches!(r.next(Expect::AuthData), Some(Input::Bad { .. })));
    r.push(b"DONE\r\n");
    assert_eq!(r.next(Expect::IdleDone), Some(Input::IdleDone));
}

#[test]
fn responses() {
    let tag = Tag::try_from("a1").expect("tag").into_static();
    assert_eq!(
        encode(&tagged(
            &tag,
            StatusKind::No,
            Some(code("AUTHENTICATIONFAILED")),
            "Invalid\r\ncredentials"
        )),
        b"a1 NO [AUTHENTICATIONFAILED] Invalid  credentials\r\n"
    );
    assert_eq!(
        encode(&tagged(&tag, StatusKind::Ok, None, "")),
        b"a1 OK -\r\n"
    );
    // RFC 8438, 3.
    let mailbox = astring(b"INBOX".to_vec()).expect("astring");
    assert_eq!(
        status_line(
            mailbox,
            &[(StatusItem::Messages, 52), (StatusItem::Size, 52_000)]
        ),
        b"* STATUS INBOX (MESSAGES 52 SIZE 52000)\r\n"
    );
    let mailbox = astring("Входящие".as_bytes().to_vec()).expect("astring");
    let line = status_line(mailbox, &[(StatusItem::Unseen, 1)]);
    assert!(
        line.ends_with("Входящие (UNSEEN 1)\r\n".as_bytes()),
        "{line:?}"
    );
    // RFC 5258, 5, пример 4: CHILDINFO.
    let data = Data::List {
        items: vec![FlagNameAttribute::from(
            types::core::Atom::try_from("HasChildren").expect("atom"),
        )],
        delimiter: Some(types::core::QuotedChar::try_from('/').expect("char")),
        mailbox: Mailbox::from(astring(b"Foo".to_vec()).expect("astring")),
    };
    assert_eq!(
        list_line(data, "(\"CHILDINFO\" (\"SUBSCRIBED\"))"),
        b"* LIST (\\HasChildren) \"/\" Foo (\"CHILDINFO\" (\"SUBSCRIBED\"))\r\n"
    );
}

#[test]
fn mailbox_names() {
    assert_eq!(
        mailbox_bytes(&Mailbox::from(astring(b"inbox".to_vec()).expect("a"))),
        b"INBOX"
    );
    assert_eq!(
        mailbox_bytes(&Mailbox::from(astring(b"Lists/x".to_vec()).expect("a"))),
        b"Lists/x"
    );
}

proptest! {
    // Fuzz на произвольных байтах и на искажённых примерах RFC: разбор не паникует,
    // разобранные команды сериализуются.
    #[test]
    fn fuzz_never_panics(data in proptest::collection::vec(any::<u8>(), 0..512)) {
        fuzz_input(&data);
    }

    #[test]
    fn fuzz_mutated_examples(
        base in prop::sample::select(vec![
            "a LIST (SUBSCRIBED RECURSIVEMATCH) \"\" (\"INBOX\" {3}\r\nx/%) RETURN (STATUS (SIZE))\r\n",
            "a FETCH 1:* (BODY.PEEK[1.2.MIME]<0.100> BINARY[2] UID)\r\n",
            "a CREATE {4+}\r\nnews (USE (\\Sent))\r\n",
            "a STATUS {5}\r\nINBOX (MESSAGES SIZE)\r\n",
        ]),
        pos in any::<prop::sample::Index>(),
        byte in any::<u8>(),
    ) {
        let mut data = base.as_bytes().to_vec();
        let i = pos.index(data.len());
        data[i] = byte;
        fuzz_input(&data);
    }
}
