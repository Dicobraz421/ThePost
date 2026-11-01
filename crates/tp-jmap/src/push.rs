//! Push (RFC 8620 §7, RFC 8887): EventSource (`eventSourceUrl`) и WebSocket (`/jmap/ws`, подпротокол `jmap`).
//!
//! Изменения приходят из [`EventBus`](tp_contracts::bus::EventBus) подпиской на учётную запись: `MailboxChanged` —
//! типы `Email`, `Thread`, `Mailbox`; `MailboxListChanged` — `Mailbox`. Состояние в `StateChange` — текущее
//! состояние учётной записи; повтор того же состояния не отправляется. Доставка шины — at-most-once: после
//! переподключения клиент сверяется по `/changes`. При остановке узла ([`Jmap::shutdown`]) EventSource завершается,
//! WebSocket получает Close 1001.
//!
//! WebSocket: `Request` → `Response` (или `RequestError`) с `requestId`; `WebSocketPushEnable` (`dataTypes`,
//! `pushState`) / `WebSocketPushDisable` включают и выключают `StateChange` с `pushState`.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event as SseEvent, Sse};
use axum::response::{IntoResponse, Response};
use futures::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::{Instant, Interval, interval_at};
use tp_contracts::auth::Principal;
use tp_contracts::bus::{EventKind, EventStream};
use tp_contracts::meta::AccountScope;

use crate::core::http::auth_error;
use crate::core::request::state_string;
use crate::core::{Jmap, Problem};

/// Типы данных, которые меняет уведомление шины.
fn changed_types(kind: &EventKind) -> &'static [&'static str] {
    match kind {
        EventKind::MailboxChanged { .. } => &["Email", "Thread", "Mailbox"],
        EventKind::MailboxListChanged => &["Mailbox"],
        _ => &[],
    }
}

/// Подписка соединения push: фильтр типов и последнее отправленное состояние.
struct Watch {
    jmap: Arc<Jmap>,
    scope: AccountScope,
    /// `None` — все типы.
    types: Option<Vec<String>>,
    last: Option<String>,
}

impl Watch {
    fn wanted<'a>(&self, types: &[&'a str]) -> Vec<&'a str> {
        types
            .iter()
            .filter(|t| {
                self.types
                    .as_ref()
                    .is_none_or(|w| w.iter().any(|x| x == *t))
            })
            .copied()
            .collect()
    }

    /// `StateChange` для типов `types`; нечего отправлять (фильтр, то же состояние, сбой) — `None`.
    async fn change(&mut self, types: &[&str], push_state: bool) -> Option<Value> {
        let types = self.wanted(types);
        if types.is_empty() {
            return None;
        }
        let state = match self.jmap.state(self.scope).await {
            Ok(s) => state_string(s),
            Err(e) => {
                tracing::warn!(error = %e, "jmap: состояние для push не получено");
                return None;
            }
        };
        if self.last.as_deref() == Some(state.as_str()) {
            return None;
        }
        self.last = Some(state.clone());
        let mut by_type = serde_json::Map::new();
        for t in types {
            by_type.insert(t.to_owned(), json!(state));
        }
        let mut changed = serde_json::Map::new();
        changed.insert(self.scope.account.to_string(), by_type.into());
        let mut v = json!({ "@type": "StateChange", "changed": changed });
        if push_state {
            v["pushState"] = json!(state);
        }
        Some(v)
    }
}

/// Остановка узла: признак уже выставлен или выставлен сейчас (закрытый канал — тоже остановка).
async fn stopped(stop: &mut watch::Receiver<bool>) {
    let _ = stop.wait_for(|s| *s).await;
}

/// Следующий `ping`; без периода — никогда.
async fn tick(t: Option<&mut Interval>) {
    match t {
        Some(t) => {
            t.tick().await;
        }
        None => std::future::pending().await,
    }
}

/// Параметры `eventSourceUrl`.
#[derive(Debug, Deserialize)]
pub(crate) struct EventSourceQuery {
    types: Option<String>,
    closeafter: Option<String>,
    ping: Option<u64>,
}

struct Source {
    watch: Watch,
    events: EventStream,
    stop: watch::Receiver<bool>,
    ping: Option<(Interval, u64)>,
    close_after_state: bool,
    done: bool,
}

fn source_stream(src: Source) -> impl Stream<Item = Result<SseEvent, Infallible>> {
    futures::stream::unfold(src, |mut s| async move {
        if s.done || *s.stop.borrow() {
            return None;
        }
        loop {
            let next = tokio::select! {
                () = stopped(&mut s.stop) => return None,
                ev = s.events.next() => match ev {
                    None => return None,
                    Some(ev) => Some(ev),
                },
                () = tick(s.ping.as_mut().map(|(t, _)| t)) => None,
            };
            match next {
                Some(ev) => {
                    if let Some(change) = s.watch.change(changed_types(&ev.kind), false).await {
                        s.done = s.close_after_state;
                        let e = SseEvent::default().event("state").data(change.to_string());
                        return Some((Ok(e), s));
                    }
                }
                None => {
                    let secs = s.ping.as_ref().map_or(0, |(_, n)| *n);
                    let e = SseEvent::default()
                        .event("ping")
                        .data(json!({ "interval": secs }).to_string());
                    return Some((Ok(e), s));
                }
            }
        }
    })
}

/// `GET eventSourceUrl` (RFC 8620 §7.3): события `state` (`StateChange`) и `ping`.
pub(crate) async fn eventsource(
    State(j): State<Arc<Jmap>>,
    Query(q): Query<EventSourceQuery>,
    headers: HeaderMap,
) -> Response {
    let p = match j.authenticate(&headers).await {
        Ok(p) => p,
        Err(e) => return auth_error(&e),
    };
    let scope = scope_of(&p);
    let events = match j.d.bus.subscribe(scope).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "jmap: подписка EventSource не создана");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let types = match q.types.as_deref() {
        None | Some("*") => None,
        Some(t) => Some(t.split(',').map(str::to_owned).collect()),
    };
    let ping = match q.ping {
        None | Some(0) => None,
        Some(n) => {
            let period = Duration::from_secs(n).max(j.cfg.push_min_ping);
            Some((
                interval_at(Instant::now() + period, period),
                period.as_secs(),
            ))
        }
    };
    let src = Source {
        watch: Watch {
            jmap: j.clone(),
            scope,
            types,
            last: None,
        },
        events,
        stop: j.stopping(),
        ping,
        close_after_state: q.closeafter.as_deref() == Some("state"),
        done: false,
    };
    Sse::new(source_stream(src)).into_response()
}

fn scope_of(p: &Principal) -> AccountScope {
    AccountScope {
        tenant: p.tenant,
        account: p.account,
    }
}

/// `GET /jmap/ws` (RFC 8887): переход на WebSocket с подпротоколом `jmap`.
pub(crate) async fn websocket(
    State(j): State<Arc<Jmap>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let p = match j.authenticate(&headers).await {
        Ok(p) => p,
        Err(e) => return auth_error(&e),
    };
    ws.protocols(["jmap"])
        .on_upgrade(move |socket| ws_session(j, p, socket))
}

fn request_error(p: &Problem, id: Option<&Value>) -> Value {
    let mut v = p.to_json();
    v["@type"] = json!("RequestError");
    if let Some(id) = id {
        v["requestId"] = id.clone();
    }
    v
}

/// Обрабатывает сообщение клиента; ответы — в `out`.
async fn ws_message(
    j: &Jmap,
    p: &Principal,
    w: &mut Watch,
    push: &mut bool,
    text: &str,
    out: &mut Vec<Value>,
) {
    let mut v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            out.push(request_error(&Problem::not_json(e.to_string()), None));
            return;
        }
    };
    let id = v.get("id").cloned();
    match v.get("@type").and_then(Value::as_str) {
        Some("Request") => {
            if let Some(m) = v.as_object_mut() {
                m.remove("@type");
                m.remove("id");
            }
            let body = v.to_string();
            match j.api(p, body.as_bytes()).await {
                Ok(r) => match serde_json::to_value(r) {
                    Ok(mut r) => {
                        r["@type"] = json!("Response");
                        if let Some(id) = &id {
                            r["requestId"] = id.clone();
                        }
                        out.push(r);
                    }
                    Err(e) => tracing::error!(error = %e, "jmap: ответ WebSocket не сериализован"),
                },
                Err(pr) => out.push(request_error(&pr, id.as_ref())),
            }
        }
        Some("WebSocketPushEnable") => {
            w.types = match v.get("dataTypes") {
                Some(Value::Array(a)) => Some(
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect(),
                ),
                _ => None,
            };
            *push = true;
            match v.get("pushState").and_then(Value::as_str) {
                // Клиент отстал: сразу сообщить текущее состояние всех типов.
                Some(ps) => {
                    w.last = Some(ps.to_owned());
                    if let Some(c) = w.change(&["Email", "Thread", "Mailbox"], true).await {
                        out.push(c);
                    }
                }
                None => {
                    w.last = j.state(w.scope).await.ok().map(state_string);
                }
            }
        }
        Some("WebSocketPushDisable") => *push = false,
        _ => out.push(request_error(
            &Problem::not_request("unknown @type"),
            id.as_ref(),
        )),
    }
}

async fn ws_session(j: Arc<Jmap>, p: Principal, mut socket: WebSocket) {
    let scope = scope_of(&p);
    let mut events = match j.d.bus.subscribe(scope).await {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "jmap: подписка WebSocket не создана");
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
    };
    let mut stop = j.stopping();
    let mut w = Watch {
        jmap: j.clone(),
        scope,
        types: None,
        last: None,
    };
    let mut push = false;
    loop {
        let mut out = Vec::new();
        tokio::select! {
            () = stopped(&mut stop) => {
                let frame = CloseFrame { code: 1001, reason: "server shutdown".into() };
                let _ = socket.send(Message::Close(Some(frame))).await;
                return;
            }
            msg = socket.recv() => match msg {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                Some(Ok(Message::Text(t))) => ws_message(&j, &p, &mut w, &mut push, t.as_str(), &mut out).await,
                Some(Ok(Message::Binary(_))) => {
                    out.push(request_error(&Problem::not_json("binary message"), None));
                }
                Some(Ok(_)) => {}
            },
            ev = events.next() => match ev {
                None => return,
                Some(ev) if push => {
                    if let Some(c) = w.change(changed_types(&ev.kind), true).await {
                        out.push(c);
                    }
                }
                Some(_) => {}
            },
        }
        for v in out {
            if socket
                .send(Message::Text(v.to_string().into()))
                .await
                .is_err()
            {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::{SinkExt, StreamExt};
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpStream;
    use tokio_tungstenite::tungstenite::Message as WsMessage;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
    use tokio_tungstenite::{WebSocketStream, client_async};
    use tp_contracts::meta::{Flags, SpecialUse};

    use crate::testenv::{Env, mail, serve};

    const WAIT: Duration = Duration::from_secs(5);

    async fn connect(addr: std::net::SocketAddr) -> WebSocketStream<TcpStream> {
        let mut req = format!("ws://{addr}/jmap/ws")
            .into_client_request()
            .expect("request");
        let h = req.headers_mut();
        h.insert("Authorization", "Bearer token-1".parse().expect("header"));
        h.insert("Sec-WebSocket-Protocol", "jmap".parse().expect("header"));
        let tcp = TcpStream::connect(addr).await.expect("connect");
        let (ws, resp) = client_async(req, tcp).await.expect("handshake");
        assert_eq!(
            resp.headers()
                .get("sec-websocket-protocol")
                .and_then(|v| v.to_str().ok()),
            Some("jmap")
        );
        ws
    }

    async fn next_json(ws: &mut WebSocketStream<TcpStream>) -> Value {
        loop {
            let msg = tokio::time::timeout(WAIT, ws.next())
                .await
                .expect("timeout")
                .expect("closed")
                .expect("frame");
            if let WsMessage::Text(t) = msg {
                return serde_json::from_str(t.as_str()).expect("json");
            }
        }
    }

    async fn send(ws: &mut WebSocketStream<TcpStream>, v: Value) {
        ws.send(WsMessage::Text(v.to_string().into()))
            .await
            .expect("send");
    }

    /// WebSocket: запрос API с requestId, изменение приходит StateChange после WebSocketPushEnable,
    /// остановка узла — Close 1001.
    #[tokio::test]
    async fn websocket_push() {
        let env = Env::new().await;
        let addr = serve(env.jmap.clone()).await;
        let mut ws = connect(addr).await;
        let acc = env.account();

        send(
            &mut ws,
            json!({"@type": "WebSocketPushEnable", "dataTypes": ["Email"]}),
        )
        .await;
        send(
            &mut ws,
            json!({"@type": "Request", "id": "r1", "using": ["urn:ietf:params:jmap:core"],
                   "methodCalls": [["Core/echo", {"a": 1}, "c"]]}),
        )
        .await;
        let r = next_json(&mut ws).await;
        assert_eq!(r["@type"], "Response");
        assert_eq!(r["requestId"], "r1");
        assert_eq!(r["methodResponses"][0][1], json!({"a": 1}));

        env.deliver(
            env.mailbox(SpecialUse::Inbox),
            &mail("Push", "a@x.test", "", "b"),
            Flags::new(),
        )
        .await;
        let c = next_json(&mut ws).await;
        assert_eq!(c["@type"], "StateChange", "{c}");
        let state = c["changed"][&acc]["Email"].as_str().expect("Email state");
        assert_eq!(c["pushState"], state);
        assert!(c["changed"][&acc].get("Mailbox").is_none(), "{c}");

        send(&mut ws, json!({"@type": "Request", "using": []})).await;
        let e = next_json(&mut ws).await;
        assert_eq!(e["@type"], "RequestError", "{e}");
        send(&mut ws, json!({"@type": "Nope", "id": "x"})).await;
        let e = next_json(&mut ws).await;
        assert_eq!(e["@type"], "RequestError");
        assert_eq!(e["requestId"], "x");

        env.jmap.shutdown();
        let close = loop {
            match tokio::time::timeout(WAIT, ws.next())
                .await
                .expect("timeout")
            {
                Some(Ok(WsMessage::Close(f))) => break f,
                Some(Ok(_)) => {}
                other => panic!("no close: {other:?}"),
            }
        };
        assert_eq!(close.map(|f| f.code), Some(CloseCode::Away));
    }

    /// EventSource: изменение — событие `state`, `closeafter=state` закрывает поток.
    #[tokio::test]
    async fn eventsource_state() {
        let env = Env::new().await;
        let addr = serve(env.jmap.clone()).await;
        let acc = env.account();
        let mut s = TcpStream::connect(addr).await.expect("connect");
        s.write_all(
            b"GET /jmap/eventsource/?types=*&closeafter=state&ping=0 HTTP/1.0\r\n\
              Host: x\r\nAuthorization: Bearer token-1\r\n\r\n",
        )
        .await
        .expect("write");
        let mut buf = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = tokio::time::timeout(WAIT, s.read(&mut chunk))
                .await
                .expect("timeout")
                .expect("read");
            assert!(n > 0, "closed before headers");
            buf.extend_from_slice(&chunk[..n]);
        }
        let head = String::from_utf8_lossy(&buf).to_lowercase();
        assert!(head.contains("200 ok"), "{head}");
        assert!(head.contains("text/event-stream"), "{head}");
        env.deliver(
            env.mailbox(SpecialUse::Inbox),
            &mail("Push", "a@x.test", "", "b"),
            Flags::new(),
        )
        .await;
        tokio::time::timeout(WAIT, s.read_to_end(&mut buf))
            .await
            .expect("stream not closed")
            .expect("read");
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("event: state"), "{text}");
        let data = text
            .lines()
            .find_map(|l| l.strip_prefix("data: "))
            .expect("data");
        let v: Value = serde_json::from_str(data).expect("json");
        assert_eq!(v["@type"], "StateChange");
        assert!(v["changed"][&acc]["Email"].is_string(), "{v}");
        assert!(v["changed"][&acc]["Mailbox"].is_string(), "{v}");
    }

    /// SearchSnippet/get: подсветка по текстовой части фильтра; чужое письмо — notFound.
    #[tokio::test]
    async fn search_snippets() {
        let env = Env::new().await;
        let id = env
            .deliver(
                env.mailbox(SpecialUse::Inbox),
                &mail(
                    "Квартальный отчёт",
                    "a@x.test",
                    "",
                    "Отчёт готов к проверке",
                ),
                Flags::new(),
            )
            .await
            .to_string();
        let r = env
            .call(
                "SearchSnippet/get",
                json!({"accountId": env.account(), "emailIds": [&id, "nope"],
                       "filter": {"operator": "AND", "conditions": [{"text": "отчёт"}, {"hasKeyword": "$seen"}]}}),
                "SearchSnippet/get",
            )
            .await;
        assert_eq!(r["list"][0]["emailId"], id);
        let subject = r["list"][0]["subject"].as_str().unwrap_or_default();
        assert!(subject.contains("<mark>"), "{r}");
        assert_eq!(r["notFound"], json!(["nope"]));
        let r = env
            .call(
                "SearchSnippet/get",
                json!({"accountId": env.account(), "emailIds": [&id], "filter": {"hasKeyword": "$seen"}}),
                "SearchSnippet/get",
            )
            .await;
        assert_eq!(r["list"][0]["subject"], Value::Null);
        assert_eq!(r["notFound"], Value::Null);
    }
}
