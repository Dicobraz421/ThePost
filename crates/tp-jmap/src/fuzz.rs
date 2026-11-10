//! Проверки разбора JMAP на произвольных данных (JMAP-09; SRS 6.8): цели cargo-fuzz `crates/tp-jmap/fuzz`
//! и тесты крейта на примерах. Каждая функция паникует, только если нарушен инвариант разбора.
//!
//! - [`request`] — тело запроса API: объект Request и вызовы методов, выполненные как `Core/echo`
//!   (ответ — аргументы после подстановки ссылок на результаты); подстановка не теряет и не путает
//!   аргументы, бюджет подстановок не превышается, ответ сериализуется;
//! - [`reference`] — ссылки на результаты (RFC 8620 §3.7): до первого NUL — JSON-ответ, после — байты
//!   выбора пути; путь JSON Pointer с экранированием `~0`/`~1` находит выбранное значение, `*` — элементы
//!   массива, неверные `resultOf`/`name` и исчерпанный бюджет — `invalidResultReference`;
//! - [`query`] — аргументы `Email/query` (фильтр, сортировка, `collapseThreads`, окно) на письмах-образцах:
//!   `NOT` обращает фильтр, найдено ровно подходящее и по порядку сортировки, окно — часть результатов.

use serde_json::{Map, Value, json};
use time::OffsetDateTime;
use tp_contracts::meta::{Flag, Flags, Message};
use tp_contracts::{BlobHash, BlobKey, Gen, MailboxId, MessageId, ModSeq, TenantId, Uid};

use crate::core::request::{self, Invocation, Response, object, pointer, resolve_references};

/// Лимит тела запроса в целях (`maxSizeRequest`; важна граница, не значение).
const MAX_REQUEST: u64 = 1 << 20;
/// Вызовов в запросе (`maxCallsInRequest` по умолчанию).
const MAX_CALLS: usize = 16;
/// Байт подстановок за запрос: меньше лимита по умолчанию, чтобы цели не тратили время на копирование.
const BUDGET: u64 = 64 * 1024;
/// Наибольший `limit` окна `/query`.
const MAX_QUERY_LIMIT: u32 = 3;

/// Папки писем-образцов (`inMailbox` в примерах).
pub const MAILBOXES: [&str; 2] = [
    "00000000-0000-7000-8000-00000000000a",
    "00000000-0000-7000-8000-00000000000b",
];

/// Тело запроса API: вызовы выполняются как `Core/echo`, ответ сериализуется.
pub fn request(data: &[u8]) {
    let Ok(req) = request::parse(data, MAX_REQUEST) else {
        return;
    };
    let mut budget = BUDGET;
    let mut responses: Vec<Invocation> = Vec::new();
    for Invocation(name, args, call_id) in req.method_calls.into_iter().take(MAX_CALLS) {
        let before = budget;
        match resolve_references(args.clone(), &responses, &mut budget) {
            Ok(out) => {
                resolved(&args, &out, &responses);
                responses.push(Invocation(name, out, call_id));
            }
            Err(e) => {
                assert_eq!(budget, before, "ошибка подстановки израсходовала бюджет");
                let body = e.to_json();
                assert!(body.get("type").is_some_and(Value::is_string), "{body}");
                responses.push(Invocation("error".into(), object(body), call_id));
            }
        }
    }
    let resp = Response {
        method_responses: responses,
        created_ids: req.created_ids,
        session_state: "0".into(),
    };
    if let Err(e) = serde_json::to_vec(&resp) {
        panic!("ответ не сериализуется: {e}");
    }
    let back = serde_json::to_value(&resp)
        .ok()
        .and_then(|v| v.get("methodResponses").cloned())
        .and_then(|v| serde_json::from_value::<Vec<Invocation>>(v).ok());
    assert_eq!(
        back.as_ref(),
        Some(&resp.method_responses),
        "ответы методов"
    );
}

/// Подстановка: те же ключи без `#`, значения без ссылок не тронуты, по ссылке — значение по пути из ответа.
fn resolved(args: &Map<String, Value>, out: &Map<String, Value>, responses: &[Invocation]) {
    assert_eq!(out.len(), args.len(), "число аргументов");
    for (key, value) in args {
        let Some(name) = key.strip_prefix('#') else {
            assert_eq!(out.get(key), Some(value), "аргумент {key}");
            continue;
        };
        let field = |f: &str| value.get(f).and_then(Value::as_str).unwrap_or_default();
        let resp = responses
            .iter()
            .find(|i| i.2 == field("resultOf"))
            .unwrap_or_else(|| panic!("ссылка {key} без ответа"));
        assert_eq!(resp.0, field("name"), "имя ответа {key}");
        let expect = pointer(&Value::Object(resp.1.clone()), field("path"));
        assert_eq!(out.get(name), expect.as_ref(), "ссылка {key}");
    }
}

/// Ссылки на результаты: до первого NUL — JSON-ответ, после — байты выбора пути.
pub fn reference(data: &[u8]) {
    let (doc, sel) = match data.iter().position(|&b| b == 0) {
        Some(i) => (&data[..i], &data[i + 1..]),
        None => (data, &[][..]),
    };
    let any = String::from_utf8_lossy(sel);
    let Ok(v) = serde_json::from_slice::<Value>(doc) else {
        let _ = pointer(&json!({"a": [1, {"b": [2, 3]}], "~/": {"*": 4}}), &any);
        return;
    };
    let _ = pointer(&v, &any);
    assert_eq!(pointer(&v, "").as_ref(), Some(&v), "пустой путь");
    // Путь по байтам выбора: ключ объекта или номер элемента, токены экранированы (RFC 6901).
    let mut path = String::new();
    let mut at = &v;
    for &b in sel.iter().take(64) {
        at = match at {
            Value::Object(m) if !m.is_empty() => {
                let Some((k, next)) = m.iter().nth(usize::from(b) % m.len()) else {
                    break;
                };
                path.push('/');
                path.push_str(&k.replace('~', "~0").replace('/', "~1"));
                next
            }
            Value::Array(items) if !items.is_empty() => {
                let i = usize::from(b) % items.len();
                path.push_str(&format!("/{i}"));
                &items[i]
            }
            _ => break,
        };
        assert_eq!(pointer(&v, &path).as_ref(), Some(at), "путь {path}");
    }
    if let Value::Array(items) = at {
        let flat: Vec<Value> = items
            .iter()
            .flat_map(|i| match i {
                Value::Array(inner) => inner.clone(),
                x => vec![x.clone()],
            })
            .collect();
        assert_eq!(
            pointer(&v, &format!("{path}/*")),
            Some(Value::Array(flat)),
            "путь {path}/*"
        );
    }
    // Ссылка `#r` на ответ `c0`: значение по пути, иначе — `invalidResultReference`.
    let responses = [Invocation(
        "Foo/get".into(),
        object(json!({ "v": v })),
        "c0".into(),
    )];
    let args = |result_of: &str, name: &str| {
        object(json!({
            "x": 1,
            "#r": { "resultOf": result_of, "name": name, "path": format!("/v{path}") },
        }))
    };
    let mut budget = u64::MAX;
    match resolve_references(args("c0", "Foo/get"), &responses, &mut budget) {
        Ok(out) => {
            assert_eq!(out.get("r"), Some(at), "#r по пути {path}");
            assert_eq!(out.get("x"), Some(&json!(1)));
            assert!(!out.contains_key("#r"));
        }
        Err(e) => panic!("ссылка по пути {path}: {e:?}"),
    }
    for (result_of, name, mut budget) in [
        ("c1", "Foo/get", u64::MAX),
        ("c0", "Bar/get", u64::MAX),
        ("c0", "Foo/get", 0),
    ] {
        match resolve_references(args(result_of, name), &responses, &mut budget) {
            Err(e) => assert_eq!(e.kind, "invalidResultReference", "{result_of} {name}"),
            Ok(out) => panic!("ссылка {result_of} {name} при бюджете {budget}: {out:?}"),
        }
    }
}

/// Аргументы `Email/query` (JSON-объект; без `accountId` — подставляется) на письмах-образцах.
pub fn query(data: &[u8]) {
    let Ok(Value::Object(mut args)) = serde_json::from_slice::<Value>(data) else {
        return;
    };
    args.entry("accountId").or_insert_with(|| json!("a"));
    crate::email_query::fuzz(args, &samples(), MAX_QUERY_LIMIT);
}

/// Письма для фильтров: две папки, цепочка из двух писем, разные даты, размеры, ключевые слова, вложение.
fn samples() -> Vec<Message> {
    let raws: [&[u8]; 4] = [
        b"From: Alice <alice@example.org>\r\nTo: bob@example.org\r\nSubject: Hello\r\n\
          Date: Mon, 2 Nov 2026 10:00:00 +0300\r\nMessage-ID: <1@example.org>\r\n\r\nhi\r\n",
        b"From: =?utf-8?B?0JDQvdC90LA=?= <anna@example.org>\r\nTo: Bob <bob@example.org>\r\n\
          Subject: Re: Hello\r\nIn-Reply-To: <1@example.org>\r\nReferences: <1@example.org>\r\n\
          Message-ID: <2@example.org>\r\n\r\ntext\r\n",
        b"From: c@example.org\r\nSubject: =?utf-8?B?0YHRh9GR0YI=?=\r\nMIME-Version: 1.0\r\n\
          Content-Type: multipart/mixed; boundary=b\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nx\r\n\
          --b\r\nContent-Type: application/pdf\r\nContent-Disposition: attachment; filename=a.pdf\r\n\r\n\
          %PDF\r\n--b--\r\n",
        b"",
    ];
    let flags: [&[Flag]; 4] = [
        &[Flag::Seen],
        &[Flag::Seen, Flag::Flagged, Flag::Keyword("$Junk".into())],
        &[Flag::Draft, Flag::Keyword("work".into())],
        &[Flag::Deleted],
    ];
    raws.iter()
        .zip(flags)
        .enumerate()
        .map(|(i, (raw, flags))| {
            let n = i as u64;
            Message {
                id: format!("00000000-0000-7000-8000-00000000000{i}")
                    .parse()
                    .unwrap_or_else(|_| MessageId::new()),
                mailbox: MAILBOXES[i % 2]
                    .parse()
                    .unwrap_or_else(|_| MailboxId::new()),
                uid: Uid(1 + i as u32),
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

    /// Запросы API — начальный корпус цели `request`.
    pub(super) const REQUESTS: &[&str] = &[
        r##"{"using":["urn:ietf:params:jmap:core"],"methodCalls":[["Core/echo",{"a":1,"b":[{"id":"x"},{"id":"y"}]},"c0"],["Core/echo",{"#ids":{"resultOf":"c0","name":"Core/echo","path":"/b/*/id"}},"c1"]]}"##,
        r##"{"using":[],"methodCalls":[["Email/query",{"accountId":"a"},"0"],["Email/get",{"#ids":{"resultOf":"0","name":"Email/query","path":"/ids"},"properties":["subject"]},"1"]],"createdIds":{"k":"M1"}}"##,
        r##"{"using":[],"methodCalls":[["A",{"x":{"~/":[1,2]}},"a"],["B",{"#y":{"resultOf":"a","name":"A","path":"/x/~0~1/1"},"y":0},"b"],["C",{"#z":{"resultOf":"b","name":"error","path":"/type"}},"c"]]}"##,
        r##"{"using":[],"methodCalls":[["E",{"s":"0123456789"},"0"],["E",{"#a":{"resultOf":"0","name":"E","path":""},"#b":{"resultOf":"0","name":"E","path":""}},"1"],["E",{"#a":{"resultOf":"1","name":"E","path":""},"#b":{"resultOf":"1","name":"E","path":""}},"2"]]}"##,
        r##"{"using":[],"methodCalls":[["E",{"#a":{"resultOf":"0","name":"E","path":"/x","extra":1}},"0"]]}"##,
        r##"{"using":["urn:ietf:params:jmap:mail"],"methodCalls":[["Email/get",{},"c"],[1,{},"c"]]}"##,
    ];

    /// Ответ и байты выбора пути — начальный корпус цели `reference`.
    pub(super) const REFERENCES: &[&[u8]] = &[
        b"{\"list\":[{\"id\":\"a\",\"t\":[1,2]},{\"id\":\"b\",\"t\":[3]}],\"~a/b\":{\"*\":[[]]}}\0\x00\x00\x01",
        b"[[1,[2]],{\"\":0}]\0\x01\x00",
        b"{\"ids\":[\"x\",\"y\"],\"notFound\":null}\0\x00",
        b"\"s\"\0/a/~2",
    ];

    /// Аргументы `Email/query` — начальный корпус цели `query`.
    pub(super) fn queries() -> Vec<String> {
        let [inbox, other] = MAILBOXES;
        vec![
            format!(r##"{{"filter":{{"inMailbox":"{inbox}"}},"sort":[{{"property":"size","isAscending":true}}],"limit":2,"calculateTotal":true}}"##),
            format!(r##"{{"filter":{{"operator":"OR","conditions":[{{"hasKeyword":"$seen","inMailboxOtherThan":["{other}"]}},{{"operator":"NOT","conditions":[{{"subject":"hello"}},{{"hasAttachment":true}}]}}]}},"collapseThreads":true}}"##),
            r##"{"filter":{"operator":"AND","conditions":[{"after":"2026-09-01T00:00:00Z","before":"2027-01-01T00:00:00Z"},{"minSize":1,"maxSize":100000},{"someInThreadHaveKeyword":"$flagged"}]},"sort":[{"property":"allInThreadHaveKeyword","keyword":"$seen"},{"property":"subject","isAscending":false}],"position":-2}"##.into(),
            r##"{"filter":{"text":"re","from":"a","header":["Message-ID","1"],"notKeyword":"work"},"sort":[{"property":"from"},{"property":"receivedAt"}],"anchor":"00000000-0000-7000-8000-000000000001","anchorOffset":9223372036854775807}"##.into(),
            r##"{"filter":{"noneInThreadHaveKeyword":"$junk","header":["X"]},"sort":[{"property":"hasKeyword","keyword":"$draft"},{"property":"sentAt"},{"property":"to"}],"anchor":"00000000-0000-7000-8000-000000000002","anchorOffset":-9223372036854775808,"limit":18446744073709551615}"##.into(),
            r##"{"filter":{"operator":"XOR","conditions":[]},"anchor":"zzz"}"##.into(),
            r##"{"filter":{"before":"not a date"},"sort":[{"property":"size"}],"position":9223372036854775807}"##.into(),
            r##"{"anchor":"zzz","position":-9223372036854775808}"##.into(),
            r##"{"sort":[{"property":"size"}],"anchor":"00000000-0000-7000-8000-000000000001","anchorOffset":9223372036854775807}"##.into(),
        ]
    }

    #[test]
    fn examples_pass_checks() {
        for r in REQUESTS {
            assert!(
                request::parse(r.as_bytes(), MAX_REQUEST).is_ok() || r.contains("[1,"),
                "{r}"
            );
            request(r.as_bytes());
        }
        for r in REFERENCES {
            reference(r);
        }
        for q in queries() {
            assert!(serde_json::from_str::<Value>(&q).is_ok(), "{q}");
            query(q.as_bytes());
        }
    }

    #[test]
    fn arbitrary_bytes_do_not_panic() {
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        let mut samples: Vec<Vec<u8>> = REQUESTS.iter().map(|r| r.as_bytes().to_vec()).collect();
        samples.extend(REFERENCES.iter().map(|r| r.to_vec()));
        samples.extend(queries().into_iter().map(String::into_bytes));
        for base in &samples {
            for _ in 0..64 {
                // Простой генератор (xorshift): заменённый байт в примере.
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let mut data = base.clone();
                let at = (seed % data.len().max(1) as u64) as usize;
                if let Some(b) = data.get_mut(at) {
                    *b = (seed >> 32) as u8;
                }
                request(&data);
                reference(&data);
                query(&data);
            }
        }
    }

    #[test]
    fn query_examples_find_messages() {
        let list = samples();
        assert_eq!(list[0].meta.thread_key, list[1].meta.thread_key, "цепочка");
        assert!(list[2].meta.has_attachment, "вложение");
        assert_eq!(list[1].mailbox.to_string(), MAILBOXES[1]);
    }

    #[test]
    fn echo_chain_stops_at_budget() {
        // Каждый вызов удваивает предыдущий ответ: без бюджета 2^n копий.
        let mut calls = vec![json!(["E", {"s": "x".repeat(1000)}, "0"])];
        for i in 1..16 {
            let r = json!({"resultOf": (i - 1).to_string(), "name": "E", "path": ""});
            calls.push(json!(["E", {"#a": r, "#b": r}, i.to_string()]));
        }
        let req = json!({"using": [], "methodCalls": calls}).to_string();
        let Ok(parsed) = request::parse(req.as_bytes(), MAX_REQUEST) else {
            panic!("запрос");
        };
        let mut budget = BUDGET;
        let mut responses = Vec::new();
        let mut errors = 0;
        for Invocation(name, args, id) in parsed.method_calls {
            match resolve_references(args, &responses, &mut budget) {
                Ok(out) => responses.push(Invocation(name, out, id)),
                Err(e) => {
                    assert_eq!(e.kind, "invalidResultReference");
                    errors += 1;
                    responses.push(Invocation("error".into(), object(e.to_json()), id));
                }
            }
        }
        assert!(errors > 0, "бюджет не сработал");
        let size = serde_json::to_vec(&responses)
            .map(|v| v.len())
            .unwrap_or(usize::MAX);
        assert!(size < 4 * BUDGET as usize, "ответ {size} байт");
        request(req.as_bytes());
    }
}
