//! Тесты ядра по примерам RFC 8620.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tp_contracts::auth::Principal;
use tp_contracts::ext::JmapCapability;
use tp_contracts::{Error, Result};

use super::request::pointer;
use super::{Deps, Jmap, JmapConfig};
use crate::testenv::Env;

/// RFC 8620 §4.1: Core/echo возвращает аргументы.
#[tokio::test]
async fn core_echo_rfc_example() {
    let env = Env::new().await;
    let resp = env
        .request(json!({
            "using": ["urn:ietf:params:jmap:core"],
            "methodCalls": [["Core/echo", {"hello": true, "high": 5}, "b3ff"]]
        }))
        .await;
    assert_eq!(
        resp["methodResponses"],
        json!([["Core/echo", {"hello": true, "high": 5}, "b3ff"]])
    );
    assert!(resp["sessionState"].is_string());
    assert!(resp.get("createdIds").is_none());
}

/// RFC 8620 §3.7: ссылка на результат с `*` в пути.
#[tokio::test]
async fn result_references() {
    let env = Env::new().await;
    let r = env
        .calls(json!([
            ["Core/echo", {"list": [{"id": "a", "threadId": "t1"}, {"id": "b", "threadId": "t2"}]}, "t0"],
            ["Core/echo", {"#ids": {"resultOf": "t0", "name": "Core/echo", "path": "/list/*/threadId"}}, "t1"],
            ["Core/echo", {"#ids": {"resultOf": "t0", "name": "Mailbox/get", "path": "/list"}}, "t2"],
            ["Core/echo", {"#ids": {"resultOf": "zz", "name": "Core/echo", "path": "/list"}}, "t3"],
            ["Core/echo", {"ids": [], "#ids": {"resultOf": "t0", "name": "Core/echo", "path": "/list"}}, "t4"],
            ["Core/echo", {"#ids": {"resultOf": "t0", "name": "Core/echo", "path": "/nope"}}, "t5"]
        ]))
        .await;
    assert_eq!(r[1], json!(["Core/echo", {"ids": ["t1", "t2"]}, "t1"]));
    for (i, kind) in [
        (2, "invalidResultReference"),
        (3, "invalidResultReference"),
        (4, "invalidArguments"),
        (5, "invalidResultReference"),
    ] {
        assert_eq!(r[i][0], "error");
        assert_eq!(r[i][1]["type"], kind, "{}", r[i]);
    }
}

#[test]
fn json_pointer() {
    let v = json!({"a/b": {"m~n": [1, 2]}, "l": [{"x": [1, 2]}, {"x": [3]}]});
    assert_eq!(pointer(&v, "/a~1b/m~0n/1"), Some(json!(2)));
    assert_eq!(pointer(&v, "/l/*/x"), Some(json!([1, 2, 3])));
    assert_eq!(pointer(&v, ""), Some(v.clone()));
    assert_eq!(pointer(&v, "/l/7"), None);
    assert_eq!(pointer(&v, "l"), None);
}

/// RFC 8620 §3.6.1: ошибки запроса — problem details.
#[tokio::test]
async fn request_level_errors() {
    let env = Env::new().await;
    let p = &env.principal;
    let kind = |r: std::result::Result<_, super::Problem>| r.err().map(|p| p.to_json());
    let e = kind(env.jmap.api(p, b"{not json").await).expect("error");
    assert_eq!(e["type"], "urn:ietf:params:jmap:error:notJSON");
    assert_eq!(e["status"], 400);
    let e = kind(env.jmap.api(p, br#"{"using": []}"#).await).expect("error");
    assert_eq!(e["type"], "urn:ietf:params:jmap:error:notRequest");
    let e = kind(
        env.jmap
            .api(p, br#"{"using": ["urn:x:unknown"], "methodCalls": []}"#)
            .await,
    )
    .expect("error");
    assert_eq!(e["type"], "urn:ietf:params:jmap:error:unknownCapability");
    let calls: Vec<Value> = (0..17)
        .map(|i| json!(["Core/echo", {}, i.to_string()]))
        .collect();
    let body = json!({"using": ["urn:ietf:params:jmap:core"], "methodCalls": calls}).to_string();
    let e = kind(env.jmap.api(p, body.as_bytes()).await).expect("error");
    assert_eq!(e["type"], "urn:ietf:params:jmap:error:limit");
    assert_eq!(e["limit"], "maxCallsInRequest");
}

/// Метод без capability в `using` и неизвестный метод — `unknownMethod`; `createdIds` возвращается.
#[tokio::test]
async fn unknown_method_and_created_ids() {
    let env = Env::new().await;
    let resp = env
        .request(json!({
            "using": ["urn:ietf:params:jmap:core"],
            "methodCalls": [["Foo/bar", {}, "a"], ["Mailbox/get", {"accountId": env.account()}, "b"]],
            "createdIds": {"k": "v"}
        }))
        .await;
    let r = &resp["methodResponses"];
    assert_eq!(r[0], json!(["error", {"type": "unknownMethod"}, "a"]));
    assert_eq!(r[1], json!(["error", {"type": "unknownMethod"}, "b"]));
    assert_eq!(resp["createdIds"], json!({"k": "v"}));
}

#[tokio::test]
async fn session_object() {
    let env = Env::new().await;
    let s = env.jmap.session(&env.principal, "https://mail.example.org");
    let core = &s["capabilities"]["urn:ietf:params:jmap:core"];
    assert_eq!(core["maxCallsInRequest"], 16);
    assert_eq!(core["maxObjectsInGet"], 500);
    let acc = env.account();
    assert_eq!(s["accounts"][&acc]["name"], "user@example.org");
    assert_eq!(s["accounts"][&acc]["isPersonal"], true);
    assert!(s["accounts"][&acc]["accountCapabilities"]["urn:ietf:params:jmap:mail"].is_object());
    assert_eq!(s["primaryAccounts"]["urn:ietf:params:jmap:mail"], acc);
    assert_eq!(s["apiUrl"], "https://mail.example.org/jmap/api");
    assert!(
        s["downloadUrl"]
            .as_str()
            .is_some_and(|u| u.contains("{accountId}") && u.contains("{blobId}"))
    );
    assert_eq!(s["username"], "user@example.org");
    assert_eq!(
        s["state"],
        env.jmap.session(&env.principal, "https://mail.example.org")["state"]
    );
}

struct Echo(&'static str);

#[async_trait]
impl JmapCapability for Echo {
    fn uri(&self) -> &str {
        self.0
    }

    fn session_object(&self) -> Value {
        json!({"maxFoo": 3})
    }

    fn methods(&self) -> &[&str] {
        &["Foo/echo"]
    }

    async fn call(&self, principal: &Principal, method: &str, args: Value) -> Result<Value> {
        if args.get("fail").is_some() {
            return Err(Error::Forbidden);
        }
        Ok(json!({"method": method, "login": principal.login, "args": args}))
    }
}

/// Capability модуля: в сессии, методы вызываются, повтор URI или метода — `Conflict`.
#[tokio::test]
async fn module_capability() {
    let env = Env::with(JmapConfig::default(), vec![Arc::new(Echo("urn:x:foo"))]).await;
    let s = env.jmap.session(&env.principal, "");
    assert_eq!(s["capabilities"]["urn:x:foo"], json!({"maxFoo": 3}));
    assert_eq!(s["primaryAccounts"]["urn:x:foo"], env.account());
    let resp = env
        .request(json!({
            "using": ["urn:ietf:params:jmap:core", "urn:x:foo"],
            "methodCalls": [["Foo/echo", {"a": 1}, "0"], ["Foo/echo", {"fail": 1}, "1"]]
        }))
        .await;
    let r = &resp["methodResponses"];
    assert_eq!(
        r[0],
        json!(["Foo/echo", {"method": "Foo/echo", "login": "user@example.org", "args": {"a": 1}}, "0"])
    );
    assert_eq!(r[1], json!(["error", {"type": "forbidden"}, "1"]));
    let resp = env
        .request(
            json!({"using": ["urn:ietf:params:jmap:core"], "methodCalls": [["Foo/echo", {}, "0"]]}),
        )
        .await;
    assert_eq!(resp["methodResponses"][0][1]["type"], "unknownMethod");

    let deps = |caps: Vec<Arc<dyn JmapCapability>>| Deps {
        capabilities: caps,
        ..env.jmap.d.clone()
    };
    let dup = Jmap::new(
        deps(vec![
            Arc::new(Echo("urn:x:foo")),
            Arc::new(Echo("urn:x:foo")),
        ]),
        JmapConfig::default(),
    );
    assert_eq!(dup.err(), Some(Error::Conflict));
    let core = Jmap::new(
        deps(vec![Arc::new(Echo("urn:ietf:params:jmap:core"))]),
        JmapConfig::default(),
    );
    assert_eq!(core.err(), Some(Error::Conflict));
    let same_method = Jmap::new(
        deps(vec![Arc::new(Echo("urn:x:a")), Arc::new(Echo("urn:x:b"))]),
        JmapConfig::default(),
    );
    assert_eq!(same_method.err(), Some(Error::Conflict));
}

/// Запрос HTTP/1.1 к серверу; ответ: код, заголовки и тело.
async fn http(addr: std::net::SocketAddr, req: String) -> (u16, String, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
    s.write_all(req.as_bytes()).await.expect("write");
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.expect("read");
    let text = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = text.split_once("\r\n\r\n").unwrap_or((&text, ""));
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    (status, head.to_owned(), body.to_owned())
}

/// HTTP: сессия по `/.well-known/jmap`, Basic и Bearer, 401 без аутентификации, API, лимит тела.
#[tokio::test]
async fn http_endpoints() {
    let cfg = JmapConfig {
        public_url: Some("https://mail.example.org/".into()),
        max_size_request: 200,
        ..JmapConfig::default()
    };
    let env = Env::with(cfg, Vec::new()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    let router = env.jmap.clone().router();
    tokio::spawn(async move { axum::serve(listener, router).await });

    let get = |path: &str, auth: &str| {
        format!("GET {path} HTTP/1.1\r\nHost: x\r\n{auth}Connection: close\r\n\r\n")
    };
    let (code, head, _) = http(addr, get("/.well-known/jmap", "")).await;
    assert_eq!(code, 401);
    assert!(head.to_lowercase().contains("www-authenticate: basic"));
    // base64("user@example.org:app-secret")
    let basic = "Authorization: Basic dXNlckBleGFtcGxlLm9yZzphcHAtc2VjcmV0\r\n";
    let (code, _, body) = http(addr, get("/.well-known/jmap", basic)).await;
    assert_eq!(code, 200);
    let s: Value = serde_json::from_str(&body).expect("session");
    assert_eq!(s["apiUrl"], "https://mail.example.org/jmap/api");
    let (code, _, _) = http(
        addr,
        get("/jmap/session", "Authorization: Bearer token-1\r\n"),
    )
    .await;
    assert_eq!(code, 200);
    let (code, _, _) = http(addr, get("/jmap/session", "Authorization: Bearer bad\r\n")).await;
    assert_eq!(code, 401);

    let post = |body: &str| {
        format!(
            "POST /jmap/api HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer token-1\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
    };
    let req =
        r#"{"using":["urn:ietf:params:jmap:core"],"methodCalls":[["Core/echo",{"x":1},"a"]]}"#;
    let (code, _, body) = http(addr, post(req)).await;
    assert_eq!(code, 200);
    let r: Value = serde_json::from_str(&body).expect("response");
    assert_eq!(r["methodResponses"][0], json!(["Core/echo", {"x": 1}, "a"]));
    let (code, head, body) = http(addr, post("[1,")).await;
    assert_eq!(code, 400);
    assert!(head.contains("application/problem+json"));
    assert!(body.contains("notJSON"));
    let big = format!(
        r#"{{"using":[],"methodCalls":[],"x":"{}"}}"#,
        "a".repeat(300)
    );
    let (code, _, body) = http(addr, post(&big)).await;
    assert_eq!(code, 400);
    assert!(body.contains("maxSizeRequest"), "{body}");
}

/// Недоступный каталог при аутентификации — 503, а не 401.
#[tokio::test]
async fn auth_unavailable() {
    let env = Env::new().await;
    env.auth.set_failure(Some(Error::UNAVAILABLE));
    let mut h = axum::http::HeaderMap::new();
    h.insert(
        axum::http::header::AUTHORIZATION,
        "Bearer token-1".parse().expect("header"),
    );
    assert_eq!(env.jmap.authenticate(&h).await, Err(Error::UNAVAILABLE));
    h.insert(
        axum::http::header::AUTHORIZATION,
        "Digest x".parse().expect("header"),
    );
    assert_eq!(env.jmap.authenticate(&h).await, Err(Error::Forbidden));
}
