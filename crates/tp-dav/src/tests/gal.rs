//! DAV-12: глобальная адресная книга «Организация» на in-memory каталоге: состав, скрытые из GAL (группы и учётные
//! записи — DAV-27), запрет записи, синхронизация по версии снимка каталога (изменения от прежнего снимка — DAV-28).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use tp_contracts::auth::{DirAccount, DirGroup, DirOrg};
use tp_contracts::groups::{DistributionGroup, GroupStore, SenderRestriction};
use tp_contracts::ids::GroupId;
use tp_testkit::{FakeAuthenticator, MemDav};

use super::{BOOKS, Env, vcard};
use crate::{DavConfig, Deps, router};

fn account(id: &str, name: &str, emails: &[&str], enabled: bool) -> DirAccount {
    DirAccount {
        external_id: id.to_owned(),
        login: emails.first().copied().unwrap_or(id).to_owned(),
        display_name: name.to_owned(),
        emails: emails.iter().map(|e| (*e).to_owned()).collect(),
        enabled,
        attributes: BTreeMap::new(),
        org: DirOrg::default(),
        hidden_from_gal: false,
    }
}

fn group(id: &str, name: &str, email: Option<&str>) -> DirGroup {
    DirGroup {
        external_id: id.to_owned(),
        name: name.to_owned(),
        email: email.map(str::to_owned),
        members: vec!["u-ivan".to_owned()],
    }
}

/// Сервер без хранения снимка в памяти (каждый запрос видит текущий каталог) и каталог: Иван (два адреса,
/// телефоны), Мария, отключённый Пётр, учётная запись без адреса; группы «Продажи», скрытая «Руководство» и группа
/// без адреса.
async fn env() -> Env {
    let env = Env::with_config(DavConfig {
        gal_cache: Duration::ZERO,
        gal_page_size: 2,
        ..Default::default()
    });
    let t = env.scope.tenant;
    let mut ivan = account(
        "u-ivan",
        "Иван Петров",
        &["ivan@example.org", "i.petrov@example.org"],
        true,
    );
    ivan.attributes = BTreeMap::from([
        ("telephoneNumber".to_owned(), "+7 495 123-45-67".to_owned()),
        ("mobile".to_owned(), "+7 916 000-11-22".to_owned()),
        ("title".to_owned(), "Бухгалтер".to_owned()),
    ]);
    env.dir.upsert_account(t, ivan);
    env.dir.upsert_account(
        t,
        account("u-maria", "Мария Иванова", &["maria@example.org"], true),
    );
    env.dir.upsert_account(
        t,
        account("u-petr", "Пётр Сидоров", &["petr@example.org"], false),
    );
    env.dir
        .upsert_account(t, account("u-svc", "Служебная", &[], true));
    env.dir
        .upsert_group(t, group("g-sales", "Продажи", Some("sales@example.org")));
    env.dir.upsert_group(
        t,
        group("g-board", "Руководство", Some("board@example.org")),
    );
    env.dir
        .upsert_group(t, group("g-acl", "Доступ к складу", None));
    env.groups
        .put(DistributionGroup {
            id: GroupId::new(),
            tenant: t,
            name: "Руководство".to_owned(),
            address: Some("board@example.org".to_owned()),
            external_id: Some("g-board".to_owned()),
            members: Vec::new(),
            senders: SenderRestriction::Anyone,
            moderators: Vec::new(),
            hidden_from_gal: true,
        })
        .await
        .expect("группа");
    env
}

fn gal() -> String {
    format!("{BOOKS}gal/")
}

/// Имена ресурсов (последний сегмент ссылок) ответа.
fn names(body: &str) -> Vec<String> {
    body.split("<d:href>")
        .skip(1)
        .filter_map(|s| s.split("</d:href>").next())
        .filter_map(|h| h.rsplit('/').next())
        .filter(|n| n.ends_with(".vcf"))
        .map(str::to_owned)
        .collect()
}

fn token(body: &str) -> String {
    body.split("<d:sync-token>")
        .nth(1)
        .and_then(|s| s.split("</d:sync-token>").next())
        .expect("sync-token")
        .to_owned()
}

fn sync_body(token: &str) -> String {
    format!(
        r#"<d:sync-collection xmlns:d="DAV:"><d:sync-token>{token}</d:sync-token><d:sync-level>1</d:sync-level>
        <d:prop><d:getetag/></d:prop></d:sync-collection>"#
    )
}

const ETAGS: &str = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:getetag/></d:prop></d:propfind>"#;

#[tokio::test]
async fn home_lists_gal_snapshot() {
    let env = env().await;
    let body = r#"<d:propfind xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
      <d:prop><d:resourcetype/><d:displayname/><d:current-user-privilege-set/><d:supported-report-set/>
      <card:supported-address-data/></d:prop></d:propfind>"#;
    let r = env.propfind(BOOKS, "1", body).await;
    insta::assert_snapshot!("home", r.show());
}

#[tokio::test]
async fn composition() {
    let env = env().await;
    let r = env.propfind(&gal(), "1", ETAGS).await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    // Отключённый, без адреса, скрытая группа и группа без адреса не входят.
    assert_eq!(names(&r.body), ["g-sales.vcf", "u-ivan.vcf", "u-maria.vcf"]);

    let r = env
        .send("GET", &format!("{}u-ivan.vcf", gal()), &[], "")
        .await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.header("content-type"), Some("text/vcard; charset=utf-8"));
    insta::assert_snapshot!("account_card", r.body.replace("\r\n", "\n"));
    let r = env
        .send(
            "GET",
            &format!("{}g-sales.vcf", gal()),
            &[("accept", "text/vcard; version=4.0")],
            "",
        )
        .await;
    insta::assert_snapshot!("group_card_v4", r.body.replace("\r\n", "\n"));
    for missing in ["u-petr.vcf", "g-board.vcf", "none.vcf"] {
        let r = env
            .send("GET", &format!("{}{missing}", gal()), &[], "")
            .await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "{missing}");
    }
}

#[tokio::test]
async fn reports() {
    let env = env().await;
    let q = r#"<card:addressbook-query xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
      <d:prop><d:getetag/><card:address-data/></d:prop>
      <card:filter><card:prop-filter name="EMAIL"><card:text-match match-type="starts-with">i.petrov@</card:text-match></card:prop-filter></card:filter>
    </card:addressbook-query>"#;
    let r = env.send("REPORT", &gal(), &[], q).await;
    assert_eq!(names(&r.body), ["u-ivan.vcf"], "{}", r.body);
    assert!(
        r.body.contains("FN;CHARSET=UTF-8:Иван Петров"),
        "{}",
        r.body
    );

    let m = format!(
        r#"<card:addressbook-multiget xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
      <d:prop><d:getetag/></d:prop><d:href>{0}u-maria.vcf</d:href><d:href>{0}g-board.vcf</d:href>
    </card:addressbook-multiget>"#,
        gal()
    );
    let r = env.send("REPORT", &gal(), &[], &m).await;
    assert_eq!(r.body.matches("200 OK").count(), 1, "{}", r.body);
    assert_eq!(r.body.matches("404 Not Found").count(), 1, "{}", r.body);
}

#[tokio::test]
async fn read_only() {
    let env = env().await;
    let card = format!("{}new.vcf", gal());
    let patch = r#"<d:propertyupdate xmlns:d="DAV:"><d:set><d:prop><d:displayname>Моя</d:displayname></d:prop></d:set></d:propertyupdate>"#;
    let requests = [
        ("PUT", card.clone(), vcard("c-1", "Новый")),
        (
            "PUT",
            format!("{}u-ivan.vcf", gal()),
            vcard("u-ivan", "Иван"),
        ),
        ("DELETE", format!("{}u-ivan.vcf", gal()), String::new()),
        ("DELETE", gal(), String::new()),
        ("PROPPATCH", gal(), patch.to_owned()),
        ("MKCOL", gal(), String::new()),
    ];
    for (method, path, body) in requests {
        let r = env.send(method, &path, &[], &body).await;
        assert_eq!(r.status, StatusCode::FORBIDDEN, "{method} {path}");
        assert!(
            r.body.contains("<d:need-privileges>") && r.body.contains("<d:write/>"),
            "{method} {path}: {}",
            r.body
        );
    }
    let r = env.propfind(&gal(), "1", ETAGS).await;
    assert_eq!(names(&r.body).len(), 3);
    // Права книги и карточек — только чтение.
    let privs = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-privilege-set/></d:prop></d:propfind>"#;
    let r = env.propfind(&gal(), "1", privs).await;
    assert!(!r.body.contains("<d:write/>"), "{}", r.body);
    assert_eq!(r.body.matches("<d:read/>").count(), 4, "{}", r.body);
}

#[tokio::test]
async fn sync_by_snapshot_version() {
    let env = env().await;
    let t = env.scope.tenant;
    let r = env.send("REPORT", &gal(), &[], &sync_body("")).await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert_eq!(names(&r.body).len(), 3);
    let first = token(&r.body);
    // ETag по имени карточки.
    let etags = |body: &str| -> BTreeMap<String, String> {
        body.split("<d:response>")
            .skip(1)
            .filter_map(|r| {
                let name = names(r).pop()?;
                let etag = r
                    .split("<d:getetag>")
                    .nth(1)?
                    .split("</d:getetag>")
                    .next()?;
                Some((name, etag.to_owned()))
            })
            .collect()
    };
    let before = etags(&r.body);
    assert_eq!(before.len(), 3);

    // Каталог не менялся: токен тот же, изменений нет.
    let r = env.send("REPORT", &gal(), &[], &sync_body(&first)).await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert!(names(&r.body).is_empty(), "{}", r.body);
    assert_eq!(token(&r.body), first);

    // Изменение каталога меняет токен и getctag; от прежнего токена (DAV-28) — изменённая и новая карточки и
    // удалённая (404). ETag меняется только у изменённой карточки.
    env.dir.upsert_account(
        t,
        account("u-ivan", "Иван Петров", &["ivan.petrov@example.org"], true),
    );
    env.dir.upsert_account(
        t,
        account("u-olga", "Ольга Смирнова", &["olga@example.org"], true),
    );
    env.dir.remove_account(t, "u-maria");
    let r = env.send("REPORT", &gal(), &[], &sync_body(&first)).await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert_eq!(
        names(&r.body),
        ["u-ivan.vcf", "u-olga.vcf", "u-maria.vcf"],
        "{}",
        r.body
    );
    assert_eq!(r.body.matches("404 Not Found").count(), 1, "{}", r.body);
    let delta_token = token(&r.body);
    let r = env.send("REPORT", &gal(), &[], &sync_body("")).await;
    assert_eq!(token(&r.body), delta_token);
    assert_eq!(names(&r.body), ["g-sales.vcf", "u-ivan.vcf", "u-olga.vcf"]);
    let second = token(&r.body);
    assert_ne!(second, first);
    let after = etags(&r.body);
    assert_eq!(after["g-sales.vcf"], before["g-sales.vcf"]);
    assert_ne!(after["u-ivan.vcf"], before["u-ivan.vcf"]);

    // Скрытие группы в хранилище групп — тоже новый снимок.
    let mut g = env
        .groups
        .by_external_id(t, "g-board")
        .await
        .expect("хранилище")
        .expect("группа");
    g.hidden_from_gal = false;
    env.groups.put(g).await.expect("группа");
    let r = env.send("REPORT", &gal(), &[], &sync_body(&second)).await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert_eq!(names(&r.body), ["g-board.vcf"], "{}", r.body);
    let r = env.propfind(&gal(), "1", ETAGS).await;
    assert!(names(&r.body).contains(&"g-board.vcf".to_owned()));
    // Токен первого снимка — ещё в истории: изменения за два снимка.
    let r = env.send("REPORT", &gal(), &[], &sync_body(&first)).await;
    assert_eq!(
        names(&r.body),
        ["g-board.vcf", "u-ivan.vcf", "u-olga.vcf", "u-maria.vcf"],
        "{}",
        r.body
    );
}

/// DAV-28: изменение одной записи каталога меняет ETag одной карточки — `sync-collection` от прежнего токена
/// отдаёт одну; токен старше `dav.gal_sync_history` снимков — 403 `valid-sync-token`.
#[tokio::test]
async fn sync_changed_card_only() {
    let env = Env::with_config(DavConfig {
        gal_cache: Duration::ZERO,
        gal_sync_history: 1,
        ..Default::default()
    });
    let t = env.scope.tenant;
    for (id, name, email) in [
        ("u-1", "Первый", "one@example.org"),
        ("u-2", "Второй", "two@example.org"),
        ("u-3", "Третий", "three@example.org"),
    ] {
        env.dir.upsert_account(t, account(id, name, &[email], true));
    }
    let r = env.send("REPORT", &gal(), &[], &sync_body("")).await;
    assert_eq!(names(&r.body).len(), 3);
    let first = token(&r.body);
    env.dir.upsert_account(
        t,
        account("u-2", "Второй Иванов", &["two@example.org"], true),
    );
    let r = env.send("REPORT", &gal(), &[], &sync_body(&first)).await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert_eq!(names(&r.body), ["u-2.vcf"], "{}", r.body);
    assert!(!r.body.contains("404 Not Found"), "{}", r.body);
    let second = token(&r.body);
    assert_ne!(second, first);

    // Ещё одно изменение: первый токен вышел из истории (один снимок).
    env.dir.remove_account(t, "u-3");
    let r = env.send("REPORT", &gal(), &[], &sync_body(&first)).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("<d:valid-sync-token/>"), "{}", r.body);
    let r = env.send("REPORT", &gal(), &[], &sync_body(&second)).await;
    assert_eq!(names(&r.body), ["u-3.vcf"], "{}", r.body);
    assert_eq!(r.body.matches("404 Not Found").count(), 1, "{}", r.body);
}

#[tokio::test]
async fn cached_snapshot() {
    // Со сроком хранения по умолчанию изменение каталога видно не сразу.
    let env = Env::new();
    let t = env.scope.tenant;
    env.dir
        .upsert_account(t, account("u-1", "Первый", &["one@example.org"], true));
    let r = env.propfind(&gal(), "1", ETAGS).await;
    assert_eq!(names(&r.body), ["u-1.vcf"]);
    env.dir
        .upsert_account(t, account("u-2", "Второй", &["two@example.org"], true));
    let r = env.propfind(&gal(), "1", ETAGS).await;
    assert_eq!(names(&r.body), ["u-1.vcf"]);
}

#[tokio::test]
async fn no_directory_no_gal() {
    let env = Env::new();
    let app = router(
        Deps {
            auth: Arc::new(env.auth.clone()),
            store: Arc::new(MemDav::new()),
        },
        DavConfig::default(),
    );
    let env = Env { app, ..env };
    let _: &FakeAuthenticator = &env.auth;
    let r = env.propfind(BOOKS, "1", "").await;
    assert!(!r.body.contains("/gal/"), "{}", r.body);
    assert_eq!(
        env.propfind(&gal(), "0", "").await.status,
        StatusCode::NOT_FOUND
    );
    // Имя зарезервировано и без каталога.
    let r = env.send("MKCOL", &gal(), &[], "").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

/// DAV-27: учётная запись, скрытая из адресной книги (`hidden_from_gal`), не видна в `addressbook-query` и
/// `sync-collection`; после снятия признака — появляется.
#[tokio::test]
async fn hidden_account() {
    let env = env().await;
    let t = env.scope.tenant;
    let mut maria = account("u-maria", "Мария Иванова", &["maria@example.org"], true);
    maria.hidden_from_gal = true;
    env.dir.upsert_account(t, maria.clone());
    let query = r#"<card:addressbook-query xmlns:d="DAV:" xmlns:card="urn:ietf:params:xml:ns:carddav">
      <d:prop><d:getetag/></d:prop>
      <card:filter><card:prop-filter name="FN"><card:text-match>Мария</card:text-match></card:prop-filter></card:filter>
    </card:addressbook-query>"#;
    let r = env.send("REPORT", &gal(), &[], query).await;
    assert!(names(&r.body).is_empty(), "{}", r.body);
    let r = env.send("REPORT", &gal(), &[], &sync_body("")).await;
    assert_eq!(names(&r.body), ["g-sales.vcf", "u-ivan.vcf"]);
    let r = env
        .send("GET", &format!("{}u-maria.vcf", gal()), &[], "")
        .await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);

    maria.hidden_from_gal = false;
    env.dir.upsert_account(t, maria);
    let r = env.send("REPORT", &gal(), &[], query).await;
    assert_eq!(names(&r.body), ["u-maria.vcf"], "{}", r.body);
    let r = env.send("REPORT", &gal(), &[], &sync_body("")).await;
    assert_eq!(names(&r.body), ["g-sales.vcf", "u-ivan.vcf", "u-maria.vcf"]);
}
