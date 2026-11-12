//! DAV-10: `addressbook-query` (фильтры, `limit`, выбор свойств `address-data`) и `addressbook-multiget` на наборе
//! карточек: кириллица, несколько адресов и телефонов.

use axum::http::StatusCode;
use calcard::vcard::VCard;
use tp_contracts::dav::{CollectionKind, DavStore, TimeRange};

use super::{BOOKS, Env};
use crate::DavConfig;
use crate::card::Filter;
use crate::xml::CARDDAV;

const IVAN: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:c-ivan\r\nFN:Иван Петров\r\nN:Петров;Иван;Сергеевич;;\r\n\
    EMAIL;TYPE=INTERNET,WORK:ivan.petrov@example.org\r\nEMAIL;TYPE=INTERNET,HOME:vanya@mail.example\r\n\
    TEL;TYPE=WORK,VOICE:+7 495 123-45-67\r\nTEL;TYPE=CELL:+7 916 000-11-22\r\nORG:ООО Ромашка;Бухгалтерия\r\n\
    END:VCARD\r\n";

const MARIA: &str = "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:c-maria\r\nFN:Мария Иванова\r\nN:Иванова;Мария;;;\r\n\
    EMAIL;TYPE=work:maria@example.org\r\nTEL;TYPE=cell;VALUE=uri:tel:+7-903-555-66-77\r\nNICKNAME:Маша\r\n\
    END:VCARD\r\n";

const JOHN: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:c-john\r\nFN:John Smith\r\nN:Smith;John;;;\r\n\
    EMAIL;TYPE=INTERNET:john@partner.example\r\nEND:VCARD\r\n";

const NOMAIL: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:c-nomail\r\nFN:Склад\r\nN:Склад;;;;\r\nTEL;TYPE=WORK:+7 495 765-43-21\r\nEND:VCARD\r\n";

/// Книга `contacts` с четырьмя карточками.
async fn env() -> Env {
    let env = Env::new();
    env.propfind(BOOKS, "0", "").await;
    for (name, body) in [
        ("ivan.vcf", IVAN),
        ("maria.vcf", MARIA),
        ("john.vcf", JOHN),
        ("nomail.vcf", NOMAIL),
    ] {
        let r = env
            .send("PUT", &format!("{BOOKS}contacts/{name}"), &[], body)
            .await;
        assert_eq!(r.status, StatusCode::CREATED, "{name}");
    }
    env
}

fn query(inner: &str) -> String {
    format!(
        r#"<card:addressbook-query xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
          <d:prop><d:getetag/></d:prop>{inner}</card:addressbook-query>"#
    )
}

/// Имена карточек ответа по порядку.
fn names(body: &str) -> Vec<String> {
    body.split("<d:href>")
        .skip(1)
        .filter_map(|s| s.split("</d:href>").next())
        .filter_map(|h| h.rsplit('/').next())
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .collect()
}

async fn found(env: &Env, filter: &str) -> Vec<String> {
    let r = env
        .send("REPORT", &format!("{BOOKS}contacts/"), &[], &query(filter))
        .await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS, "{filter}: {}", r.body);
    let mut n = names(&r.body);
    n.sort();
    n
}

#[tokio::test]
async fn text_match_types() {
    let env = env().await;
    let cases: [(&str, &[&str]); 9] = [
        // Без условий — все.
        (
            "<card:filter/>",
            &["ivan.vcf", "john.vcf", "maria.vcf", "nomail.vcf"],
        ),
        // Подстрока без учёта регистра, кириллица (сравнение по умолчанию — i;unicode-casemap).
        (
            r#"<card:filter><card:prop-filter name="FN"><card:text-match>ИВАН</card:text-match></card:prop-filter></card:filter>"#,
            &["ivan.vcf", "maria.vcf"],
        ),
        (
            r#"<card:filter><card:prop-filter name="FN"><card:text-match match-type="starts-with">иван</card:text-match></card:prop-filter></card:filter>"#,
            &["ivan.vcf"],
        ),
        (
            r#"<card:filter><card:prop-filter name="FN"><card:text-match match-type="ends-with">иванова</card:text-match></card:prop-filter></card:filter>"#,
            &["maria.vcf"],
        ),
        (
            r#"<card:filter><card:prop-filter name="FN"><card:text-match match-type="equals">john smith</card:text-match></card:prop-filter></card:filter>"#,
            &["john.vcf"],
        ),
        // i;octet — с учётом регистра.
        (
            r#"<card:filter><card:prop-filter name="FN"><card:text-match collation="i;octet" match-type="equals">john smith</card:text-match></card:prop-filter></card:filter>"#,
            &[],
        ),
        // Второй адрес и второй телефон карточки.
        (
            r#"<card:filter><card:prop-filter name="EMAIL"><card:text-match match-type="ends-with">@mail.example</card:text-match></card:prop-filter></card:filter>"#,
            &["ivan.vcf"],
        ),
        (
            r#"<card:filter><card:prop-filter name="TEL"><card:text-match>916 000</card:text-match></card:prop-filter></card:filter>"#,
            &["ivan.vcf"],
        ),
        // Составное значение: часть и запись целиком.
        (
            r#"<card:filter><card:prop-filter name="N"><card:text-match match-type="starts-with">Петров;Иван</card:text-match></card:prop-filter></card:filter>"#,
            &["ivan.vcf"],
        ),
    ];
    for (filter, want) in cases {
        assert_eq!(found(&env, filter).await, want, "{filter}");
    }
}

#[tokio::test]
async fn tests_params_and_not_defined() {
    let env = env().await;
    let cases: [(&str, &[&str]); 7] = [
        // anyof (по умолчанию) и allof между prop-filter.
        (
            r#"<card:filter><card:prop-filter name="FN"><card:text-match>smith</card:text-match></card:prop-filter>
               <card:prop-filter name="NICKNAME"><card:text-match>маша</card:text-match></card:prop-filter></card:filter>"#,
            &["john.vcf", "maria.vcf"],
        ),
        (
            r#"<card:filter test="allof"><card:prop-filter name="FN"><card:text-match>иван</card:text-match></card:prop-filter>
               <card:prop-filter name="EMAIL"><card:text-match>maria@</card:text-match></card:prop-filter></card:filter>"#,
            &["maria.vcf"],
        ),
        // Свойства нет.
        (
            r#"<card:filter><card:prop-filter name="EMAIL"><card:is-not-defined/></card:prop-filter></card:filter>"#,
            &["nomail.vcf"],
        ),
        // Свойство есть.
        (
            r#"<card:filter><card:prop-filter name="ORG"/></card:filter>"#,
            &["ivan.vcf"],
        ),
        // param-filter: телефон с TYPE=CELL (регистр значения параметра не важен).
        (
            r#"<card:filter><card:prop-filter name="TEL"><card:param-filter name="TYPE"><card:text-match match-type="equals">cell</card:text-match></card:param-filter></card:prop-filter></card:filter>"#,
            &["ivan.vcf", "maria.vcf"],
        ),
        // allof внутри prop-filter: одно свойство — и текст, и параметр.
        (
            r#"<card:filter><card:prop-filter name="EMAIL" test="allof"><card:text-match>example.org</card:text-match>
               <card:param-filter name="TYPE"><card:text-match>home</card:text-match></card:param-filter></card:prop-filter></card:filter>"#,
            &[],
        ),
        // negate-condition.
        (
            r#"<card:filter><card:prop-filter name="EMAIL"><card:text-match negate-condition="yes">example.org</card:text-match></card:prop-filter></card:filter>"#,
            &["ivan.vcf", "john.vcf"],
        ),
    ];
    for (filter, want) in cases {
        assert_eq!(found(&env, filter).await, want, "{filter}");
    }
}

#[tokio::test]
async fn limit_and_errors() {
    let env = env().await;
    let path = format!("{BOOKS}contacts/");
    let r = env
        .send(
            "REPORT",
            &path,
            &[],
            &query("<card:filter/><card:limit><card:nresults>2</card:nresults></card:limit>"),
        )
        .await;
    assert_eq!(names(&r.body).len(), 2, "{}", r.body);
    assert!(r.body.contains("507 Insufficient Storage"), "{}", r.body);
    assert!(r.body.contains("number-of-matches-within-limits"));

    let bad = [
        (r#"<card:filter test="some"/>"#, StatusCode::BAD_REQUEST, ""),
        (
            r#"<card:filter><card:prop-filter/></card:filter>"#,
            StatusCode::BAD_REQUEST,
            "",
        ),
        (
            r#"<card:filter><card:prop-filter name="FN"><card:text-match match-type="like">x</card:text-match></card:prop-filter></card:filter>"#,
            StatusCode::BAD_REQUEST,
            "",
        ),
        (
            r#"<card:filter><card:prop-filter name="FN"><card:text-match collation="i;other">x</card:text-match></card:prop-filter></card:filter>"#,
            StatusCode::FORBIDDEN,
            "supported-collation",
        ),
        (
            "<card:filter/><card:limit><card:nresults>много</card:nresults></card:limit>",
            StatusCode::BAD_REQUEST,
            "",
        ),
    ];
    for (inner, code, text) in bad {
        let r = env.send("REPORT", &path, &[], &query(inner)).await;
        assert_eq!(r.status, code, "{inner}");
        assert!(r.body.contains(text), "{inner}: {}", r.body);
    }
    // Отчёты CardDAV — только у адресной книги.
    let r = env.send("REPORT", BOOKS, &[], &query("")).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = env
        .send("REPORT", &format!("{BOOKS}none/"), &[], &query(""))
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn address_data_props_snapshot() {
    let env = env().await;
    let body = r#"<card:addressbook-query xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
      <d:prop><card:address-data><card:prop name="UID"/><card:prop name="FN"/><card:prop name="EMAIL"/></card:address-data></d:prop>
      <card:filter><card:prop-filter name="TEL"><card:text-match match-type="starts-with">+7 495</card:text-match></card:prop-filter></card:filter>
    </card:addressbook-query>"#;
    let r = env
        .send("REPORT", &format!("{BOOKS}contacts/"), &[], body)
        .await;
    insta::assert_snapshot!("address_data_props", r.show());
}

#[tokio::test]
async fn multiget_snapshot() {
    let env = env().await;
    let body = format!(
        r#"<card:addressbook-multiget xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
      <d:prop><d:getcontenttype/><card:address-data/></d:prop>
      <d:href>{BOOKS}contacts/john.vcf</d:href>
      <d:href>https://mail.example.org{BOOKS}contacts/maria.vcf</d:href>
      <d:href>{BOOKS}contacts/none.vcf</d:href>
      <d:href>{BOOKS}other/ivan.vcf</d:href>
    </card:addressbook-multiget>"#
    );
    let r = env
        .send("REPORT", &format!("{BOOKS}contacts/"), &[], &body)
        .await;
    insta::assert_snapshot!("multiget", r.show());
}

#[tokio::test]
async fn multiget_limit() {
    let cfg = crate::DavConfig {
        max_multiget_hrefs: 1,
        ..Default::default()
    };
    let env = Env::with_config(cfg);
    env.propfind(BOOKS, "0", "").await;
    let body = format!(
        r#"<card:addressbook-multiget xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
      <d:href>{BOOKS}contacts/a.vcf</d:href><d:href>{BOOKS}contacts/b.vcf</d:href></card:addressbook-multiget>"#
    );
    let r = env
        .send("REPORT", &format!("{BOOKS}contacts/"), &[], &body)
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("number-of-matches-within-limits"));
}

/// DAV-24: `PUT` карточки заполняет поисковые строки индекса (CTR-54): полное имя, части имени, адреса e-mail.
#[tokio::test]
async fn put_fills_search_index() {
    let env = env().await;
    let book = env
        .collection(CollectionKind::AddressBook, "contacts")
        .await
        .expect("книга");
    let o = env
        .store
        .get_object(env.scope, book.id, "ivan.vcf")
        .await
        .expect("хранилище")
        .expect("карточка");
    let strings = &o.index.search.strings;
    for s in [
        "иван петров",
        "петров",
        "иван",
        "сергеевич",
        "ivan.petrov@example.org",
        "vanya@mail.example",
    ] {
        assert!(strings.iter().any(|x| x == s), "{s}: {strings:?}");
    }
    assert!(
        !strings
            .iter()
            .any(|x| x.contains("495") || x.contains("ромашка")),
        "телефоны и организация — не в индексе: {strings:?}"
    );
}

/// DAV-24: `addressbook-query` по части имени и по адресу — кандидаты из индекса (`find_cards`), ответ тот же, что
/// у полного перебора карточек книги.
#[tokio::test]
async fn query_by_index_same_as_full_scan() {
    let env = env().await;
    let book = env
        .collection(CollectionKind::AddressBook, "contacts")
        .await
        .expect("книга");
    let all = env
        .store
        .in_range(env.scope, book.id, TimeRange::default())
        .await
        .expect("карточки");
    let filters = [
        r#"<card:filter><card:prop-filter name="FN"><card:text-match>ИВАН</card:text-match></card:prop-filter></card:filter>"#,
        r#"<card:filter><card:prop-filter name="N"><card:text-match match-type="equals">петров</card:text-match></card:prop-filter></card:filter>"#,
        r#"<card:filter><card:prop-filter name="fn"><card:text-match collation="i;octet" match-type="starts-with">Мар</card:text-match></card:prop-filter></card:filter>"#,
        r#"<card:filter><card:prop-filter name="EMAIL"><card:text-match>EXAMPLE.ORG</card:text-match></card:prop-filter></card:filter>"#,
        r#"<card:filter><card:prop-filter name="EMAIL"><card:text-match collation="i;ascii-casemap" match-type="ends-with">@PARTNER.example</card:text-match></card:prop-filter></card:filter>"#,
        r#"<card:filter><card:prop-filter name="NICKNAME"><card:text-match>маш</card:text-match></card:prop-filter></card:filter>"#,
        r#"<card:filter test="allof"><card:prop-filter name="TEL"/><card:prop-filter name="EMAIL"><card:text-match>example</card:text-match></card:prop-filter></card:filter>"#,
        r#"<card:filter><card:prop-filter name="EMAIL" test="allof"><card:text-match>ivan</card:text-match><card:text-match match-type="ends-with">.org</card:text-match></card:prop-filter></card:filter>"#,
    ];
    for inner in filters {
        let root = crate::xml::parse(query(inner).as_bytes(), DavConfig::default().xml_limits())
            .expect("XML");
        let filter = Filter::parse(root.child(CARDDAV, "filter").expect("filter")).expect("фильтр");
        assert!(filter.needle().is_some(), "кандидаты по индексу: {inner}");
        let mut want: Vec<String> = all
            .iter()
            .filter(|o| VCard::parse(&o.body).is_ok_and(|c| filter.matches(&c)))
            .map(|o| o.name.clone())
            .collect();
        want.sort();
        assert!(!want.is_empty(), "{inner}");
        assert_eq!(found(&env, inner).await, want, "{inner}");
    }
    // Без условия, которое сужает выборку, — полный перебор.
    for inner in [
        r#"<card:filter><card:prop-filter name="FN"><card:text-match negate-condition="yes">иван</card:text-match></card:prop-filter></card:filter>"#,
        r#"<card:filter><card:prop-filter name="TEL"><card:text-match>495</card:text-match></card:prop-filter></card:filter>"#,
        r#"<card:filter><card:prop-filter name="FN"><card:text-match>иван</card:text-match></card:prop-filter><card:prop-filter name="TEL"/></card:filter>"#,
        r#"<card:filter><card:prop-filter name="N"><card:text-match>Петров;Иван</card:text-match></card:prop-filter></card:filter>"#,
    ] {
        let root = crate::xml::parse(query(inner).as_bytes(), DavConfig::default().xml_limits())
            .expect("XML");
        let filter = Filter::parse(root.child(CARDDAV, "filter").expect("filter")).expect("фильтр");
        assert_eq!(filter.needle(), None, "{inner}");
    }
}
