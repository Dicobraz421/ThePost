//! Узлы `NodeRpc` в одном процессе: настоящие HTTPS с mTLS на 127.0.0.1. Сертификаты — `src/testdata`, срок —
//! 100 лет, ключи P-256 (openssl; ключи УЦ не хранятся — новый сертификат требует нового набора):
//! - `ca` — УЦ кластера; `ca2` — второй УЦ (смена УЦ); `rogue` — узел `a` от чужого УЦ;
//! - `node-a`, `node-b` — SAN `thepost:node:<id>`, `localhost`, `127.0.0.1` и CN узла; `node-c` — то же, CN
//!   `legacy-c` (URI важнее CN); `node-b2` — узел `b` от `ca2`; `node-d` — без CN, только SAN;
//! - `host-dns` (SAN `localhost`) и `host-ip` (SAN `127.0.0.1`) — без CN и URI; `cn-only` — CN `e` без SAN.

use std::sync::atomic::{AtomicUsize, Ordering};

use tp_contracts::secrets::SecretRef;
use tp_testkit::MemSecretStore;
use tp_testkit::membership::MemMembership;
use tp_testkit::suite::{RpcFixture, rpc as rpc_suite};

use super::*;

const CA: &[u8] = include_bytes!("../testdata/ca.pem");
const ROGUE: &[u8] = include_bytes!("../testdata/rogue.pem");
const ROGUE_KEY: &[u8] = include_bytes!("../testdata/rogue-key.pem");
const CA2: &[u8] = include_bytes!("../testdata/ca2.pem");

/// С запасом: сроки, которые не должны истекать.
const LONG: Duration = Duration::from_secs(20);

fn node(name: &str) -> NodeId {
    NodeId::new(name).expect("node")
}

/// Сертификат и ключ: `a`, `b`, `c`, `d` — `node-<имя>`, иначе — файл `<имя>`.
fn pem(name: &str) -> (&'static [u8], &'static [u8]) {
    macro_rules! pair {
        ($file:literal) => {
            (
                include_bytes!(concat!("../testdata/", $file, ".pem")).as_slice(),
                include_bytes!(concat!("../testdata/", $file, "-key.pem")).as_slice(),
            )
        };
    }
    match name {
        "a" => pair!("node-a"),
        "b" => pair!("node-b"),
        "c" => pair!("node-c"),
        "d" => pair!("node-d"),
        "node-b2" => pair!("node-b2"),
        "host-dns" => pair!("host-dns"),
        "host-ip" => pair!("host-ip"),
        "cn-only" => pair!("cn-only"),
        _ => panic!("no certificate for {name}"),
    }
}

fn tls(name: &str) -> NodeTls {
    let (cert, key) = pem(name);
    NodeTls::from_pem(CA, cert, key).expect("tls")
}

fn named(id: &str) -> NodeIdentity {
    NodeIdentity::Node(node(id))
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

    /// Запускает узел `id` (сертификат `cert`) с ролями `roles` и объявляет его в составе (`127.0.0.1`).
    async fn start(
        &mut self,
        id: &str,
        cert: NodeTls,
        roles: &[Role],
        config: &RpcConfig,
    ) -> NodeRpc {
        self.start_at(id, cert, roles, config, "127.0.0.1").await
    }

    /// Как [`Cluster::start`], в адресе узла — хост `host` (слушает 127.0.0.1).
    async fn start_at(
        &mut self,
        id: &str,
        cert: NodeTls,
        roles: &[Role],
        config: &RpcConfig,
        host: &str,
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
        self.announce(id, roles, format!("https://{host}:{port}"))
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
    // URI `thepost:node:<id>` в SAN; у `c` CN другой, у `d` CN нет.
    for n in ["a", "b", "c", "d"] {
        assert_eq!(tls(n).identity(), &named(n));
    }
    // Без URI — имена DNS и адреса IP из SAN; без SAN — CN (выпуск до CLU-14).
    assert_eq!(
        tls("host-dns").identity(),
        &NodeIdentity::Hosts(vec!["localhost".into()])
    );
    assert_eq!(
        tls("host-ip").identity(),
        &NodeIdentity::Hosts(vec!["127.0.0.1".into()])
    );
    assert_eq!(tls("cn-only").identity(), &named("e"));
    let rogue = NodeTls::from_pem(CA, ROGUE, ROGUE_KEY).expect("rogue");
    assert_eq!(rogue.identity(), &named("a"));

    // Узел по хостам: хост адреса из состава, без учёта регистра; два узла на хосте — неоднозначно.
    let info = |id: &str, addr: &str| {
        let mut n = NodeInfo::new(node(id));
        n.addr = addr.to_owned();
        n
    };
    let hosts = tls("host-dns").identity().clone();
    let live = vec![
        info("x", "https://LocalHost:7000"),
        info("y", "https://127.0.0.1:7000"),
    ];
    assert!(hosts.names(&live[0]) && !hosts.names(&live[1]));
    assert_eq!(hosts.resolve(&live), Some(node("x")));
    let mut shared = live.clone();
    shared.push(info("z", "https://localhost:7001/"));
    assert_eq!(hosts.resolve(&shared), None);
    assert_eq!(hosts.resolve(&live[1..]), None);
    assert_eq!(named("q").resolve(&[]), Some(node("q")));
    // Сертификат по хостам годится любому узлу: адрес проверяют другие узлы.
    NodeRpc::new(
        node("x"),
        &RpcConfig::default(),
        tls("host-dns"),
        Arc::new(MemMembership::default()),
    )
    .expect("host identity");
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
    let store = MemSecretStore::default();
    let e = NodeTls::load(&InternalTls::default(), &store)
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
        NodeTls::load(&missing, &store).await,
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

    // Клиент с сертификатом чужого УЦ (узел a): сервер не принимает соединение.
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

/// Сертификаты без CN: узел по URI (`d`) и по хосту адреса (`l` — `localhost`, `i` — `127.0.0.1`).
#[tokio::test]
async fn identity_by_san_without_cn() {
    let mut cluster = Cluster::new();
    let cfg = config("permissions:\n  jmap: [search/*]\n", 4);
    let i = cluster
        .start("i", tls("host-ip"), &[Role::Search], &cfg)
        .await;
    let l = cluster
        .start_at("l", tls("host-dns"), &[Role::Jmap], &cfg, "localhost")
        .await;
    let d = cluster.start("d", tls("d"), &[Role::Jmap], &cfg).await;
    let echo = svc(Role::Search, "echo");
    i.serve(echo.clone(), Arc::new(Echo("i"))).expect("echo");
    // Сервер `i` назван адресом 127.0.0.1, вызывающий `l` — единственный узел на `localhost`.
    assert_eq!(
        l.call(&echo, "k", Bytes::new(), LONG).await,
        Ok(Bytes::from("i|l|k|"))
    );
    assert_eq!(
        d.call(&echo, "k", Bytes::new(), LONG).await,
        Ok(Bytes::from("i|d|k|"))
    );
    // Второй узел на `localhost`: вызывающий по хосту неоднозначен — отказ.
    cluster
        .announce("m", &[Role::Jmap], "https://localhost:1".to_owned())
        .await;
    assert_eq!(
        l.call(&echo, "k", Bytes::new(), LONG).await,
        Err(RpcError::Failed(Error::Forbidden))
    );
}

/// Замена сертификатов без перезапуска: сервер — по событию из файлов, клиент — `reload_tls`; ошибка — прежние.
#[tokio::test]
async fn reloads_certificates_without_restart() {
    let mut cluster = Cluster::new();
    let cfg = config("permissions:\n  jmap: [search/*]\n", 4);
    let a = cluster.start("a", tls("a"), &[Role::Jmap], &cfg).await;
    let b = cluster.start("b", tls("b"), &[Role::Search], &cfg).await;
    let echo = svc(Role::Search, "echo");
    b.serve(echo.clone(), Arc::new(Echo("b"))).expect("echo");
    assert_eq!(
        a.call(&echo, "k", Bytes::new(), LONG).await,
        Ok(Bytes::from("b|a|k|"))
    );

    // Смена УЦ: b доверяет обоим УЦ, его сертификат — от второго.
    let bundle = [CA, CA2].concat();
    let dir = std::env::temp_dir().join(format!("tp-cluster-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    let (b2, b2_key) = pem("node-b2");
    for (file, data) in [
        ("ca.pem", &bundle[..]),
        ("b.pem", b2),
        ("b-key.pem", b2_key),
    ] {
        std::fs::write(dir.join(file), data).expect("write");
    }
    let files = InternalTls {
        ca_file: Some(dir.join("ca.pem")),
        cert_file: Some(dir.join("b.pem")),
        key_file: Some(dir.join("b-key.pem")),
        ..InternalTls::default()
    };
    let no_secrets: Arc<dyn SecretStore> = Arc::new(MemSecretStore::default());
    b.follow_tls(&files, no_secrets.clone(), futures::stream::iter([()]))
        .await;

    // Новый узел знает только прежний УЦ: сервер b не принят.
    let c = cluster.start("c", tls("c"), &[Role::Jmap], &cfg).await;
    assert_eq!(
        c.call(&echo, "k", Bytes::new(), LONG).await,
        Err(RpcError::Unavailable)
    );
    let (c_cert, c_key) = pem("c");
    c.reload_tls(NodeTls::from_pem(&bundle, c_cert, c_key).expect("c tls"))
        .expect("reload c");
    assert_eq!(
        c.call(&echo, "k", Bytes::new(), LONG).await,
        Ok(Bytes::from("b|c|k|"))
    );

    // Сертификат другого узла и нечитаемые файлы — отказ, прежние сертификаты в силе.
    assert!(matches!(b.reload_tls(tls("a")), Err(Error::Invalid(_))));
    let broken = InternalTls {
        key_file: Some(dir.join("missing-key.pem")),
        ..files
    };
    b.follow_tls(&broken, no_secrets, futures::stream::iter([()]))
        .await;
    let d = cluster.start("d", tls("d"), &[Role::Jmap], &cfg).await;
    assert_eq!(
        d.call(&echo, "k", Bytes::new(), LONG).await,
        Err(RpcError::Unavailable)
    );
    assert_eq!(
        c.call(&echo, "k", Bytes::new(), LONG).await,
        Ok(Bytes::from("b|c|k|"))
    );
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

/// Каталог с `ca.pem` и `<имя>.pem`; ключ узла в файл не пишется.
fn cert_dir(tag: &str, name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("tp-cluster-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    std::fs::write(dir.join("ca.pem"), CA).expect("ca");
    std::fs::write(dir.join(format!("{name}.pem")), pem(name).0).expect("cert");
    dir
}

fn key_ref(name: &str) -> SecretRef {
    format!("thepost/tls/node-{name}#key").parse().expect("ref")
}

/// Секция `rpc.tls` узла `name` с ключом по ссылке на секрет.
fn secret_tls(dir: &std::path::Path, name: &str) -> InternalTls {
    InternalTls {
        ca_file: Some(dir.join("ca.pem")),
        cert_file: Some(dir.join(format!("{name}.pem"))),
        key: Some(key_ref(name)),
        ..InternalTls::default()
    }
}

fn key_text(name: &str) -> &'static str {
    std::str::from_utf8(pem(name).1).expect("pem")
}

/// CLU-16: два узла с ключом из хранилища секретов устанавливают mTLS-соединение.
#[tokio::test]
async fn key_from_secret_store() {
    let dir = cert_dir("secret", "a");
    std::fs::write(dir.join("b.pem"), pem("b").0).expect("cert");
    let store = MemSecretStore::default();
    store.set(&key_ref("a"), key_text("a"));
    store.set(&key_ref("b"), key_text("b"));
    let a_tls = NodeTls::load(&secret_tls(&dir, "a"), &store)
        .await
        .expect("a");
    let b_tls = NodeTls::load(&secret_tls(&dir, "b"), &store)
        .await
        .expect("b");
    assert_eq!(a_tls.identity(), &named("a"));

    let mut cluster = Cluster::new();
    let cfg = config("permissions:\n  jmap: [search/*]\n", 4);
    let a = cluster.start("a", a_tls, &[Role::Jmap], &cfg).await;
    let b = cluster.start("b", b_tls, &[Role::Search], &cfg).await;
    let echo = svc(Role::Search, "echo");
    b.serve(echo.clone(), Arc::new(Echo("b"))).expect("echo");
    assert_eq!(
        a.call(&echo, "k", Bytes::new(), LONG).await,
        Ok(Bytes::from("b|a|k|"))
    );
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

/// CLU-16: `key` и `key_file` вместе, нет секрета, не PEM, чужой ключ — `Invalid` без
/// значения секрета в тексте; недоступное хранилище — повторяемая ошибка.
#[tokio::test]
async fn key_secret_errors() {
    let dir = cert_dir("secret-err", "a");
    let cfg = secret_tls(&dir, "a");
    let store = MemSecretStore::default();
    store.set(&key_ref("a"), key_text("a"));

    let both = InternalTls {
        key_file: Some(dir.join("a-key.pem")),
        ..cfg.clone()
    };
    let e = NodeTls::load(&both, &store).await.expect_err("both");
    assert!(
        matches!(&e, Error::Invalid(m) if m.contains("mutually exclusive")),
        "{e:?}"
    );
    let none = InternalTls {
        key: None,
        ..cfg.clone()
    };
    let e = NodeTls::load(&none, &store).await.expect_err("no key");
    assert!(
        matches!(&e, Error::Invalid(m) if m.contains("key_file") && m.contains("rpc.tls.key ")),
        "{e:?}"
    );

    let empty = MemSecretStore::default();
    let e = NodeTls::load(&cfg, &empty)
        .await
        .expect_err("missing secret");
    assert!(
        matches!(&e, Error::Invalid(m) if m.contains("thepost/tls/node-a")),
        "{e:?}"
    );

    store.set(&key_ref("a"), "garbage-value");
    let e = NodeTls::load(&cfg, &store).await.expect_err("garbage");
    assert!(
        matches!(&e, Error::Invalid(m) if !m.contains("garbage-value")),
        "{e:?}"
    );
    // Ключ другого узла не подходит к сертификату.
    store.set(&key_ref("a"), key_text("b"));
    let e = NodeTls::load(&cfg, &store).await.expect_err("foreign key");
    assert!(matches!(e, Error::Invalid(_)), "{e:?}");

    store.set(&key_ref("a"), key_text("a"));
    store.seal();
    let e = NodeTls::load(&cfg, &store).await.expect_err("sealed");
    assert!(e.is_retryable(), "{e:?}");
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

/// CLU-16: ключ из хранилища перечитывается по тем же событиям, что и файлы; не подходящий к сертификату ключ
/// и недоступное хранилище оставляют прежнюю пару.
#[tokio::test]
async fn reloads_key_from_secret_store() {
    let mut cluster = Cluster::new();
    let cfg = config("permissions:\n  jmap: [search/*]\n", 4);
    let a = cluster.start("a", tls("a"), &[Role::Jmap], &cfg).await;
    let b = cluster.start("b", tls("b"), &[Role::Search], &cfg).await;
    let echo = svc(Role::Search, "echo");
    b.serve(echo.clone(), Arc::new(Echo("b"))).expect("echo");

    // Смена УЦ: новая пара b — сертификат в файле, ключ в хранилище.
    let dir = cert_dir("secret-reload", "b");
    let bundle = [CA, CA2].concat();
    let (b2, b2_key) = pem("node-b2");
    std::fs::write(dir.join("ca.pem"), &bundle).expect("ca");
    std::fs::write(dir.join("b.pem"), b2).expect("cert");
    let section = secret_tls(&dir, "b");
    let store = Arc::new(MemSecretStore::default());
    let once = || futures::stream::iter([()]);

    // Прежний ключ с новым сертификатом, затем недоступное хранилище: действует прежняя пара.
    store.set(&key_ref("b"), key_text("b"));
    b.follow_tls(&section, store.clone(), once()).await;
    store.set(&key_ref("b"), std::str::from_utf8(b2_key).expect("pem"));
    store.seal();
    b.follow_tls(&section, store.clone(), once()).await;
    assert_eq!(
        a.call(&echo, "k", Bytes::new(), LONG).await,
        Ok(Bytes::from("b|a|k|"))
    );

    store.unseal();
    b.follow_tls(&section, store.clone(), once()).await;
    // Сертификат b теперь от второго УЦ: узел с прежним УЦ его не принимает, с обоими — принимает.
    let c = cluster.start("c", tls("c"), &[Role::Jmap], &cfg).await;
    assert_eq!(
        c.call(&echo, "k", Bytes::new(), LONG).await,
        Err(RpcError::Unavailable)
    );
    let (c_cert, c_key) = pem("c");
    c.reload_tls(NodeTls::from_pem(&bundle, c_cert, c_key).expect("c tls"))
        .expect("reload c");
    assert_eq!(
        c.call(&echo, "k", Bytes::new(), LONG).await,
        Ok(Bytes::from("b|c|k|"))
    );
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

/// CLU-17: новая версия секрета ключа подхватывается опросом за период, без событий и без перезапуска;
/// недоступное хранилище и неизменная версия пару не трогают.
#[tokio::test]
async fn polls_key_secret_version() {
    let mut cluster = Cluster::new();
    let cfg = config("permissions:\n  jmap: [search/*]\n", 4);
    let mut b_cfg = cfg.clone();
    b_cfg.key_poll_interval = Duration::from_millis(20);
    let a = cluster.start("a", tls("a"), &[Role::Jmap], &cfg).await;
    let b = cluster.start("b", tls("b"), &[Role::Search], &b_cfg).await;
    let echo = svc(Role::Search, "echo");
    b.serve(echo.clone(), Arc::new(Echo("b"))).expect("echo");

    // Сертификат b в файле уже от второго УЦ, в хранилище — прежний ключ: пока версия не сменилась, b работает
    // на прежней паре.
    let dir = cert_dir("secret-poll", "b");
    let bundle = [CA, CA2].concat();
    let (b2, b2_key) = pem("node-b2");
    std::fs::write(dir.join("ca.pem"), &bundle).expect("ca");
    std::fs::write(dir.join("b.pem"), b2).expect("cert");
    let section = secret_tls(&dir, "b");
    let store = Arc::new(MemSecretStore::default());
    store.set(&key_ref("b"), key_text("b"));
    let follower = b.clone();
    let follow = tokio::spawn({
        let store: Arc<dyn SecretStore> = store.clone();
        async move {
            follower
                .follow_tls(&section, store, futures::stream::pending())
                .await;
        }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        a.call(&echo, "k", Bytes::new(), LONG).await,
        Ok(Bytes::from("b|a|k|"))
    );
    store.seal();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        a.call(&echo, "k", Bytes::new(), LONG).await,
        Ok(Bytes::from("b|a|k|"))
    );

    // Новая версия секрета — новая пара за период опроса.
    store.unseal();
    store.set(&key_ref("b"), std::str::from_utf8(b2_key).expect("pem"));
    let (c_cert, c_key) = pem("c");
    let c = cluster
        .start(
            "c",
            NodeTls::from_pem(&bundle, c_cert, c_key).expect("c tls"),
            &[Role::Jmap],
            &cfg,
        )
        .await;
    let fresh = cluster.start("d", tls("d"), &[Role::Jmap], &cfg).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // Новый клиент d (без соединений из пула) с прежним УЦ: сервер b с новой парой он не принимает.
        fresh.reload_tls(tls("d")).expect("reload d");
        if fresh.call(&echo, "k", Bytes::new(), LONG).await == Err(RpcError::Unavailable) {
            break;
        }
        assert!(Instant::now() < deadline, "new key version not picked up");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        c.call(&echo, "k", Bytes::new(), LONG).await,
        Ok(Bytes::from("b|c|k|"))
    );
    follow.abort();
    std::fs::remove_dir_all(&dir).expect("cleanup");
}

/// CLU-17: опрос выключен (`0s`) — новая версия секрета без события не применяется.
#[tokio::test]
async fn key_poll_disabled() {
    let mut cluster = Cluster::new();
    let mut cfg = config("permissions:\n  jmap: [search/*]\n", 4);
    cfg.key_poll_interval = Duration::ZERO;
    let b = cluster.start("b", tls("b"), &[Role::Search], &cfg).await;
    let echo = svc(Role::Search, "echo");
    b.serve(echo.clone(), Arc::new(Echo("b"))).expect("echo");
    let dir = cert_dir("secret-nopoll", "b");
    let (b2, b2_key) = pem("node-b2");
    std::fs::write(dir.join("ca.pem"), [CA, CA2].concat()).expect("ca");
    std::fs::write(dir.join("b.pem"), b2).expect("cert");
    let section = secret_tls(&dir, "b");
    let store = Arc::new(MemSecretStore::default());
    store.set(&key_ref("b"), key_text("b"));
    let follower = b.clone();
    let follow = tokio::spawn({
        let store: Arc<dyn SecretStore> = store.clone();
        async move {
            follower
                .follow_tls(&section, store, futures::stream::pending())
                .await;
        }
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    // Подходящая новая пара в хранилище и файле, но без опроса и событий b остаётся на прежней.
    store.set(&key_ref("b"), std::str::from_utf8(b2_key).expect("pem"));
    tokio::time::sleep(Duration::from_millis(100)).await;
    let a = cluster.start("a", tls("a"), &[Role::Jmap], &cfg).await;
    assert_eq!(
        a.call(&echo, "k", Bytes::new(), LONG).await,
        Ok(Bytes::from("b|a|k|"))
    );
    follow.abort();
    std::fs::remove_dir_all(&dir).expect("cleanup");
}
