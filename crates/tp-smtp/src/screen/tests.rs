//! Экран соединений: проверки сессии над `tokio::io::duplex`, кэш и частота на управляемом времени, роль
//! smtp-in на `127.0.0.1` (адрес исключён из `exempt`).

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tp_contracts::role::{CancellationToken, ReadyHandle, RoleCtx, fixed_config};
use tp_platform::{EndpointConfig, Listener};

use super::*;
use crate::config::SmtpConfig;
use crate::server::tests::{Client, Recorder, conn_info};
use crate::server::{Exit, Mode, ServerDeps, Session, SessionCtx, SmtpService};

fn probe(enforce: bool) -> Probe {
    Probe {
        greet_delay: Duration::from_millis(300),
        pipelining: true,
        enforce,
    }
}

fn ctx(metrics: Option<Arc<SmtpMetrics>>) -> SessionCtx {
    SessionCtx {
        mode: Mode::Inbound,
        config: Arc::new(SmtpConfig {
            hostname: "mx.example.org".into(),
            ..SmtpConfig::default()
        }),
        handler: Arc::new(Recorder::default()),
        sasl: None,
        limiter: None,
        send_limits: None,
        metrics,
        screen: None,
    }
}

type Task = tokio::task::JoinHandle<(std::io::Result<Exit>, Option<Result<(), Violation>>)>;

fn session(p: Probe, metrics: Option<Arc<SmtpMetrics>>) -> (Client<tokio::io::DuplexStream>, Task) {
    let (client, mut server) = tokio::io::duplex(64 * 1024);
    let task = tokio::spawn(async move {
        let mut s = Session::new(ctx(metrics), conn_info(false), false);
        s.screening(p);
        let exit = s.serve(&mut server, true).await;
        (exit, s.screen_outcome())
    });
    (Client::new(client), task)
}

#[tokio::test]
async fn pregreet_client_is_rejected() {
    let registry = tp_platform::Metrics::default();
    let metrics = Arc::new(SmtpMetrics::new());
    metrics.register(&registry);
    let (mut c, task) = session(probe(true), Some(metrics));
    // Команда до конца приветствия.
    c.send("EHLO bot.example\r\n").await;
    let (code, lines) = c.reply().await;
    assert_eq!(code, 554, "{lines:?}");
    assert!(lines[0].starts_with("mx.example.org"), "первая строка 220-");
    assert!(lines[1].contains("talked before greeting"), "{lines:?}");
    assert!(c.closed().await);
    let (exit, outcome) = task.await.expect("join");
    assert_eq!(exit.expect("io"), Exit::Closed);
    assert_eq!(outcome, Some(Err(Violation::Pregreet)));
    let text = registry.encode().expect("encode");
    assert!(
        text.contains(r#"tp_smtp_screen_violations_total{reason="pregreet",action="rejected"} 1"#),
        "{text}"
    );
}

#[tokio::test]
async fn patient_client_passes() {
    let (mut c, task) = session(probe(true), None);
    let (code, lines) = c.reply().await;
    assert_eq!(code, 220);
    assert_eq!(lines.len(), 2, "приветствие из двух строк: {lines:?}");
    assert_eq!(c.cmd("EHLO client.example.net").await, 250);
    // После EHLO конвейер разрешён (PIPELINING объявлен).
    c.send("MAIL FROM:<a@example.net>\r\nRSET\r\n").await;
    assert_eq!(c.code().await, 250);
    assert_eq!(c.code().await, 250);
    assert_eq!(c.cmd("QUIT").await, 221);
    let (exit, outcome) = task.await.expect("join");
    assert_eq!(exit.expect("io"), Exit::Quit);
    assert_eq!(outcome, Some(Ok(())));
}

#[tokio::test]
async fn pipelining_without_negotiation_is_rejected() {
    let (mut c, task) = session(probe(true), None);
    assert_eq!(c.code().await, 220);
    // Вслед за EHLO, не дождавшись ответа.
    c.send("EHLO bot.example\r\nMAIL FROM:<a@example.net>\r\n")
        .await;
    assert_eq!(c.code().await, 250);
    let (code, lines) = c.reply().await;
    assert_eq!(code, 554);
    assert!(lines[0].contains("pipelining"), "{lines:?}");
    assert!(c.closed().await);
    let (_, outcome) = task.await.expect("join");
    assert_eq!(outcome, Some(Err(Violation::Pipelining)));

    // После HELO (PIPELINING не объявлен).
    let (mut c, task) = session(probe(true), None);
    assert_eq!(c.code().await, 220);
    assert_eq!(c.cmd("HELO bot.example").await, 250);
    c.send("MAIL FROM:<a@example.net>\r\nRCPT TO:<b@example.org>\r\n")
        .await;
    assert_eq!(c.code().await, 250);
    assert_eq!(c.code().await, 554);
    let (_, outcome) = task.await.expect("join");
    assert_eq!(outcome, Some(Err(Violation::Pipelining)));
}

#[tokio::test]
async fn log_mode_keeps_session() {
    let (mut c, task) = session(probe(false), None);
    c.send("EHLO bot.example\r\n").await;
    // Приветствие целиком, затем ответ на раннюю команду.
    let (code, lines) = c.reply().await;
    assert_eq!(code, 220, "{lines:?}");
    assert_eq!(c.code().await, 250);
    c.send("NOOP\r\nNOOP\r\n").await;
    assert_eq!(c.code().await, 250);
    assert_eq!(c.code().await, 250);
    assert_eq!(c.cmd("QUIT").await, 221);
    let (_, outcome) = task.await.expect("join");
    assert_eq!(outcome, Some(Err(Violation::Pregreet)));
}

fn cfg() -> ScreenConfig {
    ScreenConfig {
        max_rate: 3,
        exempt: vec!["10.0.0.0/8".into(), "2001:db8::1".into()],
        ..ScreenConfig::default()
    }
}

#[test]
fn cache_rate_and_exempt() {
    let s = Screen::default();
    let cfg = cfg();
    let ip: IpAddr = "192.0.2.10".parse().expect("ip");
    let t0 = Instant::now();
    assert!(matches!(s.admit_at(&cfg, ip, t0), Admission::Probe(_)));
    // Прошёл проверки: повтор из кэша без проверки до `pass_ttl`.
    s.record_at(&cfg, ip, Some(Ok(())), t0);
    assert_eq!(s.admit_at(&cfg, ip, t0), Admission::Passed);
    let later = t0 + cfg.pass_ttl + Duration::from_secs(1);
    assert!(matches!(s.admit_at(&cfg, ip, later), Admission::Probe(_)));
    // Нарушитель: отказ из кэша до `reject_ttl`.
    let bad: IpAddr = "::ffff:198.51.100.7".parse().expect("ip");
    s.record_at(&cfg, bad, Some(Err(Violation::Pregreet)), t0);
    assert_eq!(
        s.admit_at(&cfg, "198.51.100.7".parse().expect("ip"), t0),
        Admission::Reject {
            violation: Violation::Pregreet,
            cached: true
        }
    );
    assert!(matches!(
        s.admit_at(&cfg, bad, t0 + cfg.reject_ttl),
        Admission::Probe(_)
    ));
    // Частота: четвёртое соединение в окне — отказ и запись в кэш.
    let fast: IpAddr = "203.0.113.5".parse().expect("ip");
    for _ in 0..3 {
        assert!(matches!(s.admit_at(&cfg, fast, t0), Admission::Probe(_)));
    }
    assert_eq!(
        s.admit_at(&cfg, fast, t0),
        Admission::Reject {
            violation: Violation::Rate,
            cached: false
        }
    );
    assert!(matches!(
        s.admit_at(&cfg, fast, t0 + Duration::from_secs(1)),
        Admission::Reject { cached: true, .. }
    ));
    // Исключённые сети и выключенный экран — без проверок.
    for ip in ["10.1.2.3", "2001:db8::1"] {
        assert_eq!(
            s.admit_at(&cfg, ip.parse().expect("ip"), t0),
            Admission::Exempt
        );
    }
    let off = ScreenConfig {
        enabled: false,
        ..cfg.clone()
    };
    assert_eq!(s.admit_at(&off, bad, t0), Admission::Exempt);
    // Режим журнала: нарушитель не запоминается, частота не отклоняет.
    let log = ScreenConfig {
        mode: ScreenMode::Log,
        ..cfg
    };
    let other: IpAddr = "203.0.113.9".parse().expect("ip");
    s.record_at(&log, other, Some(Err(Violation::Pipelining)), t0);
    for _ in 0..5 {
        assert!(matches!(s.admit_at(&log, other, t0), Admission::Probe(_)));
    }
}

/// Роль smtp-in на `127.0.0.1` с экраном без исключений.
async fn server() -> (std::net::SocketAddr, CancellationToken) {
    let tcp = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = tcp.local_addr().expect("addr");
    let listener = Listener::from_tcp(tcp, &EndpointConfig::new(addr), None).expect("listener");
    let svc = SmtpService::new(
        Mode::Inbound,
        ServerDeps {
            handler: Arc::new(Recorder::default()),
            sasl: None,
            limiter: None,
            tls: None,
            send_limits: None,
            metrics: None,
        },
    );
    let shutdown = CancellationToken::new();
    let ctx = RoleCtx {
        config: fixed_config(SmtpConfig {
            hostname: "mx.example.org".into(),
            screen: ScreenConfig {
                greet_delay: Duration::from_millis(300),
                exempt: Vec::new(),
                ..ScreenConfig::default()
            },
            ..SmtpConfig::default()
        }),
        shutdown: shutdown.clone(),
        drain_deadline: Duration::from_secs(1),
        ready: ReadyHandle::default(),
        load: tp_contracts::load::fixed_load(tp_contracts::load::LoadLevel::Normal),
    };
    tokio::spawn(async move { svc.serve(vec![listener], ctx).await });
    (addr, shutdown)
}

/// Первая строка ответа сервера.
async fn first_line(s: &mut TcpStream) -> String {
    let mut buf = Vec::new();
    let mut b = [0u8; 1];
    while !buf.ends_with(b"\r\n") {
        let n = tokio::time::timeout(Duration::from_secs(5), s.read(&mut b))
            .await
            .expect("timeout")
            .expect("read");
        assert!(n > 0, "closed");
        buf.push(b[0]);
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Соединение с адреса `127.0.0.<n>`.
async fn connect_from(n: u8, addr: std::net::SocketAddr) -> TcpStream {
    let sock = tokio::net::TcpSocket::new_v4().expect("socket");
    sock.bind(std::net::SocketAddr::from(([127, 0, 0, n], 0)))
        .expect("bind");
    sock.connect(addr).await.expect("connect")
}

#[tokio::test]
async fn role_caches_results_by_ip() {
    let (addr, stop) = server().await;
    // Клиент прошёл проверки: следующее соединение — приветствие сразу, одной строкой.
    let mut c = Client::new(connect_from(1, addr).await);
    let (code, lines) = c.reply().await;
    assert_eq!((code, lines.len()), (220, 2));
    assert_eq!(c.cmd("QUIT").await, 221);
    assert!(c.closed().await);
    let mut s = connect_from(1, addr).await;
    assert!(first_line(&mut s).await.starts_with("220 "));
    drop(s);
    // Клиент, заговоривший раньше приветствия: отказ; следующее соединение — отказ из кэша без проверки.
    let mut s = connect_from(2, addr).await;
    s.write_all(b"EHLO bot.example\r\n").await.expect("write");
    let mut all = String::new();
    s.read_to_string(&mut all).await.expect("read");
    assert!(all.starts_with("220-mx.example.org"), "{all}");
    assert!(all.contains("\r\n554 5.5.1 "), "{all}");
    // Итог сессии записывается после закрытия соединения.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let mut s = connect_from(2, addr).await;
    let mut all = String::new();
    s.read_to_string(&mut all).await.expect("read");
    assert!(all.starts_with("554 5.5.1 "), "{all}");
    stop.cancel();
}
