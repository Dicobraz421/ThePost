//! Проверки разбора IMAP на произвольных данных (IMAP-13; SRS 6.8): цели cargo-fuzz `crates/tp-imap/fuzz`
//! и proptest крейта (единственные в крейте, с постоянным зерном). Каждая функция паникует, только если нарушен инвариант разбора.
//!
//! - [`command`] — поток команд IMAP4rev2: без паники, разобранная команда imap-codec сериализуется и
//!   разбирается обратно в ту же;
//! - [`literals`] — литералы и нарезка потока: разбор не зависит от того, как байты пришли по сети;
//!   байты литерала не исполняются как команда, сообщение после литерала разбирается с начала;
//! - [`search`] — критерии `SEARCH` (и `RETURN (…)`) на письмах-образцах: `NOT` обращает критерий,
//!   `ALL` ответа `ESEARCH` — ровно найденные номера;
//! - [`fetch`] — атрибуты `FETCH` по письму из байтов: разделы, `BINARY`, диапазоны `<n.m>`, ответ
//!   `* 1 FETCH (…)`.

use imap_codec::CommandCodec;
use imap_codec::encode::Encoder;
use time::OffsetDateTime;
use tp_contracts::meta::{Flag, Flags, Message};
use tp_contracts::{BlobHash, BlobKey, Gen, MailboxId, MessageId, ModSeq, TenantId, Uid};

use crate::proto::types::command::{Command as ImapCommand, CommandBody};
use crate::proto::types::core::Tag;
use crate::proto::{self, Body, Command, Expect, Input, Reader};

/// Лимит сообщения в целях (как у сессии по умолчанию — не важен, важна граница).
const MAX_MESSAGE: u32 = 64 * 1024;

/// Поток команд: без паники; команда imap-codec после сериализации разбирается в ту же.
pub fn command(data: &[u8]) {
    proto::fuzz_input(data);
    let mut reader = Reader::new(MAX_MESSAGE);
    reader.push(data);
    while let Some(input) = reader.next(Expect::Command) {
        if let Input::Command(cmd) = input {
            round_trip(cmd);
        }
    }
}

/// Сериализация imap-codec и разбор обратно дают ту же команду — с точностью до записи наборов номеров.
///
/// imap-codec при записи нормализует наборы номеров (`quirk_always_normalize_sequence_sets`: `1:1` → `1`,
/// `*:*` → `*`, `2:1` → `1:2`, `3,1` → `1,3`, слияние пересечений), поэтому разобранная обратно команда
/// может отличаться от исходной только этой записью: её сериализация обязана совпасть байт в байт.
/// Разбор наборы не меняет — их смысл раскрывает сессия (IMAP-18).
fn round_trip(cmd: Command) {
    let Body::Std(body) = &cmd.body else {
        return;
    };
    let bytes = encode(&cmd.tag, body);
    let mut reader = Reader::new(u32::MAX);
    reader.push(&bytes);
    let back = loop {
        match reader.next(Expect::Command) {
            Some(Input::Literal { .. }) => continue,
            other => break other,
        }
    };
    let shown = String::from_utf8_lossy(&bytes);
    let Some(Input::Command(back)) = back else {
        panic!("сериализация не разбирается: {shown:?}: {back:?}");
    };
    if back == cmd {
        return;
    }
    let Body::Std(again) = &back.body else {
        panic!("сериализация: {shown:?}: {back:?} вместо {cmd:?}");
    };
    assert_eq!(
        String::from_utf8_lossy(&encode(&back.tag, again)),
        shown,
        "сериализация: {back:?} вместо {cmd:?}"
    );
}

/// Запись команды кодеком imap-codec.
fn encode(tag: &Tag<'static>, body: &CommandBody<'static>) -> Vec<u8> {
    CommandCodec::default()
        .encode(&ImapCommand {
            tag: tag.clone(),
            body: body.clone(),
        })
        .dump()
}

/// Литералы: первый байт — лимит сообщения и отказы в литералах, остальное — поток клиента.
pub fn literals(data: &[u8]) {
    let Some((&ctl, rest)) = data.split_first() else {
        return;
    };
    let max = 16 + u32::from(ctl) * 8;
    let rejects = ctl.rotate_left(3);
    let whole = events(rest, max, rejects, &[rest.len().max(1)]);
    let sizes: Vec<usize> = rest
        .iter()
        .take(8)
        .map(|b| usize::from(b % 13) + 1)
        .chain([1])
        .collect();
    let chunked = events(rest, max, rejects, &sizes);
    assert_eq!(whole, chunked, "нарезка потока изменила разбор");
    framed(rest, ctl & 1 == 0);
}

/// События читателя на потоке, поданном кусками `sizes` (по кругу); `rejects` — отказ в `k`-м литерале
/// по биту `k % 8`. Что ждать дальше — как у сессии: после `AUTHENTICATE` ответ клиента, после `IDLE` — `DONE`.
fn events(data: &[u8], max: u32, rejects: u8, sizes: &[usize]) -> Vec<Input> {
    let mut reader = Reader::new(max);
    let mut out = Vec::new();
    let mut literals = 0u32;
    let mut expect = Expect::Command;
    let mut at = 0;
    for &size in sizes.iter().cycle() {
        if at >= data.len() {
            break;
        }
        let end = data.len().min(at + size);
        reader.push(&data[at..end]);
        at = end;
        while let Some(input) = reader.next(expect) {
            match &input {
                Input::Literal { .. } => {
                    if (rejects >> (literals % 8)) & 1 == 1 {
                        reader.reject_literal();
                    }
                    literals += 1;
                }
                Input::Command(Command {
                    body: Body::Std(CommandBody::Authenticate { .. }),
                    ..
                }) => expect = Expect::AuthData,
                Input::Command(Command {
                    body: Body::Std(CommandBody::Idle),
                    ..
                }) => expect = Expect::IdleDone,
                _ => expect = Expect::Command,
            }
            out.push(input);
        }
    }
    out
}

/// `APPEND` с литералом из произвольных байтов и `NOOP` за ним: литерал объявлен с тегом команды, его байты
/// не становятся командами, `NOOP` разбирается.
fn framed(payload: &[u8], sync: bool) {
    let payload = &payload[..payload.len().min(4096)];
    let plus = if sync { "" } else { "+" };
    let mut msg = format!("a APPEND INBOX {{{}{plus}}}\r\n", payload.len()).into_bytes();
    msg.extend_from_slice(payload);
    msg.extend_from_slice(b"\r\nb NOOP\r\n");
    let mut reader = Reader::new(MAX_MESSAGE);
    reader.push(&msg);
    let tag = |t: &'static str| Tag::try_from(t).ok();
    let length = u32::try_from(payload.len()).unwrap_or(u32::MAX);
    assert_eq!(
        reader.next(Expect::Command),
        Some(Input::Literal {
            tag: tag("a"),
            length,
            sync
        })
    );
    match reader.next(Expect::Command) {
        Some(Input::Command(cmd)) => {
            assert!(matches!(cmd.body, Body::Std(CommandBody::Append { .. })));
            assert_eq!(Some(cmd.tag), tag("a"));
        }
        Some(Input::Bad { tag: t, .. }) => assert_eq!(t, tag("a")),
        other => panic!("APPEND: {other:?}"),
    }
    match reader.next(Expect::Command) {
        Some(Input::Command(cmd)) => {
            assert_eq!(cmd.body, Body::Std(CommandBody::Noop));
            assert_eq!(Some(cmd.tag), tag("b"));
        }
        other => panic!("NOOP после литерала: {other:?}"),
    }
    assert_eq!(reader.next(Expect::Command), None);
}

/// Критерии `SEARCH`: `a SEARCH <data>` и `a UID SEARCH RETURN (…) <data>` на письмах-образцах.
pub fn search(data: &[u8]) {
    let list = samples();
    for head in [
        &b"a SEARCH "[..],
        b"a UID SEARCH RETURN (MIN MAX ALL COUNT) ",
        b"a SEARCH RETURN () ",
    ] {
        let Some(cmd) = parse(head, data) else {
            continue;
        };
        match cmd.body {
            Body::Std(CommandBody::Search { criteria, uid, .. }) => {
                crate::search::fuzz(criteria, uid, &[], &list);
            }
            Body::ESearch {
                criteria, uid, ret, ..
            } => crate::search::fuzz(criteria, uid, &ret, &list),
            _ => {}
        }
    }
}

/// Атрибуты `FETCH`: до первого NUL — атрибуты (`a FETCH 1 <…>`), после — письмо.
pub fn fetch(data: &[u8]) {
    let (items, raw) = match data.iter().position(|&b| b == 0) {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &b"Subject: s\r\n\r\nbody\r\n"[..]),
    };
    for head in [&b"a FETCH 1 "[..], b"a UID FETCH 1:* "] {
        if let Some(Command {
            body:
                Body::Std(CommandBody::Fetch {
                    macro_or_item_names,
                    uid,
                    ..
                }),
            ..
        }) = parse(head, items)
        {
            crate::fetch::fuzz(macro_or_item_names, uid, raw);
        }
    }
}

/// Команда `head` + `data` + CRLF через [`Reader`] (литералы принимаются).
fn parse(head: &[u8], data: &[u8]) -> Option<Command> {
    let mut reader = Reader::new(MAX_MESSAGE);
    reader.push(head);
    reader.push(data);
    reader.push(b"\r\n");
    loop {
        match reader.next(Expect::Command)? {
            Input::Literal { .. } => continue,
            Input::Command(cmd) => return Some(cmd),
            _ => return None,
        }
    }
}

/// Письма папки для критериев: разные флаги, размеры, даты, конверты.
fn samples() -> Vec<Message> {
    let raws: [&[u8]; 4] = [
        b"From: Alice <alice@example.org>\r\nTo: bob@example.org\r\nSubject: Hello\r\n\
          Date: Mon, 2 Nov 2026 10:00:00 +0300\r\nMessage-ID: <1@example.org>\r\n\r\nhi\r\n",
        b"From: =?utf-8?B?0JDQvdC90LA=?= <anna@example.org>\r\nCc: c@example.org\r\n\
          Subject: =?utf-8?B?0J/RgNC40LLQtdGC?=\r\nIn-Reply-To: <1@example.org>\r\n\r\ntext\r\n",
        b"Subject: \r\n\r\n",
        b"",
    ];
    let flags: [&[Flag]; 4] = [
        &[Flag::Seen],
        &[Flag::Answered, Flag::Flagged, Flag::Keyword("$Junk".into())],
        &[Flag::Deleted, Flag::Draft],
        &[],
    ];
    raws.iter()
        .zip(flags)
        .enumerate()
        .map(|(i, (raw, flags))| {
            let n = i as u64;
            Message {
                id: MessageId::new(),
                mailbox: MailboxId::new(),
                uid: Uid(1 + 3 * i as u32),
                modseq: ModSeq(10 * n),
                size: raw.len() as u64,
                received_at: OffsetDateTime::from_unix_timestamp(1_790_000_000 + 86_400 * n as i64)
                    .unwrap_or(OffsetDateTime::UNIX_EPOCH),
                flags: flags.iter().cloned().collect::<Flags>(),
                skeleton: BlobKey {
                    tenant: TenantId::DEFAULT,
                    hash: BlobHash([0; 32]),
                    generation: Gen::INITIAL,
                },
                parts: Vec::new(),
                meta: tp_mailstore::meta::message_meta(raw),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use std::num::NonZeroU32;

    /// Примеры RFC 9051 и расширений — начальный корпус целей cargo-fuzz.
    const EXAMPLES: &[&str] = &[
        "a LIST (SUBSCRIBED RECURSIVEMATCH) \"\" (\"INBOX\" {3}\r\nx/%) RETURN (STATUS (SIZE))\r\n",
        "a FETCH 1:* (BODY.PEEK[1.2.MIME]<0.100> BINARY[2] UID)\r\n",
        "a CREATE {4+}\r\nnews (USE (\\Sent))\r\n",
        "a STATUS {5}\r\nINBOX (MESSAGES SIZE)\r\n",
        "a UID SEARCH RETURN (MIN COUNT) OR SUBJECT {5}\r\nhello NOT SEEN\r\n",
        "a APPEND INBOX (\\Seen) {5+}\r\nhello\r\n",
        "a AUTHENTICATE PLAIN\r\nAGFAYg==\r\na IDLE\r\nDONE\r\n",
        "a LOGIN {4}\r\nuser {6}\r\nsecret\r\n",
        // IMAP-18: наборы номеров, которые imap-codec при записи нормализует.
        "a FETCH *:* (BODY.PEEK[1.2.MIME]<0.100> BINARY[2] UID)\r\n",
        "a FETCH 1:1 (BODY.PEEK[1.2.MIME]<0.100> BINARY[2] UID)\r\n",
        "a UID FETCH 4,2:1,3:3,7:*,6 (UID)\r\n",
        "a UID SEARCH UID 5:5,9:3 NOT 2:2\r\n",
    ];

    /// Зерно proptest-тестов: в CI они детерминированы, случайный поиск — ночной `fuzz.yml` (IMAP-18, IMAP-21).
    const SEED: u64 = 0x494d_4150_3138;

    #[test]
    fn examples_pass_checks() {
        for e in EXAMPLES {
            command(e.as_bytes());
            literals(e.as_bytes());
        }
        for c in [
            "OR (SEEN FLAGGED) NOT (UID 4:* LARGER 10)",
            "SUBJECT hello FROM alice SINCE 1-Jan-2026 BODY x",
            "HEADER Message-ID 1 TEXT {3}\r\nabc MODSEQ 5 KEYWORD $Junk",
            "1:2,4 UNDELETED",
        ] {
            assert!(parse(b"a SEARCH ", c.as_bytes()).is_some(), "{c}");
            search(c.as_bytes());
        }
        for f in [
            "(BODY[HEADER.FIELDS (Subject {4}\r\nFrom)]<1.5> BINARY.PEEK[1]<0.3> BINARY.SIZE[1])",
            "(BODYSTRUCTURE ENVELOPE RFC822.HEADER RFC822.TEXT MODSEQ)",
            "FULL",
            "(BODY[1.MIME] BODY[2.HEADER] BODY[TEXT] BINARY[]<2.2>)\0\
             Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\n\r\none\r\n--b\r\n\
             Content-Type: message/rfc822\r\nContent-Transfer-Encoding: base64\r\n\r\n\
             U3ViamVjdDogbgoKYm9keQo=\r\n--b--\r\n",
        ] {
            let items = f.split('\0').next().unwrap_or_default();
            assert!(parse(b"a FETCH 1 ", items.as_bytes()).is_some(), "{items}");
            fetch(f.as_bytes());
        }
    }

    #[test]
    fn search_examples_find_messages() {
        // Образцы с флагами: SEEN — только первое письмо.
        let list = samples();
        let Some(Command {
            body: Body::ESearch { criteria, ret, .. },
            ..
        }) = parse(b"a SEARCH RETURN (ALL) ", b"SEEN")
        else {
            panic!("ESEARCH");
        };
        crate::search::fuzz(criteria, false, &ret, &list);
    }

    /// IMAP-18: вырожденный диапазон `n:n` разбирается как диапазон (разбор набор не переписывает),
    /// а проверка обратной записи на нём не падает.
    #[test]
    fn degenerate_range_round_trips() {
        use crate::proto::types::sequence::{SeqOrUid, Sequence};
        for (set, edge) in [
            ("*:*", SeqOrUid::Asterisk),
            ("1:1", SeqOrUid::from(NonZeroU32::MIN)),
        ] {
            let line = format!("a FETCH {set} (BODY.PEEK[1.2.MIME]<0.100> BINARY[2] UID)\r\n");
            let Some(Command {
                body: Body::Std(CommandBody::Fetch { sequence_set, .. }),
                ..
            }) = parse(b"", line.trim_end().as_bytes())
            else {
                panic!("FETCH {set}");
            };
            assert_eq!(sequence_set.0.as_ref(), [Sequence::Range(edge, edge)]);
            command(line.as_bytes());
            literals(line.as_bytes());
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            rng_seed: proptest::test_runner::RngSeed::Fixed(SEED),
            failure_persistence: None,
            ..ProptestConfig::default()
        })]

        #[test]
        fn arbitrary_bytes(data in proptest::collection::vec(any::<u8>(), 0..512)) {
            command(&data);
            literals(&data);
            search(&data);
            fetch(&data);
        }

        #[test]
        fn mutated_examples(
            base in prop::sample::select(EXAMPLES),
            pos in any::<prop::sample::Index>(),
            byte in any::<u8>(),
        ) {
            let mut data = base.as_bytes().to_vec();
            let i = pos.index(data.len());
            data[i] = byte;
            command(&data);
            literals(&data);
        }
    }
}
