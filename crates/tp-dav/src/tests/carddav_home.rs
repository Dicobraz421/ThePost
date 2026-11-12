//! DAV-09: дом адресных книг (книга «Контакты» по умолчанию), свойства адресной книги для Thunderbird, Контактов
//! iOS и macOS, DAVx5 (снимки ответов).

use axum::http::StatusCode;
use tp_contracts::dav::{Collection, CollectionKind, DavStore};

use super::{BOOKS, Env, LOGIN};

/// Thunderbird: поиск адресных книг в доме (`Depth: 1`).
const THUNDERBIRD_HOME: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<propfind xmlns="DAV:" xmlns:CAL="urn:ietf:params:xml:ns:caldav" xmlns:CARD="urn:ietf:params:xml:ns:carddav" xmlns:CS="http://calendarserver.org/ns/">
  <prop>
    <resourcetype/>
    <displayname/>
    <current-user-privilege-set/>
    <CS:getctag/>
  </prop>
</propfind>"#;

/// Контакты iOS и macOS (AddressBookCore): дом адресных книг (`Depth: 1`).
const APPLE_HOME: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<A:propfind xmlns:A="DAV:">
  <A:prop>
    <A:add-member/>
    <A:current-user-privilege-set/>
    <A:displayname/>
    <D:max-image-size xmlns:D="urn:ietf:params:xml:ns:carddav"/>
    <D:max-resource-size xmlns:D="urn:ietf:params:xml:ns:carddav"/>
    <C:me-card xmlns:C="http://calendarserver.org/ns/"/>
    <A:owner/>
    <C:push-transports xmlns:C="http://calendarserver.org/ns/"/>
    <C:pushkey xmlns:C="http://calendarserver.org/ns/"/>
    <A:quota-available-bytes/>
    <A:quota-used-bytes/>
    <A:resource-id/>
    <A:resourcetype/>
    <A:supported-report-set/>
    <A:sync-token/>
  </A:prop>
</A:propfind>"#;

/// DAVx5: принципал — поиск дома адресных книг (`Depth: 0`).
const DAVX5_PRINCIPAL: &str = r#"<?xml version='1.0' encoding='UTF-8' ?>
<propfind xmlns="DAV:" xmlns:CARD="urn:ietf:params:xml:ns:carddav">
  <prop>
    <CARD:addressbook-home-set/>
    <current-user-principal/>
    <displayname/>
  </prop>
</propfind>"#;

/// DAVx5: свойства адресной книги (`Depth: 0`).
const DAVX5_BOOK: &str = r#"<?xml version='1.0' encoding='UTF-8' ?>
<propfind xmlns="DAV:" xmlns:CARD="urn:ietf:params:xml:ns:carddav" xmlns:CS="http://calendarserver.org/ns/">
  <prop>
    <resourcetype/>
    <displayname/>
    <CARD:addressbook-description/>
    <CARD:supported-address-data/>
    <CARD:max-resource-size/>
    <current-user-privilege-set/>
    <owner/>
    <supported-report-set/>
    <CS:getctag/>
    <sync-token/>
  </prop>
</propfind>"#;

#[tokio::test]
async fn thunderbird_home_snapshot() {
    let env = Env::new();
    let r = env.propfind(BOOKS, "1", THUNDERBIRD_HOME).await;
    insta::assert_snapshot!("thunderbird_home", r.show());
}

#[tokio::test]
async fn apple_home_snapshot() {
    let env = Env::new();
    let r = env.propfind(BOOKS, "1", APPLE_HOME).await;
    insta::assert_snapshot!("apple_home", r.show());
}

#[tokio::test]
async fn davx5_snapshots() {
    let env = Env::new();
    let r = env
        .propfind(&format!("/dav/principals/{LOGIN}/"), "0", DAVX5_PRINCIPAL)
        .await;
    insta::assert_snapshot!("davx5_principal", r.show());
    env.propfind(BOOKS, "0", "").await;
    let patch = r#"<d:propertyupdate xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
      <d:set><d:prop><card:addressbook-description>Личные контакты</card:addressbook-description></d:prop></d:set>
    </d:propertyupdate>"#;
    let r = env
        .send("PROPPATCH", &format!("{BOOKS}contacts/"), &[], patch)
        .await;
    assert!(r.body.contains("200 OK"), "{}", r.body);
    let r = env
        .propfind(&format!("{BOOKS}contacts/"), "0", DAVX5_BOOK)
        .await;
    insta::assert_snapshot!("davx5_book", r.show());
}

#[tokio::test]
async fn default_created_once() {
    let env = Env::new();
    let list = || async {
        env.store
            .list_collections(env.scope, CollectionKind::AddressBook)
            .await
            .expect("список")
    };
    assert!(list().await.is_empty());
    for depth in ["0", "1", "1"] {
        let r = env.propfind(BOOKS, depth, "").await;
        assert_eq!(r.status, StatusCode::MULTI_STATUS);
    }
    let names: Vec<_> = list()
        .await
        .into_iter()
        .map(|c| (c.name, c.display_name))
        .collect();
    assert_eq!(names, [("contacts".to_owned(), "Контакты".to_owned())]);
    // Календари по умолчанию обращением к дому адресных книг не создаются.
    let cals = env
        .store
        .list_collections(env.scope, CollectionKind::Calendar)
        .await
        .expect("список");
    assert!(cals.is_empty());
}

#[tokio::test]
async fn existing_books_keep_home_as_is() {
    let env = Env::new();
    env.create(Collection::new(CollectionKind::AddressBook, "team"))
        .await;
    let r = env.propfind(BOOKS, "1", "").await;
    assert!(r.body.contains("/team/"), "{}", r.body);
    assert!(!r.body.contains("/contacts/"), "{}", r.body);
}
