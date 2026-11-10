//! Узлы `NodeRpc` в одном процессе: настоящие HTTPS с mTLS на 127.0.0.1. Сертификаты — `src/testdata` (УЦ, узлы
//! `a`, `b`, `c` с SAN `localhost`/`127.0.0.1`; `rogue` — CN `a` от чужого УЦ), срок — 100 лет.

use std::sync::atomic::{AtomicUsize, Ordering};

use tp_testkit::membership::MemMembership;
use tp_testkit::suite::{RpcFixture, rpc as rpc_suite};

use super::*;

const CA: &[u8] = include_bytes!("../testdata/ca.pem");
const ROGUE: &[u8] = include_bytes!("../testdata/rogue.pem");
const ROGUE_KEY: &[u8] = include_bytes!("../testdata/rogue-key.pem");

/// С запасом: сроки, которые не должны истекать.
const LONG: Duration = Duration::from_secs(20);

fn node(name: &str) -> NodeId {
    NodeId::new(name).expect("node")
}

fn tls(name: &str) -> NodeTls {
    let (cert, key): (&[u8], &[u8]) = match name {
        "a" => (
            include_bytes!("../testdata/node-a.pem"),
            include_bytes!("../testdata/node-a-key.pem"),
        ),
        "b" => (
            include_bytes!("../testdata/node-b.pem"),
            include_bytes!("../testdata/node-b-key.pem"),
        ),
        "c" => (
            include_bytes!("../testdata/node-c.pem"),
            include_bytes!("../testdata/node-c-key.pem"),
        ),
        _ => panic!("no certificate for {name}"),
    };
    NodeTls::from_pem(CA, cert, key).expect("tls")
}

fn svc(role: Role, name: &str) -> RpcService {
    RpcService::new(role, name).expect("service")
}

fn config(permissions: &str, max_in_flight: usize) -> RpcConfig {
    let mut c: RpcConfig = serde_yaml_ng::from_str(permissions).expect("yaml");
    c.max_in_flight = max_in_flight;
    c.members_ttl = Duration::ZERO;
    c
}

struct Cluster {
    members: Arc<MemMembership>,
    servers: Vec<AbortOnDrop>,
}

impl Cluster {
    fn new() -> Self {
        Self {
            members: Arc::new(MemMembership::default()),
            servers: Vec::new(),
        }
    }

    async fn announce(&self, id: &str, roles: &[Role], addr: String) {
        let mut info = NodeInfo::new(node(id));
        info.roles = roles.iter().copied().collect();
        info.addr = addr;
        self.members
            .heartbeat(&info, Duration::from_secs(3600))
            .await
            .expect("heartbeat");
    }

    /// Запускает узел `id` (сертификат `cert`) с ролями `roles` и объявляет его в составе.
    async fn start(
        &mut self,
        id: &str,
        cert: NodeTls,
        roles: &[Role],
        config: &RpcConfig,
    ) -> NodeRpc {
        let rpc = NodeRpc::new(node(id), config, cert, self.members.clone()).expect("rpc");
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let server = rpc.clone();
        self.servers.push(AbortOnDrop(tokio::spawn(async move {
            server
                .listen(listener, std::future::pending())
                .await
                .expect("listen");
        })));
        self.announce(id, roles, format!("https://127.0.0.1:{port}"))
            .await;
        rpc
    }
}

/// Порт, на котором никто не слушает.
async fn closed_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    l.local_addr().expect("addr").port()
}

struct Echo(&'static str);

#[async_trait]
impl RpcHandler for Echo {
    async fn handle(&self, r: RpcRequest) -> Result<Bytes> {
        Ok(Bytes::from(format!(
            "{}|{}|{}|{}",
            self.0,
            r.caller,
            r.key,
            String::from_utf8_lossy(&r.body)
        )))
    }
}

struct Fails(Error);

#[async_trait]
impl RpcHandler for Fails {
    async fn handle(&self, _: RpcRequest) -> Result<Bytes> {
        Err(self.0.clone())
    }
}

struct Gate {
    entered: Arc<AtomicUsize>,
    open: Arc<tokio::sync::Semaphore>,
}

#[async_trait]
impl RpcHandler for Gate {
    async fn handle(&self, r: RpcRequest) -> Result<Bytes> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        let _p = self
            .open
            .acquire()
            .await
            .map_err(|_| Error::internal("closed"))?;
        Ok(Bytes::from(r.deadline.as_millis().to_string()))
    }
}

struct Sleep(Duration);

#[async_trait]
impl RpcHandler for Sleep {
    async fn handle(&self, _: RpcRequest) -> Result<Bytes> {
        tokio::time::sleep(self.0).await;
        Ok(Bytes::new())
    }
}

#[test]
fn identity_from_certificate() {
    for n in ["a", "b", "c"] {
        assert_eq!(tls(n).identity(), &node(n));
    }
    let rogue = NodeTls::from_pem(CA, ROGUE, ROGUE_KEY).expect("rogue");
    assert_eq!(rogue.identity(), &node("a"));
    assert!(NodeTls::from_pem(b"", ROGUE, ROGUE_KEY).is_err());
    assert!(NodeTls::from_pem(CA, b"", ROGUE_KEY).is_err());
    assert!(NodeTls::from_pem(CA, ROGUE, b"").is_err());
    // Сертификат называет другой узел.
    let e = NodeRpc::new(
        node("b"),
        &RpcConfig::default(),
        tls("a"),
        Arc::new(MemMembership::default()),
    )
    .expect_err("wrong identity");
    assert!(matches!(e, Error::Invalid(_)), "{e:?}");
}

#[tokio::test]
async fn load_requires_files() {
    let e = NodeTls::load(&InternalTls::default())
        .await
        .expect_err("no files");
    assert!(
        matches!(&e, Error::Invalid(m) if m.contains("ca_file")),
        "{e:?}"
    );
    let missing = InternalTls {
        ca_file: Some("/nonexistent/ca.pem".into()),
        cert_file: Some("/nonexistent/node.pem".into()),
        key_file: Some("/nonexistent/key.pem".into()),
        ..InternalTls::default()
    };
    assert!(matches!(
        NodeTls::load(&missing).await,
        Err(Error::Invalid(_))
    ));
}

#[test]
fn wire_format() {
    let framed = frame("t/a", &Bytes::from_static(b"body")).expect("frame");
    assert_eq!(
        unframe(framed),
        Some(("t/a".to_owned(), Bytes::from_static(b"body")))
    );
    assert_eq!(unframe(Bytes::from_static(b"\0\0\0\x09abc")), None);
    assert_eq!(unframe(Bytes::from_static(b"\0\0")), None);
    for e in [
        RpcError::Busy,
        RpcError::Timeout,
        RpcError::Failed(Error::NotFound),
        RpcError::Failed(Error::Conflict),
        RpcError::Failed(Error::invalid("bad")),
        RpcError::Failed(Error::Forbidden),
        RpcError::Failed(Error::QuotaExceeded),
        RpcError::Failed(Error::Unavailable {
            retry_after: Some(Duration::from_millis(1500)),
        }),
        RpcError::Failed(Error::UNAVAILABLE),
        RpcError::Failed(Error::internal("oops")),
    ] {
        let resp = encode(Err(e.clone()));
        let h = |n: &str| {
            resp.headers()
                .get(n)
                .map(|v| v.to_str().expect("ascii").to_owned())
        };
        let body = match &e {
            RpcError::Failed(Error::Invalid(m) | Error::Internal(m)) => Bytes::from(m.clone()),
            _ => Bytes::new(),
        };
        assert_eq!(
            decode(
                resp.status(),
                h(H_STATUS).as_deref(),
                h(H_RETRY_AFTER).as_deref(),
                body
            ),
            Err(e)
        );
    }
    assert!(matches!(
        decode(StatusCode::BAD_GATEWAY, None, None, Bytes::new()),
        Err(RpcError::Failed(Error::Internal(_)))
    ));
}

/// Общий набор `suite::rpc` на трёх узлах с настоящим транспортом.
#[tokio::test]
async fn passes_contract_suite() {
    let mut cluster = Cluster::new();
    let cfg = config("permissions:\n  search: [search/*]\n", 2);
    let mut nodes = Vec::new();
    for n in ["a", "b", "c"] {
        let rpc = cluster.start(n, tls(n), &[Role::Search], &cfg).await;
        nodes.push((node(n), Arc::new(rpc) as Arc<dyn Rpc>));
    }
    rpc_suite(&RpcFixture {
        nodes,
        role: Role::Search,
        absent_role: Role::Admin,
        max_in_flight: 2,
    })
    .await;
}

/// Вызов, отказ по матрице, `Busy`, `Timeout` — всегда через сеть: у вызывающих нет роли службы.
#[tokio::test]
async fn two_nodes_over_mtls() {
    let mut cluster = Cluster::new();
    let cfg = config("permissions:\n  jmap: [search/*]\n", 1);
    let a = cluster.start("a", tls("a"), &[Role::Jmap], &cfg).await;
    let b = cluster.start("b", tls("b"), &[Role::Search], &cfg).await;
    let c = cluster.start("c", tls("c"), &[Role::Imap], &cfg).await;
    let echo = svc(Role::Search, "echo");
    let (gate, slow, fail) = (
        svc(Role::Search, "gate"),
        svc(Role::Search, "slow"),
        svc(Role::Search, "fail"),
    );
    let entered = Arc::new(AtomicUsize::new(0));
    let open = Arc::new(tokio::sync::Semaphore::new(0));
    b.serve(echo.clone(), Arc::new(Echo("b"))).expect("echo");
    b.serve(
        gate.clone(),
        Arc::new(Gate {
            entered: entered.clone(),
            open: open.clone(),
        }),
    )
    .expect("gate");
    b.serve(slow.clone(), Arc::new(Sleep(LONG))).expect("slow");
    b.serve(fail.clone(), Arc::new(Fails(Error::QuotaExceeded)))
        .expect("fail");
    assert_eq!(
        b.serve(echo.clone(), Arc::new(Echo("x"))),
        Err(Error::Conflict)
    );

    // Вызов: вызывающий — по сертификату, ключ и тело доходят как есть.
    assert_eq!(
        a.call(&echo, "t/ключ", Bytes::from_static(b"ping"), LONG)
            .await,
        Ok(Bytes::from("b|a|t/ключ|ping"))
    );
    assert_eq!(
        a.call(&fail, "k", Bytes::new(), LONG).await,
        Err(RpcError::Failed(Error::QuotaExceeded))
    );
    assert_eq!(
        a.call(&svc(Role::Search, "missing"), "k", Bytes::new(), LONG)
            .await,
        Err(RpcError::Failed(Error::NotFound))
    );
    // Отказ по матрице: у imap нет разрешений.
    assert_eq!(
        c.call(&echo, "k", Bytes::new(), LONG).await,
        Err(RpcError::Failed(Error::Forbidden))
    );
    // Нет узлов роли.
    assert_eq!(
        a.call(&svc(Role::Admin, "status"), "k", Bytes::new(), LONG)
            .await,
        Err(RpcError::Unavailable)
    );

    // Busy: место обработки на b одно.
    let held = {
        let (a, gate) = (a.clone(), gate.clone());
        tokio::spawn(async move { a.call(&gate, "k", Bytes::new(), LONG).await })
    };
    let started = Instant::now();
    while entered.load(Ordering::SeqCst) < 1 {
        assert!(started.elapsed() < LONG, "обработчик не начал работу");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    assert_eq!(
        a.call(&echo, "k", Bytes::new(), LONG).await,
        Err(RpcError::Busy)
    );
    open.add_permits(1);
    // Обработчику передан остаток срока (не больше срока вызова).
    let left: u128 = String::from_utf8(held.await.expect("join").expect("gate").to_vec())
        .expect("utf8")
        .parse()
        .expect("ms");
    assert!(left > 0 && left <= LONG.as_millis(), "{left}");

    // Timeout: обработчик отвечает позже срока; место обработки освобождается.
    let started = Instant::now();
    assert_eq!(
        a.call(&slow, "k", Bytes::new(), Duration::from_millis(300))
            .await,
        Err(RpcError::Timeout)
    );
    assert!(started.elapsed() < LONG / 2, "{:?}", started.elapsed());
    let started = Instant::now();
    loop {
        match a.call(&echo, "k", Bytes::new(), LONG).await {
            Ok(_) => break,
            Err(RpcError::Busy) if started.elapsed() < LONG => {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            other => panic!("{other:?}"),
        }
    }
}

/// Недоступный узел маршрута — следующий; все недоступны — `Unavailable`; чужой УЦ и подмена узла — отказ.
#[tokio::test]
async fn unavailable_nodes_and_identity() {
    let mut cluster = Cluster::new();
    let cfg = config("permissions:\n  jmap: [search/*]\n", 4);
    let a = cluster.start("a", tls("a"), &[Role::Jmap], &cfg).await;
    let b = cluster.start("b", tls("b"), &[Role::Search], &cfg).await;
    let echo = svc(Role::Search, "echo");
    b.serve(echo.clone(), Arc::new(Echo("b"))).expect("echo");
    // c объявлен с ролью службы, но не слушает.
    cluster
        .announce(
            "c",
            &[Role::Search],
            format!("https://127.0.0.1:{}", closed_port().await),
        )
        .await;
    let live = cluster.members.live_info().await.expect("live");
    let key = (0..1000)
        .map(|i| format!("k/{i}"))
        .find(|k| route(&live, &echo, k).expect("route")[0].id == node("c"))
        .expect("key owned by c");
    assert_eq!(
        a.call(&echo, &key, Bytes::new(), LONG).await,
        Ok(Bytes::from(format!("b|a|{key}|")))
    );

    // Только недоступный узел.
    cluster.members.leave(&node("b")).await.expect("leave");
    assert_eq!(
        a.call(&echo, &key, Bytes::new(), LONG).await,
        Err(RpcError::Unavailable)
    );
    cluster.members.leave(&node("c")).await.expect("leave");

    // Подмена: в составе b указывает на адрес a — сертификат сервера называет a, узел пропускается.
    let b_addr = format!("https://127.0.0.1:{}", {
        let live = cluster.members.live_info().await.expect("live");
        let a_addr = &live.iter().find(|n| n.id == node("a")).expect("a").addr;
        a_addr
            .rsplit(':')
            .next()
            .expect("port")
            .parse::<u16>()
            .expect("port")
    });
    a.serve(echo.clone(), Arc::new(Echo("a")))
        .expect("echo on a");
    cluster.announce("b", &[Role::Search], b_addr).await;
    let c = cluster.start("c", tls("c"), &[Role::Jmap], &cfg).await;
    assert_eq!(
        c.call(&echo, "k", Bytes::new(), LONG).await,
        Err(RpcError::Unavailable)
    );

    // Клиент с сертификатом чужого УЦ (CN a): сервер не принимает соединение.
    let mut cluster = Cluster::new();
    let b = cluster.start("b", tls("b"), &[Role::Search], &cfg).await;
    b.serve(echo.clone(), Arc::new(Echo("b"))).expect("echo");
    cluster
        .announce("a", &[Role::Jmap], "https://127.0.0.1:1".to_owned())
        .await;
    let rogue = NodeRpc::new(
        node("a"),
        &cfg,
        NodeTls::from_pem(CA, ROGUE, ROGUE_KEY).expect("rogue"),
        cluster.members.clone(),
    )
    .expect("rogue rpc");
    assert_eq!(
        rogue.call(&echo, "k", Bytes::new(), LONG).await,
        Err(RpcError::Unavailable)
    );
    // Сервер с сертификатом чужого УЦ: клиент не принимает сервер.
    let mut cluster = Cluster::new();
    let fake = cluster
        .start(
            "a",
            NodeTls::from_pem(CA, ROGUE, ROGUE_KEY).expect("rogue"),
            &[Role::Search],
            &cfg,
        )
        .await;
    fake.serve(echo.clone(), Arc::new(Echo("a"))).expect("echo");
    let c = cluster.start("c", tls("c"), &[Role::Jmap], &cfg).await;
    assert_eq!(
        c.call(&echo, "k", Bytes::new(), LONG).await,
        Err(RpcError::Unavailable)
    );
}
