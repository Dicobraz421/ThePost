//! Планирование встреч (DAV-13…16): рассылка iTIP после `PUT` и `DELETE`, приём (`ItipInbox`), письма iMIP,
//! входящие и исходящие, занятость. Хранилище, учётные записи и отправка — в памяти.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::Request;
use calcard::icalendar::ICalendar;
use mail_parser::MimeHeaders;
use time::OffsetDateTime;
use tp_contracts::auth::Principal;
use tp_contracts::dav::{CollectionKind, DavObject, DavStore};
use tp_contracts::meta::{AccountKind, AccountScope, AccountStore, AccountUpsert};
use tp_contracts::queue::Outbound;
use tp_contracts::schedule::{
    ItipDelivery, ItipMessage, ItipMethod, ItipOutcome, ItipOutgoing, ItipSender, ItipSource,
};
use tp_contracts::{DomainId, TenantId};
use tp_testkit::{
    FakeAuthenticator, ManualClock, MemDav, MemGroups, MemItipSender, MemMetadata, MemOutbound,
};

use super::{Resp, basic};
use crate::{DavConfig, Deps, ImipSender, ItipInbox, ScheduleDeps, imip, itip, router_with};

const IVAN: &str = "ivan@example.org";
const PETR: &str = "petr@example.org";
const OLGA: &str = "olga@example.org";
/// Внешний адрес: учётной записи нет.
const ANNA: &str = "anna@example.net";

struct Sched {
    app: Router,
    store: MemDav,
    sender: MemItipSender,
    inbox: ItipInbox,
    tenant: TenantId,
    accounts: Vec<(String, AccountScope)>,
    /// Учётные записи (псевдонимы — DAV-23).
    meta: MemMetadata,
    /// Группы рассылки (DAV-30).
    groups: MemGroups,
}

impl Sched {
    /// Без хранения применённых сообщений: входящие CalDAV и хранилище совпадают, тесты считают только
    /// необработанные. Хранение по умолчанию (DAV-36) проверяет `keep_processed_by_default`.
    async fn new() -> Self {
        Self::with_config(DavConfig {
            schedule_keep_processed: false,
            ..DavConfig::default()
        })
        .await
    }

    async fn with_config(cfg: DavConfig) -> Self {
        let tenant = TenantId::new();
        let groups = MemGroups::new();
        let meta = MemMetadata::new(ManualClock::default().shared()).with_groups(groups.clone());
        let auth = FakeAuthenticator::default();
        let mut accounts = Vec::new();
        for login in [IVAN, PETR, OLGA] {
            let acc = meta
                .upsert(
                    tenant,
                    AccountUpsert {
                        domain: DomainId::new(),
                        login: login.to_owned(),
                        display_name: login.to_owned(),
                        external_id: None,
                        enabled: true,
                        aliases: Vec::new(),
                        kind: AccountKind::User,
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
        let inbox = ItipInbox::new(Arc::new(store.clone()), cfg.clone());
        let app = router_with(
            Deps {
                auth: Arc::new(auth),
                store: Arc::new(store.clone()),
                sharing: None,
            },
            Some(ScheduleDeps {
                accounts: Arc::new(meta.clone()),
                delivery: Arc::new(inbox.clone()),
                sender: Some(Arc::new(sender.clone())),
                freebusy: Arc::new(store.clone()),
                group_store: Arc::new(groups.clone()),
            }),
            cfg,
        );
        Self {
            app,
            store,
            sender,
            inbox,
            tenant,
            accounts,
            meta,
            groups,
        }
    }

    fn scope(&self, login: &str) -> AccountScope {
        self.accounts
            .iter()
            .find(|(l, _)| l == login)
            .expect("учётная запись")
            .1
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

    /// `PUT` объекта в календарь по умолчанию.
    async fn put(&self, login: &str, name: &str, headers: &[(&str, &str)], body: &str) -> Resp {
        // Первое обращение к дому создаёт календари по умолчанию.
        self.send(login, "PROPFIND", &home(login), &[("depth", "0")], "")
            .await;
        self.send(login, "PUT", &object(login, name), headers, body)
            .await
    }

    /// Объект с UID в календарях учётной записи.
    async fn by_uid(&self, login: &str, uid: &str) -> Option<DavObject> {
        crate::deliver::find_uid(&self.store, self.scope(login), uid)
            .await
            .expect("хранилище")
    }

    /// Число периодов занятости `login` в январе 2026: повторы разворачивает хранилище при запросе (META-54).
    async fn busy_in_january(&self, login: &str) -> usize {
        let scope = self.scope(login);
        let at = |unix| time::OffsetDateTime::from_unix_timestamp(unix).expect("время");
        let range = tp_contracts::schedule::Period {
            start: at(1_767_225_600),
            end: at(1_769_904_000),
        };
        tp_contracts::schedule::FreeBusy::query(&self.store, scope.tenant, &[scope.account], range)
            .await
            .expect("занятость")
            .into_iter()
            .map(|(_, periods)| periods.len())
            .sum()
    }

    async fn body(&self, login: &str, uid: &str) -> String {
        self.by_uid(login, uid).await.expect("объект").body
    }

    async fn inbox_len(&self, login: &str) -> usize {
        self.store
            .inbox_list(self.scope(login))
            .await
            .expect("входящие")
            .items
            .len()
    }

    /// Доставка iTIP учётной записи `to` от `from` (как письмо iMIP).
    async fn deliver(&self, to: &str, from: &str, method: ItipMethod, ics: &str) -> ItipOutcome {
        self.inbox
            .deliver(&ItipMessage {
                recipient: self.scope(to),
                to: to.to_owned(),
                from: from.to_owned(),
                method,
                ics: ics.to_owned(),
                source: ItipSource::Imip,
            })
            .await
            .expect("доставка")
    }
}

fn home(login: &str) -> String {
    format!("/dav/calendars/{login}/")
}

fn object(login: &str, name: &str) -> String {
    format!("/dav/calendars/{login}/calendar/{name}")
}

fn attendee(addr: &str, partstat: &str) -> String {
    format!("ATTENDEE;PARTSTAT={partstat}:mailto:{addr}\r\n")
}

/// Встреча организатора `organizer`: `props` — прочие свойства VEVENT (участники, повторы).
fn meeting(uid: &str, organizer: &str, start: &str, seq: u32, props: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//RU\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
         DTSTAMP:20260101T000000Z\r\nDTSTART:{start}\r\nDURATION:PT1H\r\nSEQUENCE:{seq}\r\n\
         SUMMARY:Планёрка\r\nORGANIZER:mailto:{organizer}\r\n{props}END:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

/// То же сообщением iTIP.
fn itip_msg(method: &str, body: &str) -> String {
    body.replace(
        "VERSION:2.0\r\n",
        &format!("VERSION:2.0\r\nMETHOD:{method}\r\n"),
    )
}

fn two(partstat: &str) -> String {
    format!(
        "{}{}",
        attendee(PETR, partstat),
        attendee(OLGA, "NEEDS-ACTION")
    )
}

const T10: &str = "20260105T100000Z";
const T14: &str = "20260105T140000Z";

fn tag(o: &DavObject) -> String {
    o.index.schedule_tag.clone().expect("Schedule-Tag")
}

// ---------- DAV-13: планирование внутри организации ----------

#[tokio::test]
async fn invite_accept_reschedule_remove_cancel() {
    let env = Sched::new().await;
    // Приглашение двух участников.
    let v1 = meeting("m-1", IVAN, T10, 0, &two("NEEDS-ACTION"));
    let r = env.put(IVAN, "m.ics", &[], &v1).await;
    assert_eq!(r.status, 201);
    assert!(r.header("etag").is_none(), "тело дополнено SCHEDULE-STATUS");
    let org = env.by_uid(IVAN, "m-1").await.expect("у организатора");
    let org_tag = tag(&org);
    assert_eq!(
        r.header("schedule-tag"),
        Some(format!("\"{org_tag}\"").as_str())
    );
    assert_eq!(
        org.body.matches("SCHEDULE-STATUS=1.2").count(),
        2,
        "{}",
        org.body
    );
    for who in [PETR, OLGA] {
        let o = env.by_uid(who, "m-1").await.expect("у участника");
        assert!(o.body.contains("PARTSTAT=NEEDS-ACTION"), "{}", o.body);
        assert!(!o.body.contains("METHOD"), "{}", o.body);
        assert_eq!(o.index.busy.len(), 1);
        assert_eq!(env.inbox_len(who).await, 0, "применённое не копится");
    }

    // Принятие: участник меняет PARTSTAT — REPLY организатору, теги прежние.
    let mine = env.by_uid(PETR, "m-1").await.expect("копия");
    let mine_tag = tag(&mine);
    let get = env
        .send(PETR, "GET", &object(PETR, &mine.name), &[], "")
        .await;
    assert_eq!(
        get.header("schedule-tag"),
        Some(format!("\"{mine_tag}\"").as_str())
    );
    let accepted = get
        .body
        .replacen("PARTSTAT=NEEDS-ACTION", "PARTSTAT=ACCEPTED", 1);
    let stale = env
        .send(
            PETR,
            "PUT",
            &object(PETR, &mine.name),
            &[("if-schedule-tag-match", "\"other\"")],
            &accepted,
        )
        .await;
    assert_eq!(stale.status, 412);
    let r = env
        .send(
            PETR,
            "PUT",
            &object(PETR, &mine.name),
            &[("if-schedule-tag-match", &format!("\"{mine_tag}\""))],
            &accepted,
        )
        .await;
    assert_eq!(r.status, 204);
    let mine = env.by_uid(PETR, "m-1").await.expect("копия");
    assert_eq!(tag(&mine), mine_tag, "Schedule-Tag не меняется от PARTSTAT");
    assert!(mine.body.contains("SCHEDULE-STATUS=1.2"), "{}", mine.body);
    let org = env.by_uid(IVAN, "m-1").await.expect("у организатора");
    assert_eq!(
        tag(&org),
        org_tag,
        "Schedule-Tag организатора не меняется от ответа"
    );
    assert!(
        org.body.contains(&format!(
            "PARTSTAT=ACCEPTED;SCHEDULE-STATUS=2.0:mailto:{PETR}"
        )),
        "{}",
        org.body
    );
    assert_eq!(env.inbox_len(IVAN).await, 0);

    // Перенос: новое время — участникам REQUEST, их ответы сброшены, теги новые.
    let v2 = meeting("m-1", IVAN, T14, 1, &two("ACCEPTED"));
    let r = env
        .put(
            IVAN,
            "m.ics",
            &[("if-schedule-tag-match", &format!("\"{org_tag}\""))],
            &v2,
        )
        .await;
    assert_eq!(r.status, 204);
    let mine = env.by_uid(PETR, "m-1").await.expect("копия");
    assert!(mine.body.contains(T14), "{}", mine.body);
    assert!(
        mine.body
            .contains(&format!("PARTSTAT=NEEDS-ACTION:mailto:{PETR}")),
        "{}",
        mine.body
    );
    assert_ne!(tag(&mine), mine_tag);
    assert_ne!(tag(&env.by_uid(IVAN, "m-1").await.expect("o")), org_tag);

    // Удаление участника: ему CANCEL, остальным — обновление.
    let v3 = meeting("m-1", IVAN, T14, 2, &attendee(PETR, "NEEDS-ACTION"));
    assert_eq!(env.put(IVAN, "m.ics", &[], &v3).await.status, 204);
    assert!(env.body(OLGA, "m-1").await.contains("STATUS:CANCELLED"));
    let mine = env.body(PETR, "m-1").await;
    assert!(
        !mine.contains("STATUS:CANCELLED") && !mine.contains(OLGA),
        "{mine}"
    );
    assert!(
        env.by_uid(OLGA, "m-1")
            .await
            .expect("o")
            .index
            .busy
            .is_empty()
    );

    // Отмена: организатор удаляет встречу.
    let r = env
        .send(IVAN, "DELETE", &object(IVAN, "m.ics"), &[], "")
        .await;
    assert_eq!(r.status, 204);
    assert!(env.body(PETR, "m-1").await.contains("STATUS:CANCELLED"));
    assert!(env.sender.sent().is_empty(), "внешних участников нет");
}

#[tokio::test]
async fn schedule_agent_client_and_decline_on_delete() {
    let env = Sched::new().await;
    // SCHEDULE-AGENT=CLIENT — сервер не планирует, но объект остаётся объектом планирования.
    let body = meeting(
        "m-2",
        IVAN,
        T10,
        0,
        &format!("ATTENDEE;SCHEDULE-AGENT=CLIENT:mailto:{PETR}\r\n"),
    );
    let r = env.put(IVAN, "c.ics", &[], &body).await;
    assert_eq!(r.status, 201);
    assert!(r.header("etag").is_some() && r.header("schedule-tag").is_some());
    assert!(env.by_uid(PETR, "m-2").await.is_none());

    // Участник удаляет приглашение — организатору REPLY с отказом.
    let v = meeting("m-3", IVAN, T10, 0, &attendee(PETR, "NEEDS-ACTION"));
    assert_eq!(env.put(IVAN, "d.ics", &[], &v).await.status, 201);
    let mine = env.by_uid(PETR, "m-3").await.expect("копия");
    let r = env
        .send(PETR, "DELETE", &object(PETR, &mine.name), &[], "")
        .await;
    assert_eq!(r.status, 204);
    assert!(env.body(IVAN, "m-3").await.contains("PARTSTAT=DECLINED"));

    // Обычное событие без участников — без тега и рассылки.
    let plain = super::event("p-1", "Обед");
    let r = env.put(IVAN, "p.ics", &[], &plain).await;
    assert!(r.header("schedule-tag").is_none() && r.header("etag").is_some());
}

#[tokio::test]
async fn inbox_outbox_properties() {
    let env = Sched::new().await;
    let counter = itip_msg(
        "COUNTER",
        &meeting("m-4", IVAN, T14, 0, &attendee(PETR, "TENTATIVE")),
    );
    env.put(
        IVAN,
        "e.ics",
        &[],
        &meeting("m-4", IVAN, T10, 0, &attendee(PETR, "NEEDS-ACTION")),
    )
    .await;
    assert_eq!(
        env.deliver(IVAN, PETR, ItipMethod::Counter, &counter).await,
        ItipOutcome::Applied
    );
    let inbox = format!("{}inbox/", home(IVAN));
    let r = env
        .send(
            IVAN,
            "PROPFIND",
            &inbox,
            &[("depth", "1")],
            r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:cs="http://calendarserver.org/ns/">
                 <d:prop><d:resourcetype/><c:schedule-default-calendar-URL/><d:getcontenttype/></d:prop></d:propfind>"#,
        )
        .await;
    assert_eq!(r.status, 207);
    let shown = r.show();
    assert!(shown.contains("<c:schedule-inbox/>"), "{shown}");
    assert!(
        shown.contains("<c:schedule-default-calendar-URL>\n<d:href>/dav/calendars/ivan@example.org/calendar/</d:href>"),
        "{shown}"
    );
    let items = env
        .store
        .inbox_list(env.scope(IVAN))
        .await
        .expect("i")
        .items;
    assert_eq!(items.len(), 1);
    let item = format!("{inbox}{}", items[0].name);
    assert!(shown.contains(&item), "{shown}");
    let got = env.send(IVAN, "GET", &item, &[], "").await;
    assert_eq!(got.status, 200);
    assert!(got.body.contains("METHOD:COUNTER"));
    assert_eq!(
        env.send(IVAN, "PUT", &item, &[], &counter).await.status,
        403
    );
    assert_eq!(env.send(IVAN, "DELETE", &item, &[], "").await.status, 204);
    assert_eq!(env.send(IVAN, "DELETE", &item, &[], "").await.status, 404);
    assert_eq!(env.inbox_len(IVAN).await, 0);

    // schedule-calendar-transp: чтение и запись.
    let cal = format!("{}calendar/", home(IVAN));
    let patch = r#"<d:propertyupdate xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:set><d:prop>
        <c:schedule-calendar-transp><c:transparent/></c:schedule-calendar-transp></d:prop></d:set></d:propertyupdate>"#;
    assert_eq!(
        env.send(IVAN, "PROPPATCH", &cal, &[], patch).await.status,
        207
    );
    let c = env
        .store
        .collection_by_name(env.scope(IVAN), CollectionKind::Calendar, "calendar")
        .await
        .expect("s")
        .expect("c");
    assert!(c.transparent);
    let r = env
        .send(
            IVAN,
            "PROPFIND",
            &cal,
            &[("depth", "1")],
            r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav"><d:prop>
               <c:schedule-calendar-transp/><c:schedule-tag/></d:prop></d:propfind>"#,
        )
        .await;
    let shown = r.show();
    assert!(shown.contains("<c:transparent/>"), "{shown}");
    assert!(
        shown.contains("<c:schedule-tag>&quot;") || shown.contains("<c:schedule-tag>\""),
        "{shown}"
    );
}

// ---------- DAV-14: iMIP наружу ----------

/// Письмо с постоянными границами частей (в письме они случайные).
fn stable(raw: &[u8]) -> String {
    let mut text = String::from_utf8(raw.to_vec()).expect("utf-8");
    let mut n = 0;
    while let Some(at) = text.find("boundary=\"") {
        let rest = &text[at + 10..];
        let b = rest[..rest.find('"').expect("кавычка")].to_owned();
        if b.starts_with("part-") {
            break;
        }
        n += 1;
        text = text.replace(&b, &format!("part-{n}"));
        text = text.replacen("boundary=\"part-", "boundary_done=\"part-", 1);
    }
    text.replace("boundary_done=", "boundary=")
        .replace("\r\n", "\n")
}

/// Письмо iMIP сообщения плана: снимок и обратный разбор mail-parser и calcard.
fn mail(m: &itip::Message, from: &str, tenant: TenantId) -> String {
    let (subject, text) = imip::describe(m.method, &m.ical);
    let out = ItipOutgoing {
        sender: AccountScope {
            tenant,
            account: tp_contracts::AccountId::new(),
        },
        from: from.to_owned(),
        to: m.to.clone(),
        method: m.method,
        ics: m.ics(),
        subject,
        text,
    };
    out.validate().expect("допустимо");
    let raw = imip::message(&out, OffsetDateTime::UNIX_EPOCH).expect("письмо");
    let parsed = mail_parser::MessageParser::default()
        .parse(&raw)
        .expect("mail-parser");
    let cal = parsed
        .parts
        .iter()
        .find(|p| {
            p.content_type().is_some_and(|c| {
                c.subtype() == Some("calendar") && c.attribute("method") == Some(m.method.as_str())
            })
        })
        .expect("часть text/calendar с method");
    let it = itip::parse(cal.text_contents().expect("текст")).expect("calcard");
    assert_eq!(it.method, m.method);
    assert!(
        parsed
            .attachments()
            .any(|a| a.attachment_name() == Some("invite.ics")),
        "вложение .ics"
    );
    assert_eq!(
        parsed
            .from()
            .and_then(|f| f.first())
            .and_then(|a| a.address()),
        Some(from)
    );
    stable(&raw)
}

#[test]
fn imip_snapshots() {
    let tenant = TenantId::new();
    let now = OffsetDateTime::UNIX_EPOCH;
    let parse = |s: &str| ICalendar::parse(s).expect("ical");
    let v1 = parse(&meeting(
        "m-5",
        IVAN,
        T10,
        0,
        &attendee(ANNA, "NEEDS-ACTION"),
    ));
    let me = [IVAN.to_owned()];
    let request = itip::plan(None, Some(&v1), &me, now);
    insta::assert_snapshot!("imip_request", mail(&request.messages[0], IVAN, tenant));
    let cancel = itip::plan(Some(&v1), None, &me, now);
    insta::assert_snapshot!("imip_cancel", mail(&cancel.messages[0], IVAN, tenant));
    // Участник организации отвечает внешнему организатору.
    let theirs = parse(&meeting(
        "m-6",
        ANNA,
        T10,
        0,
        &attendee(PETR, "NEEDS-ACTION"),
    ));
    let accepted = parse(&meeting("m-6", ANNA, T10, 0, &attendee(PETR, "ACCEPTED")));
    let reply = itip::plan(Some(&theirs), Some(&accepted), &[PETR.to_owned()], now);
    assert_eq!(reply.messages[0].to, vec![ANNA.to_owned()]);
    insta::assert_snapshot!("imip_reply", mail(&reply.messages[0], PETR, tenant));
}

#[tokio::test]
async fn external_attendee_and_organizer() {
    let env = Sched::new().await;
    // Внешний участник — письмо iMIP, внутренний — прямая доставка.
    let body = meeting(
        "m-7",
        IVAN,
        T10,
        0,
        &format!(
            "{}{}",
            attendee(PETR, "NEEDS-ACTION"),
            attendee(ANNA, "NEEDS-ACTION")
        ),
    );
    assert_eq!(env.put(IVAN, "x.ics", &[], &body).await.status, 201);
    let sent = env.sender.sent();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        (sent[0].method, sent[0].from.as_str()),
        (ItipMethod::Request, IVAN)
    );
    assert_eq!(sent[0].to, vec![ANNA.to_owned()]);
    assert_eq!(sent[0].subject, "Приглашение: Планёрка");
    assert_eq!(sent[0].sender, env.scope(IVAN));
    let org = env.body(IVAN, "m-7").await;
    assert!(
        org.contains(&format!("SCHEDULE-STATUS=1.1:mailto:{ANNA}")),
        "{org}"
    );
    assert!(
        org.contains(&format!("SCHEDULE-STATUS=1.2:mailto:{PETR}")),
        "{org}"
    );

    // Приглашение извне пришло письмом; ответ участника уходит организатору письмом.
    let theirs = meeting("m-8", ANNA, T10, 0, &attendee(PETR, "NEEDS-ACTION"));
    assert_eq!(
        env.deliver(
            PETR,
            ANNA,
            ItipMethod::Request,
            &itip_msg("REQUEST", &theirs)
        )
        .await,
        ItipOutcome::Applied
    );
    let mine = env.by_uid(PETR, "m-8").await.expect("копия");
    let accepted = mine
        .body
        .replacen("PARTSTAT=NEEDS-ACTION", "PARTSTAT=ACCEPTED", 1);
    let r = env
        .send(PETR, "PUT", &object(PETR, &mine.name), &[], &accepted)
        .await;
    assert_eq!(r.status, 204);
    let sent = env.sender.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        (sent[1].method, sent[1].from.as_str()),
        (ItipMethod::Reply, PETR)
    );
    assert_eq!(sent[1].to, vec![ANNA.to_owned()]);
    assert_eq!(sent[1].subject, "Принято: Планёрка");
    assert!(sent[1].ics.contains("METHOD:REPLY") && sent[1].ics.contains("PARTSTAT=ACCEPTED"));

    // Реализация отправителя ставит письмо в очередь от имени организатора.
    let out = MemOutbound::default();
    let imip = ImipSender::new(Arc::new(out.clone()) as Arc<dyn Outbound>);
    imip.send(&sent[0]).await.expect("поставлено");
    let queued = out.queued();
    assert_eq!(queued.len(), 1);
    assert_eq!(
        (queued[0].tenant, queued[0].mail_from.as_str()),
        (env.tenant, IVAN)
    );
    assert_eq!(queued[0].rcpts, vec![ANNA.to_owned()]);
}

// ---------- DAV-15: приём iTIP ----------

#[tokio::test]
async fn deliver_each_method_idempotent() {
    let env = Sched::new().await;
    let v1 = meeting("m-9", ANNA, T10, 0, &attendee(PETR, "ACCEPTED"));
    let request = itip_msg("REQUEST", &v1);
    assert_eq!(
        env.deliver(PETR, ANNA, ItipMethod::Request, &request).await,
        ItipOutcome::Applied
    );
    let o = env.by_uid(PETR, "m-9").await.expect("событие");
    assert!(
        o.body.contains("PARTSTAT=NEEDS-ACTION"),
        "чужой ACCEPTED не принимается: {}",
        o.body
    );
    assert!(o.index.schedule_tag.is_some());
    // Повторная доставка ничего не меняет.
    assert_eq!(
        env.deliver(PETR, ANNA, ItipMethod::Request, &request).await,
        ItipOutcome::Applied
    );
    assert_eq!(env.by_uid(PETR, "m-9").await.expect("o").etag, o.etag);
    assert_eq!(env.inbox_len(PETR).await, 0);

    // Устаревший SEQUENCE и чужой организатор того же UID.
    let v2 = meeting("m-9", ANNA, T14, 2, &attendee(PETR, "NEEDS-ACTION"));
    assert_eq!(
        env.deliver(PETR, ANNA, ItipMethod::Request, &itip_msg("REQUEST", &v2))
            .await,
        ItipOutcome::Applied
    );
    for (from, msg) in [
        (ANNA, itip_msg("REQUEST", &v1)),
        (
            "eve@evil.example",
            itip_msg(
                "REQUEST",
                &meeting(
                    "m-9",
                    "eve@evil.example",
                    T10,
                    9,
                    &attendee(PETR, "NEEDS-ACTION"),
                ),
            ),
        ),
        ("eve@evil.example", itip_msg("REQUEST", &v2)),
        (
            "eve@evil.example",
            itip_msg(
                "CANCEL",
                &meeting(
                    "m-9",
                    "eve@evil.example",
                    T14,
                    9,
                    &attendee(PETR, "NEEDS-ACTION"),
                ),
            ),
        ),
    ] {
        let method = if msg.contains("METHOD:CANCEL") {
            ItipMethod::Cancel
        } else {
            ItipMethod::Request
        };
        let out = env.deliver(PETR, from, method, &msg).await;
        assert!(matches!(out, ItipOutcome::Ignored(_)), "{from}: {out:?}");
    }
    assert!(env.body(PETR, "m-9").await.contains(T14));

    // CANCEL от организатора; повтор — без изменений.
    let cancel = itip_msg(
        "CANCEL",
        &meeting("m-9", ANNA, T14, 3, &attendee(PETR, "NEEDS-ACTION")),
    );
    assert_eq!(
        env.deliver(PETR, ANNA, ItipMethod::Cancel, &cancel).await,
        ItipOutcome::Applied
    );
    let o = env.by_uid(PETR, "m-9").await.expect("o");
    assert!(
        o.body.contains("STATUS:CANCELLED") && o.index.busy.is_empty(),
        "{}",
        o.body
    );
    assert_eq!(
        env.deliver(PETR, ANNA, ItipMethod::Cancel, &cancel).await,
        ItipOutcome::Applied
    );
    assert_eq!(env.by_uid(PETR, "m-9").await.expect("o").etag, o.etag);

    // REFRESH и COUNTER — во входящие; повтор перезаписывает то же сообщение.
    let mine = meeting("m-10", PETR, T10, 0, &attendee(ANNA, "NEEDS-ACTION"));
    assert_eq!(env.put(PETR, "mine.ics", &[], &mine).await.status, 201);
    for method in [
        ItipMethod::Refresh,
        ItipMethod::Counter,
        ItipMethod::Counter,
    ] {
        let msg = itip_msg(method.as_str(), &mine);
        assert_eq!(
            env.deliver(PETR, ANNA, method, &msg).await,
            ItipOutcome::Applied
        );
    }
    assert_eq!(env.inbox_len(PETR).await, 2);

    // Недопустимые сообщения.
    for (method, ics) in [
        (ItipMethod::Request, "мусор".to_owned()),
        (ItipMethod::Cancel, request.clone()),
        (ItipMethod::Publish, itip_msg("PUBLISH", &v1)),
        (
            ItipMethod::Cancel,
            itip_msg(
                "CANCEL",
                &meeting("нет", ANNA, T10, 0, &attendee(PETR, "NEEDS-ACTION")),
            ),
        ),
    ] {
        let out = env.deliver(PETR, ANNA, method, &ics).await;
        assert!(matches!(out, ItipOutcome::Ignored(_)), "{method}: {out:?}");
    }
}

#[tokio::test]
async fn reply_only_from_attendee() {
    let env = Sched::new().await;
    let v = meeting(
        "m-11",
        IVAN,
        T10,
        0,
        &format!(
            "{}{}",
            attendee(ANNA, "NEEDS-ACTION"),
            attendee(PETR, "NEEDS-ACTION")
        ),
    );
    assert_eq!(env.put(IVAN, "r.ics", &[], &v).await.status, 201);
    let before = env.by_uid(IVAN, "m-11").await.expect("o");
    let reply = |who: &str, partstat: &str| {
        itip_msg(
            "REPLY",
            &meeting("m-11", IVAN, T10, 0, &attendee(who, partstat)),
        )
    };
    // Подделка: отправитель отвечает за другого участника или не участник вовсе.
    for (from, msg) in [
        ("eve@evil.example", reply(ANNA, "ACCEPTED")),
        ("eve@evil.example", reply("eve@evil.example", "ACCEPTED")),
        (ANNA, reply(PETR, "DECLINED")),
    ] {
        let out = env.deliver(IVAN, from, ItipMethod::Reply, &msg).await;
        assert!(matches!(out, ItipOutcome::Ignored(_)), "{from}: {out:?}");
    }
    // Ответ не организатору (участнику, у которого есть копия события) не применяется.
    let out = env
        .deliver(PETR, ANNA, ItipMethod::Reply, &reply(ANNA, "ACCEPTED"))
        .await;
    assert!(matches!(out, ItipOutcome::Ignored(_)), "{out:?}");
    assert_eq!(env.by_uid(IVAN, "m-11").await.expect("o").etag, before.etag);

    // Настоящий ответ; повтор — без изменений; тег прежний.
    let ok = reply(ANNA, "ACCEPTED");
    assert_eq!(
        env.deliver(IVAN, ANNA, ItipMethod::Reply, &ok).await,
        ItipOutcome::Applied
    );
    let after = env.by_uid(IVAN, "m-11").await.expect("o");
    assert!(
        after.body.contains(&format!(
            "PARTSTAT=ACCEPTED;SCHEDULE-STATUS=2.0:mailto:{ANNA}"
        )),
        "{}",
        after.body
    );
    assert_eq!(tag(&after), tag(&before));
    assert_eq!(
        env.deliver(IVAN, ANNA, ItipMethod::Reply, &ok).await,
        ItipOutcome::Applied
    );
    assert_eq!(env.by_uid(IVAN, "m-11").await.expect("o").etag, after.etag);
    assert_eq!(env.inbox_len(IVAN).await, 0);
}

/// DAV-36: без настройки применённое приглашение остаётся во входящих для ленты уведомлений JMAP, в CalDAV
/// его не видно.
#[tokio::test]
async fn keep_processed_by_default() {
    assert!(DavConfig::default().schedule_keep_processed);
    let env = Sched::with_config(DavConfig::default()).await;
    let request = itip_msg(
        "REQUEST",
        &meeting("m-40", ANNA, T10, 0, &attendee(PETR, "NEEDS-ACTION")),
    );
    assert_eq!(
        env.deliver(PETR, ANNA, ItipMethod::Request, &request).await,
        ItipOutcome::Applied
    );
    assert!(
        env.by_uid(PETR, "m-40").await.is_some(),
        "приглашение в календаре"
    );
    let items = env
        .store
        .inbox_list(env.scope(PETR))
        .await
        .expect("входящие")
        .items;
    assert_eq!(items.len(), 1, "применённое хранится для ленты");
    assert!(
        crate::deliver::processed(&items[0].name),
        "{}",
        items[0].name
    );
    let inbox = format!("{}inbox/", home(PETR));
    let r = env
        .send(
            PETR,
            "PROPFIND",
            &inbox,
            &[("depth", "1")],
            "<d:propfind xmlns:d=\"DAV:\"><d:prop><d:getetag/></d:prop></d:propfind>",
        )
        .await;
    assert!(!r.body.contains(&items[0].name), "{}", r.body);
}

#[tokio::test]
async fn keep_processed_and_instance_cancel() {
    let env = Sched::with_config(DavConfig {
        schedule_keep_processed: true,
        ..DavConfig::default()
    })
    .await;
    let series = meeting(
        "m-12",
        ANNA,
        T10,
        0,
        &format!(
            "RRULE:FREQ=DAILY;COUNT=3\r\n{}",
            attendee(PETR, "NEEDS-ACTION")
        ),
    );
    let request = itip_msg("REQUEST", &series);
    assert_eq!(
        env.deliver(PETR, ANNA, ItipMethod::Request, &request).await,
        ItipOutcome::Applied
    );
    assert_eq!(
        env.deliver(PETR, ANNA, ItipMethod::Request, &request).await,
        ItipOutcome::Applied
    );
    assert_eq!(
        env.inbox_len(PETR).await,
        1,
        "сохранено по настройке, повтор не дублирует"
    );
    let o = env.by_uid(PETR, "m-12").await.expect("o");
    assert!(o.index.recurring && o.index.busy.is_empty());
    assert_eq!(env.busy_in_january(PETR).await, 3);
    // Отмена одного вхождения — исключение из ряда.
    let one = itip_msg(
        "CANCEL",
        &meeting(
            "m-12",
            ANNA,
            "20260106T100000Z",
            0,
            &format!(
                "RECURRENCE-ID:20260106T100000Z\r\n{}",
                attendee(PETR, "NEEDS-ACTION")
            ),
        ),
    );
    assert_eq!(
        env.deliver(PETR, ANNA, ItipMethod::Cancel, &one).await,
        ItipOutcome::Applied
    );
    let o = env.by_uid(PETR, "m-12").await.expect("o");
    assert!(
        o.body.contains("EXDATE:20260106T100000Z") && !o.body.contains("CANCELLED"),
        "{}",
        o.body
    );
    assert!(o.index.recurring && o.index.busy.is_empty());
    assert_eq!(env.busy_in_january(PETR).await, 2);
}

// ---------- DAV-16: занятость ----------

/// Запрос занятости, как его отправляет Календарь macOS («Доступность»).
fn freebusy_request(attendees: &[&str]) -> String {
    let list: String = attendees
        .iter()
        .map(|a| format!("ATTENDEE:mailto:{a}\r\n"))
        .collect();
    format!(
        "BEGIN:VCALENDAR\r\nCALSCALE:GREGORIAN\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\n\
         PRODID:-//Apple Inc.//macOS 15.1//EN\r\nBEGIN:VFREEBUSY\r\nUID:4FD3AD92-6CC1\r\n\
         DTEND:20260108T000000Z\r\n{list}DTSTART:20260105T000000Z\r\n\
         X-CALENDARSERVER-MASK-UID:m-1\r\nDTSTAMP:20260104T120000Z\r\n\
         ORGANIZER:mailto:{IVAN}\r\nEND:VFREEBUSY\r\nEND:VCALENDAR\r\n"
    )
}

#[tokio::test]
async fn freebusy_two_users() {
    let env = Sched::new().await;
    // Пётр: ежедневная встреча с пропуском 6-го и переносом 7-го (под вопросом), прозрачное и отменённое события.
    let series = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//RU\r\nBEGIN:VEVENT\r\nUID:f-1\r\n\
        DTSTAMP:20260101T000000Z\r\nDTSTART:20260105T100000Z\r\nDTEND:20260105T110000Z\r\n\
        RRULE:FREQ=DAILY;COUNT=4\r\nEXDATE:20260106T100000Z\r\nSUMMARY:Стендап\r\nEND:VEVENT\r\n\
        BEGIN:VEVENT\r\nUID:f-1\r\nRECURRENCE-ID:20260107T100000Z\r\nDTSTAMP:20260101T000000Z\r\n\
        DTSTART:20260107T150000Z\r\nDTEND:20260107T160000Z\r\nSTATUS:TENTATIVE\r\nSUMMARY:Стендап\r\n\
        END:VEVENT\r\nEND:VCALENDAR\r\n";
    assert_eq!(env.put(PETR, "f1.ics", &[], series).await.status, 201);
    for (name, uid, extra) in [
        ("f2.ics", "f-2", "TRANSP:TRANSPARENT\r\n"),
        ("f3.ics", "f-3", "STATUS:CANCELLED\r\n"),
    ] {
        let body = super::event(uid, "Не занимает").replace("SUMMARY", &format!("{extra}SUMMARY"));
        assert_eq!(env.put(PETR, name, &[], &body).await.status, 201);
    }
    // Ольга: одна встреча в интервале и одна вне его.
    for (name, uid, day) in [("o1.ics", "o-1", "20260106"), ("o2.ics", "o-2", "20260201")] {
        let body = super::event(uid, "Встреча").replace("20260105", day);
        assert_eq!(env.put(OLGA, name, &[], &body).await.status, 201);
    }
    let outbox = format!("{}outbox/", home(IVAN));
    let r = env
        .send(
            IVAN,
            "POST",
            &outbox,
            &[("content-type", "text/calendar; charset=utf-8")],
            &freebusy_request(&[PETR, OLGA, ANNA]),
        )
        .await;
    assert_eq!(r.status, 200, "{}", r.body);
    let shown: String = r
        .show()
        .replace("&#13;", "")
        .lines()
        .filter(|l| !l.starts_with("DTSTAMP:"))
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!("freebusy_macos", shown);

    // Недопустимые запросы.
    let not_mine = freebusy_request(&[PETR]).replace(
        &format!("ORGANIZER:mailto:{IVAN}"),
        "ORGANIZER:mailto:x@y.z",
    );
    let reversed = freebusy_request(&[PETR]).replace("DTEND:20260108", "DTEND:20260101");
    for (body, want) in [
        (not_mine.as_str(), "valid-organizer"),
        (reversed.as_str(), "valid-scheduling-message"),
        (
            meeting("m", IVAN, T10, 0, "").as_str(),
            "valid-scheduling-message",
        ),
        ("мусор", "valid-calendar-data"),
    ] {
        let r = env.send(IVAN, "POST", &outbox, &[], body).await;
        assert_eq!(r.status, 403, "{want}");
        assert!(r.body.contains(want), "{want}: {}", r.body);
    }
    let inbox = format!("{}inbox/", home(IVAN));
    assert_eq!(env.send(IVAN, "POST", &inbox, &[], "x").await.status, 405);
}

// ---------- DAV-23: адреса учётной записи ----------

/// Псевдоним вошедшего — второй `mailto:` в `calendar-user-address-set` (основной первым, затем ссылка на
/// принципала) и свой адрес организатора при разборе участников.
#[tokio::test]
async fn principal_addresses_with_alias() {
    let env = Sched::new().await;
    let alias = "i.ivanov@example.org";
    env.meta
        .upsert(
            env.tenant,
            AccountUpsert {
                domain: DomainId::new(),
                login: IVAN.to_owned(),
                display_name: IVAN.to_owned(),
                external_id: None,
                enabled: true,
                aliases: vec!["I.Ivanov@Example.org".to_owned()],
                kind: AccountKind::User,
                hidden_from_gal: false,
                resource_type: None,
            },
        )
        .await
        .expect("псевдоним");
    let body = r#"<?xml version="1.0" encoding="utf-8"?>
<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
  <d:prop><c:calendar-user-address-set/></d:prop>
</d:propfind>"#;
    let r = env
        .send(
            IVAN,
            "PROPFIND",
            &format!("/dav/principals/{IVAN}/"),
            &[("depth", "0")],
            body,
        )
        .await;
    insta::assert_snapshot!(r.show(), @r#"
    207 Multi-Status
    <?xml version="1.0" encoding="utf-8"?>
    <d:multistatus xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav" xmlns:card="urn:ietf:params:xml:ns:carddav" xmlns:cs="http://calendarserver.org/ns/" xmlns:ical="http://apple.com/ns/ical/">
    <d:response>
    <d:href>/dav/principals/ivan@example.org/</d:href>
    <d:propstat>
    <d:prop>
    <c:calendar-user-address-set>
    <d:href>mailto:ivan@example.org</d:href>
    <d:href>mailto:i.ivanov@example.org</d:href>
    <d:href>/dav/principals/ivan@example.org/</d:href>
    </c:calendar-user-address-set>
    </d:prop>
    <d:status>HTTP/1.1 200 OK</d:status>
    </d:propstat>
    </d:response>
    </d:multistatus>
    "#);
    // Учётная запись без псевдонимов — только адрес входа.
    let r = env
        .send(
            PETR,
            "PROPFIND",
            &format!("/dav/principals/{PETR}/"),
            &[("depth", "0")],
            body,
        )
        .await;
    assert!(
        r.body.contains(&format!(
            "<c:calendar-user-address-set><d:href>mailto:{PETR}</d:href><d:href>/dav/principals/{PETR}/</d:href></c:calendar-user-address-set>"
        )),
        "{}",
        r.body
    );

    // Запрос занятости от псевдонима — организатор свой.
    let outbox = format!("{}outbox/", home(IVAN));
    let request = freebusy_request(&[PETR]).replace(
        &format!("ORGANIZER:mailto:{IVAN}"),
        &format!("ORGANIZER:mailto:{alias}"),
    );
    let r = env.send(IVAN, "POST", &outbox, &[], &request).await;
    assert_eq!(r.status, 200, "{}", r.body);
}

// ---------- DAV-25: календарь по умолчанию ----------

/// Календарь и книга, созданные при первом обращении, — по умолчанию (`is_default`); `schedule-default-calendar-URL`
/// — по признаку коллекции, назначенному и не через CalDAV (JMAP).
#[tokio::test]
async fn default_calendar_by_flag() {
    let env = Sched::new().await;
    let scope = env.scope(IVAN);
    env.send(IVAN, "PROPFIND", &home(IVAN), &[("depth", "0")], "")
        .await;
    env.send(
        IVAN,
        "PROPFIND",
        &format!("/dav/addressbooks/{IVAN}/"),
        &[("depth", "0")],
        "",
    )
    .await;
    let get = |kind: CollectionKind, name: &'static str| {
        let store = env.store.clone();
        async move {
            store
                .collection_by_name(scope, kind, name)
                .await
                .expect("хранилище")
                .expect("коллекция")
        }
    };
    assert!(get(CollectionKind::Calendar, "calendar").await.is_default);
    assert!(!get(CollectionKind::Calendar, "tasks").await.is_default);
    assert!(
        get(CollectionKind::AddressBook, "contacts")
            .await
            .is_default
    );

    let inbox = format!("{}inbox/", home(IVAN));
    let body = r#"<d:propfind xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
        <d:prop><c:schedule-default-calendar-URL/></d:prop></d:propfind>"#;
    let default_url = |r: &Resp| {
        r.body
            .split("<c:schedule-default-calendar-URL><d:href>")
            .nth(1)
            .and_then(|s| s.split("</d:href>").next())
            .map(str::to_owned)
    };
    let r = env
        .send(IVAN, "PROPFIND", &inbox, &[("depth", "0")], body)
        .await;
    assert_eq!(
        default_url(&r).as_deref(),
        Some("/dav/calendars/ivan@example.org/calendar/"),
        "{}",
        r.body
    );
    // Другой календарь назначен по умолчанию (как JMAP `isDefault`): признак у прежнего снимает хранилище.
    let work = tp_contracts::dav::Collection {
        is_default: true,
        components: BTreeSet::from([tp_contracts::dav::Component::Event]),
        ..tp_contracts::dav::Collection::new(CollectionKind::Calendar, "work")
    };
    env.store
        .create_collection(scope, work)
        .await
        .expect("назначен");
    assert!(!get(CollectionKind::Calendar, "calendar").await.is_default);
    let r = env
        .send(IVAN, "PROPFIND", &inbox, &[("depth", "0")], body)
        .await;
    assert_eq!(
        default_url(&r).as_deref(),
        Some("/dav/calendars/ivan@example.org/work/"),
        "{}",
        r.body
    );
}

// ---------- DAV-30: участник по псевдониму и адресу группы ----------

/// Псевдоним коллеги — его учётная запись (приглашение не уходит внешним письмом, приёмка 0.2.0); адрес группы
/// рассылки — её учётные записи участниками с `MEMBER`, сама группа сообщений не получает.
#[tokio::test]
async fn invite_alias_and_group() {
    use tp_contracts::groups::{DistributionGroup, GroupMember, GroupStore, SenderRestriction};

    let env = Sched::new().await;
    let alias = "p.petrov@example.org";
    env.meta
        .upsert(
            env.tenant,
            AccountUpsert {
                domain: DomainId::new(),
                login: PETR.to_owned(),
                display_name: PETR.to_owned(),
                external_id: None,
                enabled: true,
                aliases: vec![alias.to_owned()],
                kind: AccountKind::User,
                hidden_from_gal: false,
                resource_type: None,
            },
        )
        .await
        .expect("псевдоним");
    let team = "team@example.org";
    env.groups
        .put(DistributionGroup {
            id: tp_contracts::GroupId::new(),
            tenant: env.tenant,
            name: "Команда".to_owned(),
            address: Some(team.to_owned()),
            external_id: None,
            members: vec![
                GroupMember::Account(env.scope(OLGA).account),
                GroupMember::Account(env.scope(IVAN).account),
            ],
            senders: SenderRestriction::Anyone,
            moderators: Vec::new(),
            hidden_from_gal: false,
        })
        .await
        .expect("группа");

    let body = meeting(
        "m-30",
        IVAN,
        T10,
        0,
        &format!(
            "{}ATTENDEE;CN=Команда:mailto:{team}\r\n",
            attendee(alias, "NEEDS-ACTION")
        ),
    );
    let r = env.put(IVAN, "g.ics", &[], &body).await;
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(env.sender.sent().is_empty(), "внешних писем нет");

    // Псевдоним — событие в календаре владельца.
    let petr = env
        .by_uid(PETR, "m-30")
        .await
        .expect("у владельца псевдонима");
    assert!(
        petr.body
            .contains(&format!("PARTSTAT=NEEDS-ACTION:mailto:{alias}")),
        "{}",
        petr.body
    );
    // Группа — у каждого её участника, кроме самого организатора.
    let olga = env.by_uid(OLGA, "m-30").await.expect("у участника группы");
    assert!(
        olga.body.contains("MEMBER=") && olga.body.contains(&format!("mailto:{OLGA}")),
        "{}",
        olga.body
    );
    let org = env.by_uid(IVAN, "m-30").await.expect("у организатора");
    assert!(org.body.contains("CUTYPE=GROUP"), "{}", org.body);
    assert_eq!(
        org.body.matches("SCHEDULE-STATUS=1.2").count(),
        2,
        "псевдоним и участник группы; группе не отправляется: {}",
        org.body
    );
    assert_eq!(
        org.body.matches(&format!("mailto:{IVAN}")).count(),
        1,
        "организатор участником не добавлен: {}",
        org.body
    );
    assert_eq!(
        r.header("schedule-tag"),
        Some(format!("\"{}\"", tag(&org)).as_str()),
        "тег — объекта с раскрытой группой"
    );

    // Ответ с псевдонима доходит организатору; повторная запись группу заново не раскрывает.
    let accepted = petr.body.replace(
        &format!("PARTSTAT=NEEDS-ACTION:mailto:{alias}"),
        &format!("PARTSTAT=ACCEPTED:mailto:{alias}"),
    );
    let r = env
        .send(PETR, "PUT", &object(PETR, &petr.name), &[], &accepted)
        .await;
    assert_eq!(r.status, 204, "{}", r.body);
    let org = env.by_uid(IVAN, "m-30").await.expect("у организатора");
    assert!(
        org.body.contains(&format!(
            "PARTSTAT=ACCEPTED;SCHEDULE-STATUS=2.0:mailto:{alias}"
        )),
        "{}",
        org.body
    );
    assert_eq!(org.body.matches(&format!("mailto:{OLGA}")).count(), 1);
    assert!(env.sender.sent().is_empty());
}

// ---------- DAV-35: вложенные группы участника ----------

/// Группа A содержит группу B, B — снова A (цикл A→B→A): приглашение на A доходит до участников обеих групп, каждому
/// по разу, раскрытие не зацикливается.
#[tokio::test]
async fn invite_nested_group_with_cycle() {
    use tp_contracts::GroupId;
    use tp_contracts::groups::{DistributionGroup, GroupMember, GroupStore, SenderRestriction};

    let env = Sched::new().await;
    let (a, b) = (GroupId::new(), GroupId::new());
    let group = |id, name: &str, address: Option<&str>, members| DistributionGroup {
        id,
        tenant: env.tenant,
        name: name.to_owned(),
        address: address.map(str::to_owned),
        external_id: None,
        members,
        senders: SenderRestriction::Anyone,
        moderators: Vec::new(),
        hidden_from_gal: false,
    };
    let team = "team@example.org";
    env.groups
        .put(group(
            a,
            "Команда",
            Some(team),
            vec![
                GroupMember::Account(env.scope(PETR).account),
                GroupMember::Group(b),
            ],
        ))
        .await
        .expect("группа A");
    env.groups
        .put(group(
            b,
            "Отдел",
            None,
            vec![
                GroupMember::Account(env.scope(OLGA).account),
                GroupMember::Group(a),
            ],
        ))
        .await
        .expect("группа B");

    let body = meeting(
        "m-35",
        IVAN,
        T10,
        0,
        &format!(
            "ATTENDEE;CN=Команда:mailto:{team}
"
        ),
    );
    let r = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        env.put(IVAN, "n.ics", &[], &body),
    )
    .await
    .expect("раскрытие групп не зацикливается");
    assert_eq!(r.status, 201, "{}", r.body);
    assert!(env.sender.sent().is_empty(), "внешних писем нет");

    for login in [PETR, OLGA] {
        let o = env.by_uid(login, "m-35").await.expect("у участника");
        assert!(o.body.contains(&format!("mailto:{login}")), "{}", o.body);
    }
    let org = env.by_uid(IVAN, "m-35").await.expect("у организатора");
    assert_eq!(
        org.body.matches("SCHEDULE-STATUS=1.2").count(),
        2,
        "участник A и участник вложенной B; группе не отправляется: {}",
        org.body
    );
    for login in [PETR, OLGA] {
        assert_eq!(
            org.body.matches(&format!("mailto:{login}")).count(),
            1,
            "участник добавлен один раз: {}",
            org.body
        );
    }
    assert_eq!(
        org.body.matches(&format!("mailto:{IVAN}")).count(),
        1,
        "организатор участником не добавлен: {}",
        org.body
    );
}

// ---------- DAV-31: частные случаи RFC 6638 ----------

/// Удаление календаря целиком: организатор — `CANCEL` участникам, участник — отказ организатору.
#[tokio::test]
async fn delete_calendar_cancels_and_declines() {
    let env = Sched::new().await;
    let v = meeting("m-31", IVAN, T10, 0, &two("NEEDS-ACTION"));
    assert_eq!(env.put(IVAN, "m.ics", &[], &v).await.status, 201);
    // Ольга удаляет свой календарь с приглашением — отказ.
    let r = env
        .send(OLGA, "DELETE", &format!("{}calendar/", home(OLGA)), &[], "")
        .await;
    assert_eq!(r.status, 204);
    let org = env.body(IVAN, "m-31").await;
    assert!(
        org.contains(&format!(
            "PARTSTAT=DECLINED;SCHEDULE-STATUS=2.0:mailto:{OLGA}"
        )),
        "{org}"
    );
    // Иван удаляет календарь со встречей — отмена.
    let r = env
        .send(IVAN, "DELETE", &format!("{}calendar/", home(IVAN)), &[], "")
        .await;
    assert_eq!(r.status, 204);
    assert!(env.body(PETR, "m-31").await.contains("STATUS:CANCELLED"));
}

/// Участник меняет в своей копии только своё участие и личные свойства, иначе — 403.
#[tokio::test]
async fn attendee_changes_only_own_participation() {
    let env = Sched::new().await;
    let v = meeting("m-32", IVAN, T10, 0, &two("NEEDS-ACTION"));
    assert_eq!(env.put(IVAN, "m.ics", &[], &v).await.status, 201);
    let mine = env.by_uid(PETR, "m-32").await.expect("копия");
    let path = object(PETR, &mine.name);
    let olga_needs = format!("PARTSTAT=NEEDS-ACTION:mailto:{OLGA}");
    for (what, body) in [
        ("описание", mine.body.replace("Планёрка", "Другое")),
        ("время", mine.body.replace(T10, T14)),
        (
            "чужой PARTSTAT",
            mine.body
                .replace(&olga_needs, &format!("PARTSTAT=ACCEPTED:mailto:{OLGA}")),
        ),
    ] {
        let r = env.send(PETR, "PUT", &path, &[], &body).await;
        assert_eq!(r.status, 403, "{what}");
        assert!(
            r.body.contains("allowed-attendee-scheduling-object-change"),
            "{what}: {}",
            r.body
        );
    }
    assert_eq!(env.by_uid(PETR, "m-32").await.expect("o").etag, mine.etag);
    // Свой ответ, прозрачность, напоминание и свойство клиента — можно.
    let own = mine
        .body
        .replace(
            &format!("PARTSTAT=NEEDS-ACTION:mailto:{PETR}"),
            &format!("PARTSTAT=ACCEPTED:mailto:{PETR}"),
        )
        .replace(
            "END:VEVENT",
            "TRANSP:TRANSPARENT\r\nX-MOZ-LASTACK:20260101T000000Z\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\n\
             TRIGGER:-PT15M\r\nDESCRIPTION:Скоро\r\nEND:VALARM\r\nEND:VEVENT",
        );
    let r = env.send(PETR, "PUT", &path, &[], &own).await;
    assert_eq!(r.status, 204, "{}", r.body);
    let org = env.body(IVAN, "m-32").await;
    assert!(
        org.contains(&format!(
            "PARTSTAT=ACCEPTED;SCHEDULE-STATUS=2.0:mailto:{PETR}"
        )),
        "{org}"
    );
    // Организатор меняет что угодно.
    let v2 = meeting("m-32", IVAN, T14, 1, &two("NEEDS-ACTION"));
    assert_eq!(env.put(IVAN, "m.ics", &[], &v2).await.status, 204);
}

/// Ответ на отдельное вхождение ряда — переопределение у организатора; `ADD` — новое вхождение; VTODO — в список
/// задач.
#[tokio::test]
async fn instance_reply_add_and_todo() {
    let env = Sched::new().await;
    let series = meeting(
        "m-33",
        IVAN,
        T10,
        0,
        &format!(
            "RRULE:FREQ=DAILY;COUNT=3\r\n{}",
            attendee(PETR, "NEEDS-ACTION")
        ),
    );
    assert_eq!(env.put(IVAN, "s.ics", &[], &series).await.status, 201);
    let one = itip_msg(
        "REPLY",
        &meeting(
            "m-33",
            IVAN,
            "20260106T100000Z",
            0,
            &format!(
                "RECURRENCE-ID:20260106T100000Z\r\n{}",
                attendee(PETR, "DECLINED")
            ),
        ),
    );
    assert_eq!(
        env.deliver(IVAN, PETR, ItipMethod::Reply, &one).await,
        ItipOutcome::Applied
    );
    let org = env.by_uid(IVAN, "m-33").await.expect("o");
    assert_eq!(org.body.matches("BEGIN:VEVENT").count(), 2, "{}", org.body);
    assert!(
        org.body.contains("RECURRENCE-ID:20260106T100000Z")
            && org.body.contains(&format!(
                "PARTSTAT=DECLINED;SCHEDULE-STATUS=2.0:mailto:{PETR}"
            )),
        "{}",
        org.body
    );
    assert_eq!(
        env.inbox_len(IVAN).await,
        0,
        "ответ применён, не во входящих"
    );
    // Повтор — без изменений.
    assert_eq!(
        env.deliver(IVAN, PETR, ItipMethod::Reply, &one).await,
        ItipOutcome::Applied
    );
    assert_eq!(env.by_uid(IVAN, "m-33").await.expect("o").etag, org.etag);

    // ADD: новое вхождение ряда внешнего организатора (у Ольги других встреч нет).
    let theirs = meeting(
        "m-34",
        ANNA,
        T10,
        0,
        &format!(
            "RRULE:FREQ=DAILY;COUNT=2\r\n{}",
            attendee(OLGA, "NEEDS-ACTION")
        ),
    );
    assert_eq!(
        env.deliver(
            OLGA,
            ANNA,
            ItipMethod::Request,
            &itip_msg("REQUEST", &theirs)
        )
        .await,
        ItipOutcome::Applied
    );
    let add = itip_msg(
        "ADD",
        &meeting(
            "m-34",
            ANNA,
            "20260110T100000Z",
            0,
            &attendee(OLGA, "NEEDS-ACTION"),
        ),
    );
    assert_eq!(
        env.deliver(OLGA, ANNA, ItipMethod::Add, &add).await,
        ItipOutcome::Applied
    );
    let o = env.by_uid(OLGA, "m-34").await.expect("o");
    assert!(
        o.body.contains("RDATE:20260110T100000Z")
            && o.body.contains("RECURRENCE-ID:20260110T100000Z"),
        "{}",
        o.body
    );
    assert_eq!(env.busy_in_january(OLGA).await, 3);
    assert_eq!(
        env.deliver(OLGA, ANNA, ItipMethod::Add, &add).await,
        ItipOutcome::Applied
    );
    assert_eq!(env.by_uid(OLGA, "m-34").await.expect("o").etag, o.etag);
    assert_eq!(env.inbox_len(OLGA).await, 0);
    // ADD без ряда — во входящие клиенту; чужой организатор — отказ.
    let orphan = add.replace("m-34", "m-35");
    assert_eq!(
        env.deliver(OLGA, ANNA, ItipMethod::Add, &orphan).await,
        ItipOutcome::Applied
    );
    assert_eq!(env.inbox_len(OLGA).await, 1);
    let forged = add.replace(&format!("mailto:{ANNA}"), "mailto:eve@evil.example");
    let out = env
        .deliver(OLGA, "eve@evil.example", ItipMethod::Add, &forged)
        .await;
    assert!(matches!(out, ItipOutcome::Ignored(_)), "{out:?}");

    // VTODO — в список задач по умолчанию.
    let todo = format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:REQUEST\r\nPRODID:-//test//RU\r\nBEGIN:VTODO\r\nUID:t-36\r\n\
         DTSTAMP:20260101T000000Z\r\nDUE:20260110T100000Z\r\nSUMMARY:Отчёт\r\nORGANIZER:mailto:{ANNA}\r\n{}\
         END:VTODO\r\nEND:VCALENDAR\r\n",
        attendee(PETR, "NEEDS-ACTION")
    );
    assert_eq!(
        env.deliver(PETR, ANNA, ItipMethod::Request, &todo).await,
        ItipOutcome::Applied
    );
    let task = env.by_uid(PETR, "t-36").await.expect("задача");
    assert_eq!(task.index.component, tp_contracts::dav::Component::Todo);
    let coll = env
        .store
        .get_collection(env.scope(PETR), task.collection)
        .await
        .expect("хранилище")
        .expect("коллекция");
    assert!(
        coll.components
            .contains(&tp_contracts::dav::Component::Todo),
        "{coll:?}"
    );
}

/// `REPORT` во входящих: `calendar-query` и `sync-collection` по журналу входящих.
#[tokio::test]
async fn inbox_reports() {
    let env = Sched::new().await;
    let mine = meeting("m-37", PETR, T10, 0, &attendee(ANNA, "NEEDS-ACTION"));
    assert_eq!(env.put(PETR, "mine.ics", &[], &mine).await.status, 201);
    let counter = |seq: u32| {
        itip_msg(
            "COUNTER",
            &meeting("m-37", PETR, T14, seq, &attendee(ANNA, "NEEDS-ACTION")),
        )
    };
    assert_eq!(
        env.deliver(PETR, ANNA, ItipMethod::Counter, &counter(0))
            .await,
        ItipOutcome::Applied
    );
    let inbox = format!("{}inbox/", home(PETR));
    let query = |comp: &str| {
        format!(
            "<c:calendar-query xmlns:d=\"DAV:\" xmlns:c=\"urn:ietf:params:xml:ns:caldav\">\
             <d:prop><d:getetag/><c:calendar-data/></d:prop>\
             <c:filter><c:comp-filter name=\"VCALENDAR\"><c:comp-filter name=\"{comp}\"/></c:comp-filter>\
             </c:filter></c:calendar-query>"
        )
    };
    let r = env
        .send(PETR, "REPORT", &inbox, &[("depth", "1")], &query("VEVENT"))
        .await;
    assert_eq!(r.status, 207, "{}", r.body);
    assert!(r.body.contains("METHOD:COUNTER"), "{}", r.body);
    let r = env
        .send(PETR, "REPORT", &inbox, &[("depth", "1")], &query("VTODO"))
        .await;
    assert_eq!(r.status, 207);
    assert!(!r.body.contains("<d:response>"), "{}", r.body);

    let sync = |token: &str| {
        format!(
            "<d:sync-collection xmlns:d=\"DAV:\"><d:sync-token>{token}</d:sync-token>\
             <d:sync-level>1</d:sync-level><d:prop><d:getetag/></d:prop></d:sync-collection>"
        )
    };
    let token_of = |r: &Resp| {
        r.body
            .split("<d:sync-token>")
            .nth(1)
            .and_then(|s| s.split("</d:sync-token>").next())
            .expect("токен")
            .to_owned()
    };
    let r = env.send(PETR, "REPORT", &inbox, &[], &sync("")).await;
    assert_eq!(r.status, 207, "{}", r.body);
    assert_eq!(r.body.matches("<d:response>").count(), 1, "{}", r.body);
    let first = token_of(&r);
    assert_eq!(
        env.deliver(PETR, ANNA, ItipMethod::Counter, &counter(1))
            .await,
        ItipOutcome::Applied
    );
    let r = env.send(PETR, "REPORT", &inbox, &[], &sync(&first)).await;
    assert_eq!(r.status, 207, "{}", r.body);
    assert_eq!(
        r.body.matches("<d:response>").count(),
        1,
        "только новое: {}",
        r.body
    );
    let second = token_of(&r);
    let r = env.send(PETR, "REPORT", &inbox, &[], &sync(&second)).await;
    assert_eq!(r.body.matches("<d:response>").count(), 0, "{}", r.body);
    // Удалённое сообщение: прежний токен недействителен, клиент синхронизируется заново.
    let name = env
        .store
        .inbox_list(env.scope(PETR))
        .await
        .expect("входящие")
        .items[0]
        .name
        .clone();
    let r = env
        .send(PETR, "DELETE", &format!("{inbox}{name}"), &[], "")
        .await;
    assert_eq!(r.status, 204);
    let r = env.send(PETR, "REPORT", &inbox, &[], &sync(&second)).await;
    assert_eq!(r.status, 403);
    assert!(r.body.contains("valid-sync-token"), "{}", r.body);
    let r = env.send(PETR, "REPORT", &inbox, &[], &sync("")).await;
    assert_eq!(r.body.matches("<d:response>").count(), 1, "{}", r.body);
}

// ---------- DAV-32: индекс для записи; срок обработанных ----------

/// Индекс `index::for_write` совпадает с индексом объекта, записанного по CalDAV; применённые сообщения при
/// `schedule_keep_processed` не видны в CalDAV и удаляются по сроку.
#[tokio::test]
async fn for_write_and_processed_ttl() {
    let cfg = DavConfig {
        schedule_keep_processed: true,
        ..DavConfig::default()
    };
    let env = Sched::with_config(cfg.clone()).await;
    let v = meeting("m-38", IVAN, T10, 0, &attendee(PETR, "NEEDS-ACTION"));
    assert_eq!(env.put(IVAN, "m.ics", &[], &v).await.status, 201);
    let org = env.by_uid(IVAN, "m-38").await.expect("o");
    let ical = ICalendar::parse(&org.body).expect("ical");
    let idx = crate::index::for_write(&ical, &cfg.index_options(OffsetDateTime::now_utc()))
        .expect("индекс");
    assert_eq!(idx.schedule_tag, org.index.schedule_tag);
    assert_eq!((idx.span, &idx.busy), (org.index.span, &org.index.busy));
    assert!(idx.schedule_tag.is_some() && idx.busy.len() == 1);

    // Применённое приглашение хранится для JMAP, но в CalDAV не видно; необработанное — видно.
    assert_eq!(env.inbox_len(PETR).await, 1);
    let mine = meeting("m-39", PETR, T10, 0, &attendee(ANNA, "NEEDS-ACTION"));
    assert_eq!(env.put(PETR, "mine.ics", &[], &mine).await.status, 201);
    let counter = itip_msg(
        "COUNTER",
        &meeting("m-39", PETR, T14, 0, &attendee(ANNA, "NEEDS-ACTION")),
    );
    assert_eq!(
        env.deliver(PETR, ANNA, ItipMethod::Counter, &counter).await,
        ItipOutcome::Applied
    );
    let items = env
        .store
        .inbox_list(env.scope(PETR))
        .await
        .expect("входящие")
        .items;
    assert_eq!(items.len(), 2);
    let inbox = format!("{}inbox/", home(PETR));
    let r = env
        .send(
            PETR,
            "PROPFIND",
            &inbox,
            &[("depth", "1")],
            "<d:propfind xmlns:d=\"DAV:\"><d:prop><d:getetag/></d:prop></d:propfind>",
        )
        .await;
    for i in &items {
        let href = format!("{inbox}{}", i.name);
        let shown = r.body.contains(&href);
        assert_eq!(
            shown,
            !crate::deliver::processed(&i.name),
            "{}: {}",
            i.name,
            r.body
        );
        let got = env.send(PETR, "GET", &href, &[], "").await;
        assert_eq!(got.status == 200, shown);
    }

    // Очистка: обработанные старше срока удаляются, необработанные остаются.
    let now = OffsetDateTime::now_utc();
    let purged = env
        .inbox
        .purge_processed(env.scope(PETR), now)
        .await
        .expect("очистка");
    assert_eq!(purged, 0, "срок не вышел");
    let later = now + time::Duration::days(31);
    let purged = env
        .inbox
        .purge_processed(env.scope(PETR), later)
        .await
        .expect("очистка");
    assert_eq!(purged, 1);
    let left = env
        .store
        .inbox_list(env.scope(PETR))
        .await
        .expect("входящие")
        .items;
    assert_eq!(left.len(), 1);
    assert!(!crate::deliver::processed(&left[0].name));
}
