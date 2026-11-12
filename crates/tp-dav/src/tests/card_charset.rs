//! DAV-33: 8-битные vCard (`CHARSET` без `ENCODING`) — перекод в UTF-8 перед calcard, `PUT` и поиск на корпусе
//! `tp-testkit` (`16-v21-cp1251-8bit`).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tp_testkit::corpus::{CorpusCard, vcard};

use super::{BOOKS, Env, LOGIN, basic};
use crate::charset::vcard_utf8;

/// Весь корпус: перекод совпадает с `text` корпуса, тело в UTF-8 — без копии.
#[test]
fn corpus_to_utf8() {
    let cards = tp_testkit::vcards().expect("корпус");
    assert!(
        cards.iter().any(CorpusCard::is_8bit),
        "в корпусе есть 8-битный файл"
    );
    for c in cards {
        let got = vcard_utf8(&c.raw).expect("перекод");
        assert_eq!(got, c.text, "{}", c.name);
        assert_eq!(
            matches!(got, std::borrow::Cow::Owned(_)),
            c.is_8bit(),
            "{}",
            c.name
        );
    }
}

/// `CHARSET` у каждого свойства свой, строка продолжения — по своему свойству, свойство без `CHARSET` — по первому
/// `CHARSET` тела; без `CHARSET` и с неизвестной кодировкой — `None`.
#[test]
fn charset_per_property() {
    // «Иван» в windows-1251, «Иван» в KOI8-R со свёрнутой строкой, «Ив» без своего CHARSET.
    let raw = b"BEGIN:VCARD\r\nVERSION:3.0\r\nFN;CHARSET=windows-1251:\xC8\xE2\xE0\xED\r\n\
        NOTE;CHARSET=\"KOI8-R\":\xE9\xD7\r\n \xC1\xCE\r\nNICKNAME:\xC8\xE2\r\nEND:VCARD\r\n";
    assert_eq!(
        vcard_utf8(raw).expect("перекод"),
        "BEGIN:VCARD\r\nVERSION:3.0\r\nFN;CHARSET=windows-1251:Иван\r\n\
         NOTE;CHARSET=\"KOI8-R\":Ив\r\n ан\r\nNICKNAME:Ив\r\nEND:VCARD\r\n"
    );
    assert_eq!(vcard_utf8(b"BEGIN:VCARD\r\nFN:\xC8\r\nEND:VCARD\r\n"), None);
    assert_eq!(
        vcard_utf8(b"BEGIN:VCARD\r\nFN;CHARSET=x-nope:\xC8\r\nEND:VCARD\r\n"),
        None
    );
}

/// Карточки файла корпуса по отдельности: `VERSION:2.1` → `3.0` и `UID` (CardDAV принимает 3.0 и 4.0 с `UID`),
/// значения — байты windows-1251 как в файле.
fn cards_of(c: &CorpusCard) -> Vec<Vec<u8>> {
    let begin = b"BEGIN:VCARD";
    let mut starts: Vec<usize> = (0..c.raw.len())
        .filter(|&i| c.raw[i..].starts_with(begin))
        .collect();
    starts.push(c.raw.len());
    starts
        .windows(2)
        .enumerate()
        .map(|(n, w)| {
            let card = &c.raw[w[0]..w[1]];
            let at = card
                .windows(13)
                .position(|l| l == b"VERSION:2.1\r\n")
                .expect("VERSION:2.1");
            let mut out = card[..at].to_vec();
            out.extend_from_slice(format!("VERSION:3.0\r\nUID:k-{n}\r\n").as_bytes());
            out.extend_from_slice(&card[at + 13..]);
            out
        })
        .collect()
}

/// `PUT` 8-битной карточки: кириллица сохраняется верно, тело — UTF-8 без `CHARSET`, ответ без `ETag`;
/// `addressbook-query` по `FN` находит карточку.
#[tokio::test]
async fn put_8bit_corpus() {
    let corpus = tokio::task::spawn_blocking(|| vcard("v21-cp1251-8bit"))
        .await
        .expect("задача")
        .expect("корпус");
    assert!(corpus.is_8bit());
    let env = Env::new();
    env.propfind(BOOKS, "0", "").await;
    let cards = cards_of(&corpus);
    assert_eq!(cards.len(), 2);
    for (n, body) in cards.into_iter().enumerate() {
        let req = Request::builder()
            .method("PUT")
            .uri(format!("{BOOKS}contacts/k-{n}.vcf"))
            .header("authorization", basic(LOGIN, "secret"))
            .header("content-type", "text/vcard; charset=windows-1251")
            .body(Body::from(body))
            .expect("запрос");
        let r = env.raw(req).await;
        assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
        assert_eq!(r.header("etag"), None, "тело изменено сервером");
    }
    // Тело карточки без переносов строк (запись calcard переносит длинные строки).
    let get = |n: usize| {
        let env = &env;
        async move {
            let r = env
                .send("GET", &format!("{BOOKS}contacts/k-{n}.vcf"), &[], "")
                .await;
            assert_eq!(r.status, StatusCode::OK, "{}", r.body);
            r.body.replace("\r\n ", "")
        }
    };
    let first = get(0).await;
    assert!(first.contains("FN:Елена Андреевна Козлова\r\n"), "{first}");
    assert!(first.contains("Садовая"), "{first}");
    assert!(!first.to_ascii_uppercase().contains("CHARSET"), "{first}");
    let second = get(1).await;
    assert!(second.contains("ORG:ИП Ёлкин Г."), "{second}");
    // NOTE — QP поверх того же CHARSET: раскодирует calcard.
    assert!(second.contains("«ёжик» и щука."), "{second}");
    // Поиск по поисковым строкам контакта (FN) находит кириллицу.
    let q = r#"<card:addressbook-query xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
        <d:prop><d:getetag/></d:prop>
        <card:filter><card:prop-filter name="FN"><card:text-match>Ёлкин</card:text-match></card:prop-filter></card:filter>
        </card:addressbook-query>"#;
    let r = env
        .send("REPORT", &format!("{BOOKS}contacts/"), &[], q)
        .await;
    assert!(r.body.contains("contacts/k-1.vcf"), "{}", r.body);
    assert!(!r.body.contains("contacts/k-0.vcf"), "{}", r.body);
}

/// Не UTF-8 и без `CHARSET` — `valid-address-data`.
#[tokio::test]
async fn put_8bit_without_charset() {
    let env = Env::new();
    env.propfind(BOOKS, "0", "").await;
    let req = Request::builder()
        .method("PUT")
        .uri(format!("{BOOKS}contacts/x.vcf"))
        .header("authorization", basic(LOGIN, "secret"))
        .body(Body::from(
            b"BEGIN:VCARD\r\nVERSION:3.0\r\nUID:x\r\nFN:\xC8\xE2\xE0\xED\r\nEND:VCARD\r\n".to_vec(),
        ))
        .expect("запрос");
    let r = env.raw(req).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("<card:valid-address-data/>"), "{}", r.body);
}
