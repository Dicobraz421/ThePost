//! Общий доступ (DAV-17), представители (DAV-18), бронирование ресурсов и поиск принципалов (DAV-19). Хранилище,
//! учётные записи и отправка — в памяти; права на коллекции — `FakeSharingAuthorizer` (таблица прав), записи прав и
//! представители — `MemDav` и `MemMetadata`.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use calcard::icalendar::ICalendar;
use time::Time;
use tp_contracts::auth::Principal;
use tp_contracts::dav::{Collection, CollectionKind, DavStore};
use tp_contracts::meta::{AccountKind, AccountScope, AccountStore, AccountUpsert};
use tp_contracts::schedule::{ItipDelivery, ItipMessage, ItipMethod, ItipOutcome, ItipSource};
use tp_contracts::sharing::{
    Bookers, BookingPolicy, CollectionAcl, CollectionRights, DayOfWeek, DelegateLevel,
    DelegateStore, Delegation, ResourceStore, WorkingHours,
};
use tp_contracts::{DomainId, TenantId};
use tp_testkit::{
    FakeAuthenticator, FakeSharingAuthorizer, ManualClock, MemDav, MemGroups, MemItipSender,
    MemMetadata,
};

use super::{Resp, basic, event};
use crate::{
    BookingDeps, DavConfig, Deps, ItipInbox, ScheduleDeps, SharingStores, itip, router_shared,
};

const ALICE: &str = "alice@example.org";
const BOB: &str = "bob@example.org";
const PETR: &str = "petr@example.org";
const ROOM: &str = "room@example.org";

/// Календарь Алисы, которым она делится.
const WORK: &str = "/dav/calendars/alice@example.org/work/";

struct Share {
    app: Router,
    store: MemDav,
    meta: MemMetadata,
    authz: FakeSharingAuthorizer,
    sender: MemItipSender,
    inbox: ItipInbox,
    accounts: Vec<(String, AccountScope)>,
}

impl Share {
    async fn new() -> Self {
        let tenant = TenantId::new();
        let groups = MemGroups::new();
        let meta = MemMetadata::new(ManualClock::default().shared()).with_groups(groups.clone());
        let auth = FakeAuthenticator::default();
        let mut accounts = Vec::new();
        for (login, name, kind) in [
            (ALICE, "Алиса", AccountKind::User),
            (BOB, "Борис", AccountKind::User),
            (PETR, "Пётр", AccountKind::User),
            (ROOM, "Переговорная «Байкал»", AccountKind::Resource),
        ] {
            let acc = meta
                .upsert(
                    tenant,
                    AccountUpsert {
                        domain: DomainId::new(),
                        login: login.to_owned(),
                        display_name: name.to_owned(),
                        external_id: None,
                        enabled: true,
                        aliases: Vec::new(),
                        kind,
                        hidden_from_gal: false,
                        resource_type: None,
                    },
                )
                .await
                .expect("учётная запись");
            let p = Principal {
                tenant,
                account: acc.id,
                login: login.to_owned(),
                groups: BTreeSet::new(),
                scopes: None,
            };
            auth.add_password(login, "secret", p);
            accounts.push((
                login.to_owned(),
                AccountScope {
                    tenant,
                    account: acc.id,
                },
            ));
        }
        let store = MemDav::new();
        let sender = MemItipSender::new();
        let cfg = DavConfig {
            schedule_keep_processed: false,
            ..DavConfig::default()
        };
        let inbox =
            ItipInbox::new(Arc::new(store.clone()), cfg.clone()).with_booking(BookingDeps {
                accounts: Arc::new(meta.clone()),
                resources: Arc::new(meta.clone()),
                freebusy: Arc::new(store.clone()),
                group_store: Arc::new(groups.clone()),
                sender: Some(Arc::new(sender.clone())),
            });
        let authz = FakeSharingAuthorizer::default();
        let app = router_shared(
            Deps {
                auth: Arc::new(auth),
                store: Arc::new(store.clone()),
                sharing: Some(Arc::new(authz.clone())),
            },
            Some(ScheduleDeps {
                accounts: Arc::new(meta.clone()),
                delivery: Arc::new(inbox.clone()),
                sender: Some(Arc::new(sender.clone())),
                freebusy: Arc::new(store.clone()),
                group_store: Arc::new(groups.clone()),
            }),
            Some(SharingStores {
                acl: Arc::new(store.clone()),
                delegates: Arc::new(meta.clone()),
            }),
            cfg,
        );
        let env = Self {
            app,
            store,
            meta,
            authz,
            sender,
            inbox,
            accounts,
        };
        env.store
            .create_collection(
                env.scope(ALICE),
                Collection {
                    display_name: "Работа".into(),
                    ..Collection::new(CollectionKind::Calendar, "work")
                },
            )
            .await
            .expect("календарь");
        env
    }

    fn scope(&self, login: &str) -> AccountScope {
        self.accounts
            .iter()
            .find(|(l, _)| l == login)
            .expect("учётная запись")
            .1
    }

    async fn work(&self) -> Collection {
        self.store
            .collection_by_name(self.scope(ALICE), CollectionKind::Calendar, "work")
            .await
            .expect("хранилище")
            .expect("календарь")
    }

    /// Права `login` на календарь Алисы `work`.
    async fn grant(&self, login: &str, rights: CollectionRights) {
        let work = self.work().await;
        self.authz.set_rights(
            self.scope(login).account,
            self.scope(ALICE).account,
            work.id,
            rights,
        );
    }

    async fn send(
        &self,
        login: &str,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Resp {
        use tower::ServiceExt;
        let mut b = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", basic(login, "secret"));
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let req = b.body(Body::from(body.to_owned())).expect("запрос");
        let resp = self.app.clone().oneshot(req).await.expect("infallible");
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .expect("тело");
        Resp {
            status,
            headers,
            body: String::from_utf8(bytes.to_vec()).expect("utf-8"),
        }
    }

    async fn propfind(&self, login: &str, path: &str, depth: &str, body: &str) -> Resp {
        self.send(login, "PROPFIND", path, &[("depth", depth)], body)
            .await
    }

    async fn put(&self, login: &str, path: &str, body: &str) -> Resp {
        self.send(
            login,
            "PUT",
            path,
            &[("content-type", "text/calendar")],
            body,
        )
        .await
    }

    /// `PARTSTAT` участника `who` во встрече `uid` календаря `login`.
    async fn partstat(&self, login: &str, uid: &str, who: &str) -> Option<String> {
        let o = crate::deliver::find_uid(&self.store, self.scope(login), uid)
            .await
            .expect("хранилище")?;
        let ical = ICalendar::parse(&o.body).expect("iCalendar");
        itip::meeting(&ical)?
            .attendee(who)
            .map(|a| a.partstat.clone())
    }
}

/// Встреча `uid` организатора `organizer` с участниками `attendees` в интервале (UTC, `YYYYMMDDTHHMMSSZ`).
fn meeting(uid: &str, organizer: &str, attendees: &[&str], start: &str, end: &str) -> String {
    let mut s = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//RU\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
         DTSTAMP:20260101T000000Z\r\nDTSTART:{start}\r\nDTEND:{end}\r\nSUMMARY:Встреча\r\n\
         ORGANIZER:mailto:{organizer}\r\n"
    );
    for a in attendees {
        s.push_str(&format!(
            "ATTENDEE;PARTSTAT=NEEDS-ACTION;RSVP=TRUE:mailto:{a}\r\n"
        ));
    }
    s.push_str("END:VEVENT\r\nEND:VCALENDAR\r\n");
    s
}

const MEMBERS: &str = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:group-member-set/></d:prop></d:propfind>"#;

const PRIVS: &str =
    r#"<d:propfind xmlns:d="DAV:"><d:prop><d:current-user-privilege-set/></d:prop></d:propfind>"#;

// ---------- DAV-17: общий доступ ----------

#[tokio::test]
async fn foreign_calendar_needs_rights() {
    let env = Share::new().await;
    let r = env.propfind(BOB, WORK, "0", PRIVS).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
    assert!(r.body.contains("need-privileges"), "{}", r.body);
    // Чужие дом и принципал: принципал виден, дом без прав — пустой.
    let r = env
        .propfind(BOB, "/dav/calendars/alice@example.org/", "1", "")
        .await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert!(!r.body.contains("/work/"), "{}", r.body);
    let r = env
        .propfind(BOB, "/dav/principals/alice@example.org/", "0", "")
        .await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert!(r.body.contains("Алиса"), "{}", r.body);
    // Создать коллекцию в чужом доме нельзя.
    let r = env
        .send(
            BOB,
            "MKCALENDAR",
            "/dav/calendars/alice@example.org/x/",
            &[],
            "",
        )
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn read_without_write() {
    let env = Share::new().await;
    assert!(
        env.put(ALICE, &format!("{WORK}a.ics"), &event("a", "План"))
            .await
            .status
            .is_success()
    );
    env.grant(BOB, CollectionRights::READ).await;
    let r = env.send(BOB, "GET", &format!("{WORK}a.ics"), &[], "").await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.body.contains("План"));
    let r = env.propfind(BOB, WORK, "1", PRIVS).await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert!(r.body.contains("a.ics"), "{}", r.body);
    assert!(r.body.contains("<d:read/>"), "{}", r.body);
    assert!(!r.body.contains("write-content"), "{}", r.body);
    // Запись — 403 need-privileges.
    let r = env
        .put(BOB, &format!("{WORK}b.ics"), &event("b", "Чужое"))
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("write-content"), "{}", r.body);
    let r = env
        .send(BOB, "DELETE", &format!("{WORK}a.ics"), &[], "")
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = env.send(BOB, "DELETE", WORK, &[], "").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn write_and_revoke() {
    let env = Share::new().await;
    env.grant(BOB, CollectionRights::WRITE).await;
    let r = env
        .put(BOB, &format!("{WORK}b.ics"), &event("b", "От Бориса"))
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let work = env.work().await;
    let o = env
        .store
        .get_object(env.scope(ALICE), work.id, "b.ics")
        .await
        .expect("хранилище");
    assert!(o.is_some(), "событие — в календаре владельца");
    let r = env
        .send(BOB, "DELETE", &format!("{WORK}b.ics"), &[], "")
        .await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    // Отзыв: доступа больше нет.
    env.grant(BOB, CollectionRights::NONE).await;
    let r = env.propfind(BOB, WORK, "0", PRIVS).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = env
        .put(BOB, &format!("{WORK}c.ics"), &event("c", "После отзыва"))
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn free_busy_only() {
    let env = Share::new().await;
    env.put(ALICE, &format!("{WORK}a.ics"), &event("a", "Тайное"))
        .await;
    env.grant(BOB, CollectionRights::FREE_BUSY).await;
    let r = env.send(BOB, "GET", &format!("{WORK}a.ics"), &[], "").await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let query = r#"<c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><d:getetag/></d:prop><c:filter><c:comp-filter name="VCALENDAR"/></c:filter></c:calendar-query>"#;
    let r = env
        .send(BOB, "REPORT", WORK, &[("depth", "1")], query)
        .await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let fb = r#"<c:free-busy-query xmlns:c="urn:ietf:params:xml:ns:caldav"><c:time-range start="20260105T000000Z" end="20260106T000000Z"/></c:free-busy-query>"#;
    let r = env.send(BOB, "REPORT", WORK, &[("depth", "1")], fb).await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    assert!(r.body.contains("FREEBUSY"), "{}", r.body);
    assert!(!r.body.contains("Тайное"));
    let r = env.propfind(BOB, WORK, "1", PRIVS).await;
    assert!(r.body.contains("read-free-busy"), "{}", r.body);
    assert!(
        !r.body.contains("a.ics"),
        "объекты без права чтения не видны"
    );
}

#[tokio::test]
async fn shared_calendar_in_recipient_home() {
    let env = Share::new().await;
    env.grant(BOB, CollectionRights::READ).await;
    let body = r#"<d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:displayname/><d:owner/><d:current-user-privilege-set/></d:prop></d:propfind>"#;
    let r = env
        .propfind(BOB, "/dav/calendars/bob@example.org/", "1", body)
        .await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert!(
        r.body.contains(WORK),
        "ссылка на календарь владельца: {}",
        r.body
    );
    assert!(r.body.contains("<cs:shared/>"), "{}", r.body);
    assert!(r.body.contains("Работа (Алиса)"), "{}", r.body);
    assert!(
        r.body
            .contains("<d:owner><d:href>/dav/principals/alice@example.org/</d:href></d:owner>"),
        "{}",
        r.body
    );
    // Свои календари по умолчанию — тоже в доме.
    assert!(r.body.contains("/dav/calendars/bob@example.org/calendar/"));
    // Отзыв убирает календарь из дома.
    env.grant(BOB, CollectionRights::NONE).await;
    let r = env
        .propfind(BOB, "/dav/calendars/bob@example.org/", "1", body)
        .await;
    assert!(!r.body.contains(WORK), "{}", r.body);
}

#[tokio::test]
async fn post_share_grants_and_removes() {
    let env = Share::new().await;
    let work = env.work().await;
    let share = r#"<cs:share xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"><cs:set><d:href>mailto:bob@example.org</d:href><cs:summary>Работа</cs:summary><cs:read-write/></cs:set><cs:set><d:href>/dav/principals/petr@example.org/</d:href><cs:read/></cs:set></cs:share>"#;
    let r = env
        .send(
            ALICE,
            "POST",
            WORK,
            &[("content-type", "application/xml")],
            share,
        )
        .await;
    assert_eq!(r.status, StatusCode::OK, "{}", r.body);
    let mut acl = env
        .store
        .collection_acl(env.scope(ALICE), work.id)
        .await
        .expect("права");
    acl.sort_by(|a, b| a.identifier.cmp(&b.identifier));
    assert_eq!(acl.len(), 2);
    assert_eq!(acl[0].identifier, BOB);
    assert!(acl[0].rights.contains(CollectionRights::WRITE));
    assert_eq!(acl[1].identifier, PETR);
    assert_eq!(acl[1].rights.effective(), CollectionRights::READ);
    // Выданные права — в `cs:invite` у владельца.
    let invite = r#"<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"><d:prop><cs:invite/><cs:allowed-sharing-modes/></d:prop></d:propfind>"#;
    let r = env.propfind(ALICE, WORK, "0", invite).await;
    assert!(r.body.contains("mailto:bob@example.org"), "{}", r.body);
    assert!(r.body.contains("<cs:read-write/>"), "{}", r.body);
    assert!(r.body.contains("can-be-shared"), "{}", r.body);
    // WebDAV share-resource: отзыв.
    let revoke = r#"<d:share-resource xmlns:d="DAV:"><d:sharee><d:href>mailto:bob@example.org</d:href><d:share-access><d:no-access/></d:share-access></d:sharee></d:share-resource>"#;
    let r = env.send(ALICE, "POST", WORK, &[], revoke).await;
    assert_eq!(r.status, StatusCode::OK);
    let acl = env
        .store
        .collection_acl(env.scope(ALICE), work.id)
        .await
        .expect("права");
    assert_eq!(acl.len(), 1);
    assert_eq!(acl[0].identifier, PETR);
    // Неизвестный получатель — 409, ничего не записано.
    let bad = r#"<cs:share xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"><cs:set><d:href>mailto:nobody@example.org</d:href><cs:read/></cs:set></cs:share>"#;
    let r = env.send(ALICE, "POST", WORK, &[], bad).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    // Получатель без права admin выдавать права не может.
    env.grant(BOB, CollectionRights::WRITE).await;
    let r = env.send(BOB, "POST", WORK, &[], share).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN, "{}", r.body);
}

#[tokio::test]
async fn options_advertise_sharing() {
    let env = Share::new().await;
    let r = env.send(BOB, "OPTIONS", "/dav/", &[], "").await;
    let dav = r.header("dav").expect("DAV");
    assert!(dav.contains("calendarserver-sharing"), "{dav}");
    assert!(dav.contains("calendar-proxy"), "{dav}");
}

// ---------- DAV-18: представители ----------

#[tokio::test]
async fn proxy_group_membership() {
    let env = Share::new().await;
    let group = "/dav/principals/alice@example.org/calendar-proxy-write/";
    let set = r#"<d:propertyupdate xmlns:d="DAV:"><d:set><d:prop><d:group-member-set><d:href>/dav/principals/bob@example.org/</d:href></d:group-member-set></d:prop></d:set></d:propertyupdate>"#;
    let r = env.send(ALICE, "PROPPATCH", group, &[], set).await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert!(r.body.contains("200 OK"), "{}", r.body);
    let delegates = env
        .meta
        .delegates(env.scope(ALICE))
        .await
        .expect("представители");
    assert_eq!(delegates.len(), 1);
    assert_eq!(delegates[0].delegate, env.scope(BOB).account);
    assert_eq!(delegates[0].level, DelegateLevel::Write);
    assert!(delegates[0].schedule_on_behalf);
    let r = env.propfind(ALICE, group, "0", MEMBERS).await;
    assert!(r.body.contains("calendar-proxy-write"), "{}", r.body);
    assert!(
        r.body
            .contains("<d:href>/dav/principals/bob@example.org/</d:href>"),
        "{}",
        r.body
    );
    // Группы — дочерние ресурсы своего принципала.
    let r = env
        .propfind(ALICE, "/dav/principals/alice@example.org/", "1", "")
        .await;
    assert!(r.body.contains("calendar-proxy-read/"), "{}", r.body);
    // Чужую группу менять нельзя.
    let r = env.send(BOB, "PROPPATCH", group, &[], set).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    // Пустой состав — отзыв.
    let clear = r#"<d:propertyupdate xmlns:d="DAV:"><d:remove><d:prop><d:group-member-set/></d:prop></d:remove></d:propertyupdate>"#;
    let r = env.send(ALICE, "PROPPATCH", group, &[], clear).await;
    assert!(r.body.contains("200 OK"), "{}", r.body);
    assert!(
        env.meta
            .delegates(env.scope(ALICE))
            .await
            .expect("представители")
            .is_empty()
    );
    // Сам себе — 409.
    let me = r#"<d:propertyupdate xmlns:d="DAV:"><d:set><d:prop><d:group-member-set><d:href>mailto:alice@example.org</d:href></d:group-member-set></d:prop></d:set></d:propertyupdate>"#;
    let r = env.send(ALICE, "PROPPATCH", group, &[], me).await;
    assert!(r.body.contains("409"), "{}", r.body);
}

#[tokio::test]
async fn proxy_for_and_expand() {
    let env = Share::new().await;
    env.authz.set_delegations(
        env.scope(BOB).account,
        vec![Delegation {
            owner: env.scope(ALICE).account,
            level: DelegateLevel::Write,
            schedule_on_behalf: true,
        }],
    );
    let body = r#"<d:propfind xmlns:d="DAV:" xmlns:cs="http://calendarserver.org/ns/"><d:prop><cs:calendar-proxy-write-for/><cs:calendar-proxy-read-for/><d:group-membership/></d:prop></d:propfind>"#;
    let r = env
        .propfind(BOB, "/dav/principals/bob@example.org/", "0", body)
        .await;
    assert!(
        r.body.contains(
            "<cs:calendar-proxy-write-for><d:href>/dav/principals/alice@example.org/</d:href>"
        ),
        "{}",
        r.body
    );
    assert!(
        r.body.contains("<cs:calendar-proxy-read-for/>"),
        "{}",
        r.body
    );
    assert!(
        r.body
            .contains("/dav/principals/alice@example.org/calendar-proxy-write/"),
        "{}",
        r.body
    );
    let expand = r#"<d:expand-property xmlns:d="DAV:"><d:property name="calendar-proxy-write-for" namespace="http://calendarserver.org/ns/"><d:property name="displayname"/><d:property name="calendar-user-address-set" namespace="urn:ietf:params:xml:ns:caldav"/></d:property></d:expand-property>"#;
    let r = env
        .send(
            BOB,
            "REPORT",
            "/dav/principals/bob@example.org/",
            &[("depth", "0")],
            expand,
        )
        .await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS, "{}", r.body);
    assert!(
        r.body.contains("<cs:calendar-proxy-write-for><d:response><d:href>/dav/principals/alice@example.org/</d:href>"),
        "{}",
        r.body
    );
    assert!(
        r.body.contains("<d:displayname>Алиса</d:displayname>"),
        "{}",
        r.body
    );
    assert!(r.body.contains("mailto:alice@example.org"), "{}", r.body);
}

#[tokio::test]
async fn delegate_schedules_on_behalf() {
    let env = Share::new().await;
    env.grant(
        BOB,
        CollectionRights {
            schedule: true,
            ..CollectionRights::WRITE
        },
    )
    .await;
    let ics = meeting("m1", ALICE, &[PETR], "20260105T100000Z", "20260105T110000Z");
    let r = env.put(BOB, &format!("{WORK}m1.ics"), &ics).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    let o = crate::deliver::find_uid(&env.store, env.scope(PETR), "m1")
        .await
        .expect("хранилище")
        .expect("приглашение у участника");
    assert!(
        o.body.contains("SENT-BY=\"mailto:bob@example.org\"")
            || o.body.contains("SENT-BY=mailto:bob@example.org"),
        "{}",
        o.body
    );
    assert!(o.body.contains("ORGANIZER"), "{}", o.body);
    // Без права планирования от имени владельца встречи не меняются, обычные события — да.
    env.grant(BOB, CollectionRights::WRITE).await;
    let ics = meeting("m2", ALICE, &[PETR], "20260106T100000Z", "20260106T110000Z");
    let r = env.put(BOB, &format!("{WORK}m2.ics"), &ics).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    assert!(r.body.contains("schedule-send"), "{}", r.body);
    let r = env
        .put(BOB, &format!("{WORK}e.ics"), &event("e", "Без участников"))
        .await;
    assert_eq!(r.status, StatusCode::CREATED);
}

// ---------- DAV-19: ресурсы ----------

impl Share {
    async fn policy(&self, policy: BookingPolicy) {
        self.meta
            .set_booking_policy(self.scope(ROOM), Some(policy))
            .await
            .expect("правила");
    }

    /// Встреча организатора `login` с переговорной: у Алисы — в календаре `work`, у остальных — в календаре по
    /// умолчанию.
    async fn book(&self, login: &str, uid: &str, start: &str, end: &str) -> Resp {
        let home = format!("/dav/calendars/{login}/");
        self.propfind(login, &home, "0", "").await;
        let coll = if login == ALICE {
            WORK.to_owned()
        } else {
            format!("{home}calendar/")
        };
        let ics = meeting(uid, login, &[ROOM, PETR], start, end);
        self.put(login, &format!("{coll}{uid}.ics"), &ics).await
    }
}

#[tokio::test]
async fn room_free_accepted_busy_declined() {
    let env = Share::new().await;
    env.policy(BookingPolicy {
        horizon: None,
        ..BookingPolicy::default()
    })
    .await;
    let r = env
        .book(ALICE, "r1", "20260105T100000Z", "20260105T110000Z")
        .await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.body);
    assert_eq!(
        env.partstat(ROOM, "r1", ROOM).await.as_deref(),
        Some("ACCEPTED"),
        "событие — в календаре ресурса"
    );
    assert_eq!(
        env.partstat(ALICE, "r1", ROOM).await.as_deref(),
        Some("ACCEPTED"),
        "ответ ресурса — у организатора"
    );
    // Пересечение — отказ, события в календаре ресурса нет.
    env.book(BOB, "r2", "20260105T103000Z", "20260105T113000Z")
        .await;
    assert_eq!(env.partstat(ROOM, "r2", ROOM).await, None);
    assert_eq!(
        env.partstat(BOB, "r2", ROOM).await.as_deref(),
        Some("DECLINED")
    );
    // Перенос принятой встречи на свободное время — снова принята, без пересечения с собой.
    env.book(ALICE, "r1", "20260105T103000Z", "20260105T120000Z")
        .await;
    assert_eq!(
        env.partstat(ROOM, "r1", ROOM).await.as_deref(),
        Some("ACCEPTED")
    );
}

#[tokio::test]
async fn room_working_hours_and_tentative() {
    let env = Share::new().await;
    env.policy(BookingPolicy {
        auto_accept: false,
        horizon: None,
        working_hours: Some(WorkingHours {
            timezone: "Europe/Moscow".into(),
            days: [
                DayOfWeek::Monday,
                DayOfWeek::Tuesday,
                DayOfWeek::Wednesday,
                DayOfWeek::Thursday,
                DayOfWeek::Friday,
            ]
            .into(),
            from: Time::from_hms(9, 0, 0).expect("время"),
            to: Time::from_hms(18, 0, 0).expect("время"),
        }),
        ..BookingPolicy::default()
    })
    .await;
    // Понедельник 13:00–14:00 по Москве — в рабочее время, без автоматического принятия — под вопросом.
    env.book(ALICE, "w1", "20260105T100000Z", "20260105T110000Z")
        .await;
    assert_eq!(
        env.partstat(ROOM, "w1", ROOM).await.as_deref(),
        Some("TENTATIVE")
    );
    assert_eq!(
        env.partstat(ALICE, "w1", ROOM).await.as_deref(),
        Some("TENTATIVE")
    );
    // 20:00–21:00 по Москве — вне рабочего времени.
    env.book(ALICE, "w2", "20260105T170000Z", "20260105T180000Z")
        .await;
    assert_eq!(env.partstat(ROOM, "w2", ROOM).await, None);
    assert_eq!(
        env.partstat(ALICE, "w2", ROOM).await.as_deref(),
        Some("DECLINED")
    );
}

#[tokio::test]
async fn room_organizer_not_allowed() {
    let env = Share::new().await;
    env.policy(BookingPolicy {
        horizon: None,
        bookers: Bookers::Only {
            accounts: vec![env.scope(ALICE).account],
            groups: Vec::new(),
        },
        ..BookingPolicy::default()
    })
    .await;
    env.book(BOB, "n1", "20260105T100000Z", "20260105T110000Z")
        .await;
    assert_eq!(env.partstat(ROOM, "n1", ROOM).await, None);
    assert_eq!(
        env.partstat(BOB, "n1", ROOM).await.as_deref(),
        Some("DECLINED")
    );
    env.book(ALICE, "n2", "20260105T100000Z", "20260105T110000Z")
        .await;
    assert_eq!(
        env.partstat(ROOM, "n2", ROOM).await.as_deref(),
        Some("ACCEPTED")
    );
    // Внешний организатор (iMIP) — отказ письмом с причиной.
    let ext = "anna@example.net";
    let ics = meeting("x1", ext, &[ROOM], "20260107T100000Z", "20260107T110000Z")
        .replace("BEGIN:VEVENT", "METHOD:REQUEST\r\nBEGIN:VEVENT");
    let out = env
        .inbox
        .deliver(&ItipMessage {
            recipient: env.scope(ROOM),
            to: ROOM.into(),
            from: ext.into(),
            method: ItipMethod::Request,
            ics,
            source: ItipSource::Imip,
        })
        .await
        .expect("приём");
    assert_eq!(out, ItipOutcome::Applied);
    assert_eq!(env.partstat(ROOM, "x1", ROOM).await, None);
    let sent = env.sender.sent();
    let reply = sent
        .iter()
        .find(|m| m.method == ItipMethod::Reply && m.to == [ext])
        .expect("ответ внешнему организатору");
    assert_eq!(reply.from, ROOM);
    assert!(reply.ics.contains("PARTSTAT=DECLINED"), "{}", reply.ics);
    assert!(reply.ics.contains("COMMENT:"), "{}", reply.ics);
}

impl Share {
    /// Приглашение ресурсу письмом iMIP от `from` (он же организатор).
    async fn imip_to_room(&self, uid: &str, from: &str, start: &str, end: &str) -> ItipOutcome {
        let ics = meeting(uid, from, &[ROOM], start, end)
            .replace("BEGIN:VEVENT", "METHOD:REQUEST\r\nBEGIN:VEVENT");
        self.inbox
            .deliver(&ItipMessage {
                recipient: self.scope(ROOM),
                to: ROOM.into(),
                from: from.into(),
                method: ItipMethod::Request,
                ics,
                source: ItipSource::Imip,
            })
            .await
            .expect("приём")
    }

    /// Организатор брони `uid` в календаре ресурса.
    async fn room_organizer(&self, uid: &str) -> Option<String> {
        let o = crate::deliver::find_uid(&self.store, self.scope(ROOM), uid)
            .await
            .expect("хранилище")?;
        itip::meeting(&ICalendar::parse(&o.body).expect("iCalendar"))?.organizer
    }
}

#[tokio::test]
async fn room_booking_not_taken_by_foreign_uid() {
    // DAV-40: приглашение с UID чужой встречи не снимает и не перехватывает бронь.
    let env = Share::new().await;
    env.policy(BookingPolicy {
        horizon: None,
        ..BookingPolicy::default()
    })
    .await;
    env.book(ALICE, "b1", "20260105T100000Z", "20260105T110000Z")
        .await;
    assert_eq!(
        env.partstat(ROOM, "b1", ROOM).await.as_deref(),
        Some("ACCEPTED")
    );
    // Внешний организатор с тем же UID.
    let ext = "mallory@example.net";
    let out = env
        .imip_to_room("b1", ext, "20260105T100000Z", "20260105T110000Z")
        .await;
    assert!(matches!(out, ItipOutcome::Ignored(_)), "{out:?}");
    assert_eq!(
        env.partstat(ROOM, "b1", ROOM).await.as_deref(),
        Some("ACCEPTED"),
        "бронь цела"
    );
    assert_eq!(env.room_organizer("b1").await.as_deref(), Some(ALICE));
    assert!(
        !env.sender.sent().iter().any(|m| m.to == [ext]),
        "ответа внешнему нет"
    );
    // Сотрудник с тем же UID в своё время.
    env.book(BOB, "b1", "20260106T100000Z", "20260106T110000Z")
        .await;
    assert_eq!(
        env.partstat(ROOM, "b1", ROOM).await.as_deref(),
        Some("ACCEPTED"),
        "бронь цела"
    );
    assert_eq!(env.room_organizer("b1").await.as_deref(), Some(ALICE));
    assert_eq!(
        env.partstat(BOB, "b1", ROOM).await.as_deref(),
        Some("NEEDS-ACTION"),
        "ресурс сотруднику не ответил"
    );
    // Время Алисы по-прежнему занято: встреча сотрудника в нём — отказ.
    env.book(BOB, "b2", "20260105T103000Z", "20260105T113000Z")
        .await;
    assert_eq!(
        env.partstat(BOB, "b2", ROOM).await.as_deref(),
        Some("DECLINED")
    );
}

#[tokio::test]
async fn room_imip_from_own_domain_is_external() {
    // DAV-42: письмо извне с адресом организации (DMARC pass через сторонний сервис домена) не бронирует.
    let env = Share::new().await;
    env.policy(BookingPolicy {
        horizon: None,
        ..BookingPolicy::default()
    })
    .await;
    let out = env
        .imip_to_room("i1", ALICE, "20260105T100000Z", "20260105T110000Z")
        .await;
    assert_eq!(out, ItipOutcome::Applied);
    assert_eq!(env.partstat(ROOM, "i1", ROOM).await, None, "брони нет");
    assert!(
        env.sender.sent().is_empty(),
        "ответ на свой адрес — не письмом наружу"
    );
    // Та же встреча изнутри (CalDAV) — принята.
    env.book(ALICE, "i1", "20260105T100000Z", "20260105T110000Z")
        .await;
    assert_eq!(
        env.partstat(ROOM, "i1", ROOM).await.as_deref(),
        Some("ACCEPTED")
    );
}

#[tokio::test]
async fn room_in_principal_search() {
    let env = Share::new().await;
    let cutype = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop><c:calendar-user-type/></d:prop></d:propfind>"#;
    let r = env
        .propfind(ALICE, "/dav/principals/room@example.org/", "0", cutype)
        .await;
    assert!(
        r.body
            .contains("<c:calendar-user-type>ROOM</c:calendar-user-type>"),
        "{}",
        r.body
    );
    let search = r#"<d:principal-property-search xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:property-search><d:prop><c:calendar-user-type/></d:prop><d:match>ROOM</d:match></d:property-search><d:prop><d:displayname/><c:calendar-user-address-set/></d:prop></d:principal-property-search>"#;
    let r = env
        .send(
            ALICE,
            "REPORT",
            "/dav/principals/",
            &[("depth", "0")],
            search,
        )
        .await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS, "{}", r.body);
    assert!(
        r.body.contains("/dav/principals/room@example.org/"),
        "{}",
        r.body
    );
    assert!(!r.body.contains("bob@example.org"), "{}", r.body);
    let cs = r#"<cs:calendarserver-principal-search xmlns:cs="http://calendarserver.org/ns/" xmlns:d="DAV:" context="location"><cs:search-token>байкал</cs:search-token><cs:limit><cs:nresults>10</cs:nresults></cs:limit><d:prop><d:displayname/></d:prop></cs:calendarserver-principal-search>"#;
    let r = env
        .send(ALICE, "REPORT", "/dav/principals/", &[("depth", "0")], cs)
        .await;
    assert!(r.body.contains("Переговорная «Байкал»"), "{}", r.body);
    // Поиск людей по имени не находит ресурсы.
    let people = r#"<cs:calendarserver-principal-search xmlns:cs="http://calendarserver.org/ns/" xmlns:d="DAV:" context="attendee"><cs:search-token>example.org</cs:search-token></cs:calendarserver-principal-search>"#;
    let r = env
        .send(
            ALICE,
            "REPORT",
            "/dav/principals/",
            &[("depth", "0")],
            people,
        )
        .await;
    assert!(r.body.contains("bob@example.org"), "{}", r.body);
    assert!(!r.body.contains("room@example.org/"), "{}", r.body);
}

#[tokio::test]
async fn delegate_store_used_for_delegation_rights() {
    // Представитель через хранилище (как назначит Admin API) виден владельцу в группе.
    let env = Share::new().await;
    env.meta
        .set_delegate(
            env.scope(ALICE),
            tp_contracts::sharing::Delegate {
                delegate: env.scope(PETR).account,
                level: DelegateLevel::Read,
                schedule_on_behalf: false,
            },
        )
        .await
        .expect("представитель");
    let r = env
        .propfind(
            BOB,
            "/dav/principals/alice@example.org/calendar-proxy-read/",
            "0",
            MEMBERS,
        )
        .await;
    assert_eq!(r.status, StatusCode::MULTI_STATUS);
    assert!(
        r.body.contains("/dav/principals/petr@example.org/"),
        "{}",
        r.body
    );
}
