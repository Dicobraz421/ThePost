//! Тесты HTTP DAV на `MemDav` и `FakeAuthenticator` через `tower::ServiceExt` (без сети).

use std::sync::Arc;

use axum::Router;
use axum::body::{Body, to_bytes};
use axum::http::{HeaderMap, Method, Request, StatusCode};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tower::ServiceExt;
use tp_contracts::TenantId;
use tp_contracts::dav::{Collection, CollectionKind, DavStore};
use tp_contracts::meta::AccountScope;
use tp_testkit::{FakeAuthenticator, MemDav, principal};

use crate::{DavConfig, Deps, router};

mod auth;
mod caldav_home;
mod card_query;
mod card_versions;
mod carddav_home;
mod multiget;
mod preconditions;
mod propfind;
mod query;
mod resource;
mod sync;

/// Вошедший пользователь.
pub(crate) const LOGIN: &str = "ivan@example.org";
/// Дом календарей вошедшего.
pub(crate) const CALS: &str = "/dav/calendars/ivan@example.org/";
/// Дом адресных книг вошедшего.
pub(crate) const BOOKS: &str = "/dav/addressbooks/ivan@example.org/";

/// Сервер с хранилищем в памяти: пользователь [`LOGIN`] (пароль `secret`, токен `token-1`) и
/// `petr@example.org` той же организации.
pub(crate) struct Env {
    pub app: Router,
    pub store: MemDav,
    pub auth: FakeAuthenticator,
    pub scope: AccountScope,
}

impl Env {
    pub(crate) fn new() -> Self {
        Self::with_config(DavConfig::default())
    }

    pub(crate) fn with_config(cfg: DavConfig) -> Self {
        let tenant = TenantId::new();
        let auth = FakeAuthenticator::default();
        let me = principal(tenant, LOGIN);
        auth.add_password(LOGIN, "secret", me.clone());
        auth.add_token("token-1", me.clone());
        auth.add_password(
            "petr@example.org",
            "secret2",
            principal(tenant, "petr@example.org"),
        );
        let store = MemDav::new();
        let app = router(
            Deps {
                auth: Arc::new(auth.clone()),
                store: Arc::new(store.clone()),
            },
            cfg,
        );
        Self {
            app,
            store,
            auth,
            scope: AccountScope {
                tenant,
                account: me.account,
            },
        }
    }

    /// Запрос от [`LOGIN`] (Basic).
    pub(crate) async fn send(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Resp {
        let mut b = Request::builder()
            .method(Method::from_bytes(method.as_bytes()).expect("метод"))
            .uri(path)
            .header("authorization", basic(LOGIN, "secret"));
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        self.raw(b.body(Body::from(body.to_owned())).expect("запрос"))
            .await
    }

    /// `PROPFIND` с `Depth`.
    pub(crate) async fn propfind(&self, path: &str, depth: &str, body: &str) -> Resp {
        self.send("PROPFIND", path, &[("depth", depth)], body).await
    }

    /// Запрос как есть.
    pub(crate) async fn raw(&self, req: Request<Body>) -> Resp {
        let resp = self.app.clone().oneshot(req).await.expect("infallible");
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.expect("тело");
        Resp {
            status,
            headers,
            body: String::from_utf8(bytes.to_vec()).expect("utf-8"),
        }
    }

    /// Календарь `name` в хранилище: «Работа», описание и цвет.
    pub(crate) async fn calendar(&self, name: &str) -> Collection {
        let c = Collection {
            display_name: "Работа".into(),
            description: Some("Рабочие встречи".into()),
            color: Some("#FF2968FF".into()),
            ..Collection::new(CollectionKind::Calendar, name)
        };
        self.create(c).await
    }

    /// Коллекция в хранилище как есть.
    pub(crate) async fn create(&self, c: Collection) -> Collection {
        let (kind, name) = (c.kind, c.name.clone());
        self.store
            .create_collection(self.scope, c)
            .await
            .expect("коллекция");
        self.collection(kind, &name).await.expect("создана")
    }

    /// Коллекция из хранилища.
    pub(crate) async fn collection(&self, kind: CollectionKind, name: &str) -> Option<Collection> {
        self.store
            .collection_by_name(self.scope, kind, name)
            .await
            .expect("хранилище")
    }

    /// `PUT` события с UID `uid` в календарь `coll`; ETag ответа.
    pub(crate) async fn put_event(
        &self,
        coll: &str,
        name: &str,
        uid: &str,
        summary: &str,
    ) -> String {
        let r = self
            .send(
                "PUT",
                &format!("{CALS}{coll}/{name}"),
                &[("content-type", "text/calendar")],
                &event(uid, summary),
            )
            .await;
        assert!(r.status.is_success(), "PUT {name}: {}", r.status);
        r.header("etag").expect("ETag").to_owned()
    }
}

/// Заголовок `Authorization: Basic`.
pub(crate) fn basic(login: &str, secret: &str) -> String {
    format!("Basic {}", STANDARD.encode(format!("{login}:{secret}")))
}

/// Ответ.
pub(crate) struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: String,
}

impl Resp {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// Снимок: статус и тело с переводом строки между тегами.
    pub(crate) fn show(&self) -> String {
        format!("{}\n{}", self.status, self.body.replace("><", ">\n<"))
    }
}

/// Событие iCalendar.
pub(crate) fn event(uid: &str, summary: &str) -> String {
    format!(
        "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//RU\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n\
         DTSTAMP:20260101T000000Z\r\nDTSTART:20260105T100000Z\r\nDTEND:20260105T110000Z\r\n\
         SUMMARY:{summary}\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n"
    )
}

/// Контакт vCard 3.0.
pub(crate) fn vcard(uid: &str, name: &str) -> String {
    format!("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:{uid}\r\nFN:{name}\r\nN:{name};;;;\r\nEND:VCARD\r\n")
}
