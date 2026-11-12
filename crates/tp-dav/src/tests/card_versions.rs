//! DAV-11: выдача карточек в версии vCard 3.0 или 4.0 (`Accept`, `address-data`), преобразование без потери
//! полей на корпусе карточек клиентов, предусловия `PUT` (RFC 6352 §6.3.2.1).

use std::collections::BTreeSet;

use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use calcard::vcard::{VCard, VCardVersion};

use super::{BOOKS, Env, vcard};
use crate::DavConfig;
use crate::card::convert;

/// Контакты iOS и macOS, vCard 3.0: группы свойств `itemN`, метки `X-ABLabel`, `type=pref`, фото.
const APPLE: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nPRODID:-//Apple Inc.//iOS 17.4//EN\r\n\
    N:Петров;Иван;Сергеевич;г-н;\r\nFN:г-н Иван Сергеевич Петров\r\nNICKNAME:Ваня\r\n\
    ORG:ООО Ромашка;Бухгалтерия\r\nTITLE:Главный бухгалтер\r\n\
    EMAIL;type=INTERNET;type=WORK;type=pref:ivan@example.org\r\nEMAIL;type=INTERNET;type=HOME:vanya@mail.example\r\n\
    TEL;type=CELL;type=VOICE;type=pref:+7 916 000-11-22\r\nTEL;type=WORK;type=VOICE:+7 495 123-45-67\r\n\
    item1.ADR;type=WORK;type=pref:;;ул. Ленина\\, д. 1;Москва;;101000;Россия\r\nitem1.X-ABADR:ru\r\n\
    item2.URL;type=pref:https://example.org/~ivan\r\nitem2.X-ABLabel:_$!<HomePage>!$_\r\n\
    BDAY;value=date:1985-04-12\r\nNOTE:Строка 1\\nСтрока 2\\; с точкой с запятой\r\n\
    PHOTO;ENCODING=b;TYPE=JPEG:/9j/4AAQSkZJRgABAQ==\r\nCATEGORIES:Коллеги,Друзья\r\n\
    X-SOCIALPROFILE;type=telegram:https://t.me/ivan\r\nREV:2026-01-05T10:00:00Z\r\nUID:c-ivan\r\nEND:VCARD\r\n";

/// Группа контактов Apple, vCard 3.0.
const APPLE_GROUP: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nN:Отдел продаж;;;;\r\nFN:Отдел продаж\r\n\
    X-ADDRESSBOOKSERVER-KIND:group\r\nX-ADDRESSBOOKSERVER-MEMBER:urn:uuid:c-ivan\r\n\
    X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:c-maria\r\nUID:g-sales\r\nEND:VCARD\r\n";

/// Thunderbird и DAVx5, vCard 4.0: `PREF=1`, `VALUE=uri`, фото `data:`.
const V4: &str = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:urn:uuid:4fbe8971-0bc3-424c-9c26-36c3e1eff6b1\r\n\
    FN:Мария Иванова\r\nN:Иванова;Мария;;;\r\nEMAIL;PREF=1;TYPE=work:maria@example.org\r\n\
    EMAIL;TYPE=home:masha@mail.example\r\nTEL;VALUE=uri;TYPE=\"cell,voice\";PREF=1:tel:+7-903-555-66-77\r\n\
    ADR;TYPE=home:;;ул. Мира\\, 5;Казань;;420000;Россия\r\nBDAY:19900215\r\nANNIVERSARY:20150620\r\n\
    GENDER:F\r\nLANG;PREF=1:ru\r\nORG:АО Пример;Продажи\r\nROLE:Менеджер\r\n\
    PHOTO:data:image/png;base64,iVBORw0KGgo=\r\nIMPP:xmpp:maria@example.org\r\n\
    RELATED;TYPE=spouse:urn:uuid:c-ivan\r\nNOTE:Заметка\r\nREV:20260105T100000Z\r\nEND:VCARD\r\n";

/// Группа vCard 4.0 (DAVx5, Evolution): без `N`.
const V4_GROUP: &str = "BEGIN:VCARD\r\nVERSION:4.0\r\nKIND:group\r\nFN:Проект\r\nMEMBER:urn:uuid:c-ivan\r\n\
    MEMBER:urn:uuid:c-maria\r\nUID:g-proj\r\nEND:VCARD\r\n";

const CORPUS: [&str; 4] = [APPLE, APPLE_GROUP, V4, V4_GROUP];

fn parse(body: &str) -> VCard {
    VCard::parse(body).expect("vCard")
}

/// Строки карточки (развёрнутые, без учёта регистра) в версии `v` — для сравнения состава полей.
fn lines(body: &str, v: VCardVersion) -> BTreeSet<String> {
    let mut out = String::new();
    parse(body).write_to(&mut out, v).expect("запись");
    out.replace("\r\n ", "")
        .lines()
        .map(|l| {
            // Порядок параметров не важен.
            let l = l.to_uppercase();
            let (head, value) = l.split_once(':').unwrap_or((&l, ""));
            let mut params: Vec<&str> = head.split(';').collect();
            params[1..].sort_unstable();
            format!("{}:{value}", params.join(";"))
        })
        .collect()
}

#[test]
fn roundtrip_keeps_fields() {
    for card in CORPUS {
        let own = parse(card).version().expect("версия");
        let other = if own == VCardVersion::V3_0 {
            VCardVersion::V4_0
        } else {
            VCardVersion::V3_0
        };
        let there = convert(&parse(card), other).expect("туда");
        assert_eq!(parse(&there).version(), Some(other));
        let back = convert(&parse(&there), own).expect("обратно");
        let (want, got) = (lines(card, own), lines(&back, own));
        let lost: Vec<_> = want.difference(&got).collect();
        assert!(lost.is_empty(), "потеряно {lost:?}\n{back}");
        // Число свойств не меняется (в 3.0 может добавиться обязательное N).
        let (a, b) = (parse(card).entries.len(), parse(&there).entries.len());
        assert!(b == a || b == a + 1, "{a} → {b}\n{there}");
    }
}

#[test]
fn conversion_details() {
    let v4 = convert(&parse(APPLE), VCardVersion::V4_0)
        .expect("4.0")
        .replace("\r\n ", "");
    for part in [
        "VERSION:4.0",
        "EMAIL;TYPE=INTERNET,WORK;PREF=1:ivan@example.org",
        "PHOTO:data:image/jpeg;base64\\,/9j/4AAQSkZJRgABAQ==",
        "item2.X-ABLabel:_$!<HomePage>!$_",
        "N:Петров;Иван;Сергеевич;г-н;",
    ] {
        assert!(v4.contains(part), "{part}\n{v4}");
    }
    assert!(!v4.to_lowercase().contains("type=pref"), "{v4}");

    let group = convert(&parse(APPLE_GROUP), VCardVersion::V4_0).expect("4.0");
    assert!(group.contains("KIND:GROUP\r\n"), "{group}");
    assert!(group.contains("MEMBER:urn:uuid:c-maria\r\n"), "{group}");
    assert!(!group.contains("X-ADDRESSBOOKSERVER"), "{group}");

    let v3 = convert(&parse(V4), VCardVersion::V3_0)
        .expect("3.0")
        .replace("\r\n ", "");
    for part in [
        "VERSION:3.0",
        "EMAIL;TYPE=pref,WORK:maria@example.org",
        "PHOTO;ENCODING=b;TYPE=PNG:iVBORw0KGgo=",
        "BDAY:1990-02-15",
    ] {
        assert!(v3.contains(part), "{part}\n{v3}");
    }
    assert!(!v3.contains("PREF=1"), "{v3}");

    let group = convert(&parse(V4_GROUP), VCardVersion::V3_0).expect("3.0");
    assert!(
        group.contains("X-ADDRESSBOOKSERVER-KIND:group\r\n"),
        "{group}"
    );
    assert!(
        group.contains("X-ADDRESSBOOKSERVER-MEMBER:urn:uuid:c-ivan\r\n"),
        "{group}"
    );
    // vCard 3.0 требует N.
    assert!(
        group.contains("\r\nN;CHARSET=UTF-8:Проект;;;;\r\n"),
        "{group}"
    );
}

async fn env_with(cards: &[(&str, &str)]) -> Env {
    let env = Env::new();
    env.propfind(BOOKS, "0", "").await;
    for (name, body) in cards {
        let r = env
            .send("PUT", &format!("{BOOKS}contacts/{name}"), &[], body)
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{name}: {}", r.body);
    }
    env
}

#[tokio::test]
async fn get_by_accept() {
    let env = env_with(&[("apple.vcf", APPLE), ("v4.vcf", V4)]).await;
    let get = |name: &'static str, accept: &'static str| {
        let env = &env;
        async move {
            let headers: &[(&str, &str)] = if accept.is_empty() {
                &[]
            } else {
                &[("accept", accept)]
            };
            let r = env
                .send("GET", &format!("{BOOKS}contacts/{name}"), headers, "")
                .await;
            assert_eq!(r.status, StatusCode::OK);
            assert_eq!(
                r.header("content-length"),
                Some(r.body.len().to_string().as_str())
            );
            r.body
        }
    };
    // Без версии в Accept — как принято (хранится принятая версия).
    assert_eq!(get("apple.vcf", "").await, APPLE);
    assert_eq!(get("v4.vcf", "text/vcard").await, V4);
    assert_eq!(get("v4.vcf", "text/vcard; version=4.0").await, V4);
    assert_eq!(
        get("apple.vcf", "text/vcard;version=3.0, */*;q=0.1").await,
        APPLE
    );
    // Неподдерживаемая версия — как хранится.
    assert_eq!(get("v4.vcf", "text/vcard; version=2.1").await, V4);
    let v3 = get("v4.vcf", "text/x-vcard, text/vcard; version=3.0").await;
    assert!(v3.starts_with("BEGIN:VCARD\r\nVERSION:3.0\r\n"), "{v3}");
    assert!(
        v3.contains("PHOTO;ENCODING=b;TYPE=PNG:iVBORw0KGgo="),
        "{v3}"
    );
    let v4 = get("apple.vcf", "text/vcard; version=\"4.0\"").await;
    assert!(v4.starts_with("BEGIN:VCARD\r\nVERSION:4.0\r\n"), "{v4}");
    assert!(v4.contains("UID:c-ivan"), "{v4}");
}

#[tokio::test]
async fn address_data_version() {
    let env = env_with(&[("group.vcf", V4_GROUP), ("sales.vcf", APPLE_GROUP)]).await;
    let report = |attrs: &'static str, props: &'static str| {
        let env = &env;
        async move {
            let body = format!(
                r#"<card:addressbook-multiget xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
                <d:prop><d:getetag/><card:address-data {attrs}>{props}</card:address-data></d:prop>
                <d:href>{BOOKS}contacts/group.vcf</d:href><d:href>{BOOKS}contacts/sales.vcf</d:href>
                </card:addressbook-multiget>"#
            );
            env.send("REPORT", &format!("{BOOKS}contacts/"), &[], &body)
                .await
        }
    };
    let r = report(r#"content-type="text/vcard" version="3.0""#, "").await;
    insta::assert_snapshot!("multiget_v3", r.show());
    let r = report(
        r#"version="4.0""#,
        r#"<card:prop name="KIND"/><card:prop name="MEMBER"/>"#,
    )
    .await;
    insta::assert_snapshot!("multiget_v4_props", r.show());
    for attrs in [
        r#"version="2.1""#,
        r#"content-type="application/vcard+json""#,
    ] {
        let r = report(attrs, "").await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{attrs}");
        assert!(
            r.body.contains("<card:supported-address-data/>"),
            "{}",
            r.body
        );
    }
    // addressbook-query — так же.
    let q = r#"<card:addressbook-query xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
        <d:prop><card:address-data version="4.0"/></d:prop>
        <card:filter><card:prop-filter name="FN"><card:text-match>продаж</card:text-match></card:prop-filter></card:filter>
        </card:addressbook-query>"#;
    let r = env
        .send("REPORT", &format!("{BOOKS}contacts/"), &[], q)
        .await;
    assert!(r.body.contains("VERSION:4.0"), "{}", r.body);
    assert!(r.body.contains("KIND:GROUP"), "{}", r.body);
}

#[tokio::test]
async fn put_preconditions() {
    let cfg = DavConfig {
        max_photo_size: 64,
        max_object_size: 4096,
        ..Default::default()
    };
    let env = Env::with_config(cfg);
    env.propfind(BOOKS, "0", "").await;
    let put = |name: &'static str, body: String| {
        let env = &env;
        async move {
            env.send("PUT", &format!("{BOOKS}contacts/{name}"), &[], &body)
                .await
        }
    };
    let photo = |len: usize, v4: bool| {
        let data = STANDARD.encode(vec![0xAB_u8; len]);
        if v4 {
            format!(
                "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:p4-{len}\r\nFN:Фото\r\nPHOTO:data:image/png;base64,{data}\r\nEND:VCARD\r\n"
            )
        } else {
            format!(
                "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:p-{len}\r\nFN:Фото\r\nN:Фото;;;;\r\nPHOTO;ENCODING=b;TYPE=PNG:{data}\r\nEND:VCARD\r\n"
            )
        }
    };
    let rejected = [
        ("не vCard".to_owned(), "<card:valid-address-data/>"),
        (super::event("e-1", "Событие"), "<card:valid-address-data/>"),
        (
            "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Без UID\r\nEND:VCARD\r\n".to_owned(),
            "<card:valid-address-data/>",
        ),
        (
            "BEGIN:VCARD\r\nVERSION:2.1\r\nUID:old\r\nFN:Old\r\nN:Old;;;;\r\nEND:VCARD\r\n"
                .to_owned(),
            "<card:supported-address-data/>",
        ),
        (
            "BEGIN:VCARD\r\nUID:nov\r\nFN:Без версии\r\nEND:VCARD\r\n".to_owned(),
            "<card:supported-address-data/>",
        ),
        (photo(65, false), "<card:max-resource-size/>"),
        (photo(65, true), "<card:max-resource-size/>"),
        (
            format!(
                "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:big\r\nFN:Big\r\nNOTE:{}\r\nEND:VCARD\r\n",
                "x".repeat(5000)
            ),
            "<card:max-resource-size/>",
        ),
    ];
    for (body, want) in rejected {
        let r = put("x.vcf", body.clone()).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{want}: {body}");
        assert!(r.body.contains(want), "{want}: {}", r.body);
    }
    // Фото на границе лимита и ссылка на фото — принимаются.
    for (name, body) in [
        ("p3.vcf", photo(64, false)),
        ("p4.vcf", photo(64, true)),
        (
            "url.vcf",
            "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:url\r\nFN:Ссылка\r\nPHOTO:https://example.org/p.png\r\nEND:VCARD\r\n"
                .to_owned(),
        ),
    ] {
        let r = put(name, body).await;
        assert_eq!(r.status, StatusCode::CREATED, "{name}: {}", r.body);
    }
    // no-uid-conflict: тот же UID в другом ресурсе книги.
    assert_eq!(
        put("a.vcf", vcard("c-1", "Иван")).await.status,
        StatusCode::CREATED
    );
    let r = put("b.vcf", vcard("c-1", "Иван")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(
        r.body.contains(&format!(
            "<card:no-uid-conflict><d:href>{BOOKS}contacts/a.vcf</d:href></card:no-uid-conflict>"
        )),
        "{}",
        r.body
    );
    // max-image-size опубликован.
    let r = env.propfind(&format!("{BOOKS}contacts/"), "0", r#"<d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav"><d:prop><card:max-image-size/><card:max-resource-size/></d:prop></d:propfind>"#).await;
    assert!(
        r.body
            .contains("<card:max-image-size>64</card:max-image-size>"),
        "{}",
        r.body
    );
    assert!(
        r.body
            .contains("<card:max-resource-size>4096</card:max-resource-size>"),
        "{}",
        r.body
    );
}
