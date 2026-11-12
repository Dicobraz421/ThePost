//! Поиск primary и переключение СУБД (META-25): две БД одного сервера — два «экземпляра», перевод primary
//! в режим только для чтения с обрывом сеансов (как при перезапуске бывшего primary репликой) или пропавший
//! адрес (прокси TCP, META-38).

mod common;

use std::collections::HashSet;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Executor, PgPool};
use time::OffsetDateTime;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tp_contracts::app_password::AppPassword;
use tp_contracts::delivery::{
    DeliveryDecision, DeliveryEvent, DeliveryEventKind, DeliveryLog, DeliveryQuery,
};
use tp_contracts::events::{EventLog, EventPos, Partition};
use tp_contracts::groups::{DistributionGroup, GroupStore, HeldMessage, SenderRestriction};
use tp_contracts::lease::LeaseStore;
use tp_contracts::membership::NodeId;
use tp_contracts::meta::{
    AccountScope, AccountStore, Flags, MailboxStore, MessageQuery, MessageStore, NewMessage,
    SpecialUse,
};
use tp_contracts::meta::{Policy, PolicyTarget};
use tp_contracts::queue::{DsnParams, OutMessage, OutRcpt, QueueStore};
use tp_contracts::{BlobHash, BlobKey, Error, Gen, GroupId, HeldId, QueueId, TenantId};
use tp_meta::failover::{FailoverConfig, connect_failover};
use tp_meta::pool::NoAcquireObserver;
use tp_meta::schema::db_err;
use tp_meta::{MetaConfig, PgMeta};
use uuid::Uuid;

const APP: &str = "tp_failover_test";

/// Адрес `url` с БД `db`.
fn url_for(url: &str, db: &str) -> String {
    let (base, query) = url
        .split_once('?')
        .map_or((url, None), |(b, q)| (b, Some(q)));
    let rest = base.split_once("://").map_or(base, |(_, r)| r);
    let base = match rest.find('/') {
        Some(i) => &base[..base.len() - rest.len() + i],
        None => base,
    };
    match query {
        Some(q) => format!("{base}/{db}?{q}"),
        None => format!("{base}/{db}"),
    }
}

async fn create_db(admin: &PgPool, url: &str, name: &str) {
    admin
        .execute(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .await
        .expect("create database");
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url_for(url, name))
        .await
        .expect("connect");
    pool.execute("CREATE TABLE writes (id uuid PRIMARY KEY)")
        .await
        .expect("table");
    pool.close().await;
}

async fn ids(url: &str, name: &str) -> HashSet<Uuid> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&url_for(url, name))
        .await
        .expect("connect");
    let rows: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM writes")
        .fetch_all(&pool)
        .await
        .expect("select");
    pool.close().await;
    rows.into_iter().collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn read_only_primary_switches_without_losing_writes() {
    let Some(url) = common::pg_url() else {
        return;
    };
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("подключение к TEST_DATABASE_URL");
    let tag = Uuid::now_v7().simple().to_string();
    let (a, b) = (format!("tp_fo_a_{tag}"), format!("tp_fo_b_{tag}"));
    create_db(&admin, &url, &a).await;
    create_db(&admin, &url, &b).await;

    let mut cfg = common::dev_pool_config(&url_for(&url, &a));
    cfg.urls = vec![common::dev_url(
        &common::split_password(&url_for(&url, &b)).0,
    )];
    cfg.application_name = APP.into();
    cfg.failover = FailoverConfig {
        retry_initial: Duration::from_millis(20),
        retry_max: Duration::from_millis(200),
        retry_for: Duration::from_secs(20),
        probe_timeout: Duration::from_secs(2),
    };
    let fo = connect_failover(&cfg, common::secrets(), Arc::new(NoAcquireObserver))
        .await
        .expect("connect_failover");
    assert_eq!(fo.metrics().primary, 0, "primary — первый адрес");

    let tenant = TenantId::new();
    let mut writers = Vec::new();
    for _ in 0..4 {
        let fo = fo.clone();
        writers.push(tokio::spawn(async move {
            let mut done = Vec::new();
            for _ in 0..50 {
                let id = Uuid::now_v7();
                fo.with_tenant_keyed(
                    tenant,
                    move |tx| {
                        Box::pin(async move {
                            sqlx::query(
                                "INSERT INTO writes (id) VALUES ($1) ON CONFLICT DO NOTHING",
                            )
                            .bind(id)
                            .execute(&mut *tx)
                            .await
                            .map(drop)
                            .map_err(db_err)
                        })
                    },
                    move |tx| {
                        Box::pin(async move {
                            let found: bool = sqlx::query_scalar(
                                "SELECT EXISTS (SELECT 1 FROM writes WHERE id = $1)",
                            )
                            .bind(id)
                            .fetch_one(&mut *tx)
                            .await
                            .map_err(db_err)?;
                            Ok(found.then_some(()))
                        })
                    },
                )
                .await
                .expect("запись переживает переключение");
                done.push(id);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            done
        }));
    }

    // Часть записей — на первом primary; затем он становится репликой (только чтение, сеансы оборваны).
    while ids(&url, &a).await.len() < 40 {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    admin
        .execute(AssertSqlSafe(format!(
            "ALTER DATABASE {a} SET default_transaction_read_only = on"
        )))
        .await
        .expect("read-only");
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = $1 AND application_name = $2",
    )
    .bind(&a)
    .bind(APP)
    .execute(&admin)
    .await
    .expect("terminate");

    let mut written = Vec::new();
    for w in writers {
        written.extend(w.await.expect("writer"));
    }
    assert_eq!(written.len(), 200);

    let (in_a, in_b) = (ids(&url, &a).await, ids(&url, &b).await);
    assert!(!in_a.is_empty(), "до переключения писали в первый primary");
    assert!(!in_b.is_empty(), "после переключения — во второй");
    let lost: Vec<_> = written
        .iter()
        .filter(|id| !in_a.contains(id) && !in_b.contains(id))
        .collect();
    assert!(lost.is_empty(), "потеряны записи: {lost:?}");

    let m = fo.metrics();
    assert_eq!(m.primary, 1, "{m:?}");
    assert_eq!(m.switches, 1, "{m:?}");
    assert!(m.retries >= 1, "{m:?}");
    assert!(m.last_switch.is_some() && !m.switching, "{m:?}");
    assert_eq!(m.gave_up, 0, "{m:?}");

    fo.pool().close().await;
    for db in [&a, &b] {
        admin
            .execute(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
            .await
            .expect("drop database");
    }
}

/// Прокси TCP к серверу тестовой БД: «адрес primary», который можно отключить (узел пропал).
struct Proxy {
    addr: std::net::SocketAddr,
    tasks: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
}

impl Proxy {
    async fn start(upstream: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let tasks: Arc<Mutex<Vec<tokio::task::AbortHandle>>> = Arc::default();
        let conns = tasks.clone();
        let accept = tokio::spawn(async move {
            while let Ok((mut client, _)) = listener.accept().await {
                let upstream = upstream.clone();
                let conn = tokio::spawn(async move {
                    if let Ok(mut server) = TcpStream::connect(&upstream).await {
                        let _ = tokio::io::copy_bidirectional(&mut client, &mut server).await;
                    }
                });
                conns.lock().expect("lock").push(conn.abort_handle());
            }
        });
        tasks.lock().expect("lock").push(accept.abort_handle());
        Self { addr, tasks }
    }

    /// Адрес больше не принимает соединения, открытые — оборваны.
    fn kill(&self) {
        for t in self.tasks.lock().expect("lock").drain(..) {
            t.abort();
        }
    }
}

/// `url` с хостом и портом `addr`.
fn via(url: &str, addr: std::net::SocketAddr) -> String {
    let (scheme, rest) = url.split_once("://").expect("scheme");
    let end = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(end);
    let user = authority.rsplit_once('@').map(|(u, _)| format!("{u}@"));
    format!("{scheme}://{}{addr}{tail}", user.unwrap_or_default())
}

async fn migrated(url: &str, db: &str) -> tp_meta::pool::DbPool {
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url_for(url, db))
        .await
        .expect("connect");
    let pool = common::db(pool);
    tp_meta::schema::migrate(&pool).await.expect("миграции");
    pool
}

/// META-38: `PgMeta` над `Failover` — адрес primary пропал между вызовами: чтение повторено на новом
/// primary и успешно; запись без ключа при отказе не повторяется (`Unavailable` с `retry_after`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn meta_read_retried_on_new_primary() {
    const APP_META: &str = "tp_failover_meta_test";
    let Some(url) = common::pg_url() else {
        return;
    };
    let opts = PgConnectOptions::from_str(&url).expect("url");
    if opts.get_host().starts_with('/') {
        eprintln!("TEST_DATABASE_URL — сокет Unix: прокси TCP невозможен, тест пропущен");
        return;
    }
    let upstream = format!("{}:{}", opts.get_host(), opts.get_port());
    let admin = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("подключение к TEST_DATABASE_URL");
    let tag = Uuid::now_v7().simple().to_string();
    let (a, b) = (format!("tp_fom_a_{tag}"), format!("tp_fom_b_{tag}"));
    create_db(&admin, &url, &a).await;
    create_db(&admin, &url, &b).await;
    migrated(&url, &a).await.close().await;
    // Учётная запись есть только во втором «экземпляре»: её чтение видно лишь после переключения.
    let meta_b = PgMeta::new(migrated(&url, &b).await, MetaConfig::default());
    let scope = common::account(&meta_b).await;
    meta_b.pool().close().await;

    let proxy = Proxy::start(upstream).await;
    let mut cfg = common::dev_pool_config(&via(&url_for(&url, &a), proxy.addr));
    cfg.urls = vec![common::dev_url(
        &common::split_password(&url_for(&url, &b)).0,
    )];
    cfg.application_name = APP_META.into();
    cfg.acquire_timeout = Duration::from_secs(2);
    cfg.failover = FailoverConfig {
        retry_initial: Duration::from_millis(50),
        retry_max: Duration::from_millis(500),
        retry_for: Duration::from_secs(30),
        probe_timeout: Duration::from_secs(2),
    };
    let fo = connect_failover(&cfg, common::secrets(), Arc::new(NoAcquireObserver))
        .await
        .expect("connect_failover");
    let meta = PgMeta::from_failover(&fo, MetaConfig::default());
    assert_eq!(fo.metrics().primary, 0);
    let before = AccountStore::get(&meta, scope.tenant, scope.account)
        .await
        .expect("чтение на первом primary");
    assert!(before.is_none(), "в первом экземпляре учётной записи нет");

    // Первый primary пропал: соединения оборваны, адрес не отвечает.
    proxy.kill();
    let after = AccountStore::get(&meta, scope.tenant, scope.account)
        .await
        .expect("чтение повторено на новом primary");
    assert_eq!(after.map(|acc| acc.id), Some(scope.account));
    let m = fo.metrics();
    assert_eq!((m.primary, m.switches, m.gave_up), (1, 1, 0), "{m:?}");
    assert!(m.retries >= 1, "{m:?}");

    // Новый primary стал репликой, другого нет: запись без ключа — один раз, временный отказ с паузой.
    admin
        .execute(AssertSqlSafe(format!(
            "ALTER DATABASE {b} SET default_transaction_read_only = on"
        )))
        .await
        .expect("read-only");
    sqlx::query(
        "SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
         WHERE datname = $1 AND application_name = $2",
    )
    .bind(&b)
    .bind(APP_META)
    .execute(&admin)
    .await
    .expect("terminate");
    let retries = fo.metrics().retries;
    let err = meta
        .try_acquire("meta38", "node-1", Duration::from_secs(30))
        .await
        .expect_err("запись на реплике");
    assert_eq!(
        err,
        Error::Unavailable {
            retry_after: Some(Duration::from_millis(500))
        }
    );
    assert_eq!(fo.metrics().retries, retries, "запись не повторялась");

    fo.pool().close().await;
    for db in [&a, &b] {
        admin
            .execute(AssertSqlSafe(format!("DROP DATABASE {db} WITH (FORCE)")))
            .await
            .expect("drop database");
    }
}

/// Прокси TCP, обрывающий соединение после COMMIT (META-40): COMMIT транзакции, в которой был запрос с
/// `pattern`, доходит до сервера и фиксируется, ответ сервера клиенту не передаётся — исход клиенту
/// неизвестен. Обрыв однократный: [`CommitCut::arm`] перед каждой проверяемой операцией.
struct CommitCut {
    addr: std::net::SocketAddr,
    pattern: Arc<Mutex<Option<String>>>,
    cuts: Arc<AtomicUsize>,
    accept: tokio::task::AbortHandle,
}

impl CommitCut {
    async fn start(upstream: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let pattern: Arc<Mutex<Option<String>>> = Arc::default();
        let cuts = Arc::new(AtomicUsize::new(0));
        let (p, c) = (pattern.clone(), cuts.clone());
        let accept = tokio::spawn(async move {
            while let Ok((client, _)) = listener.accept().await {
                let (upstream, p, c) = (upstream.clone(), p.clone(), c.clone());
                tokio::spawn(async move {
                    if let Ok(server) = TcpStream::connect(&upstream).await {
                        let _ = cut_after_commit(client, server, p, c).await;
                    }
                });
            }
        });
        Self {
            addr,
            pattern,
            cuts,
            accept: accept.abort_handle(),
        }
    }

    /// Оборвать COMMIT ближайшей транзакции с запросом, содержащим `pattern`.
    fn arm(&self, pattern: &str) {
        *self.pattern.lock().expect("lock") = Some(pattern.to_owned());
    }

    fn cuts(&self) -> usize {
        self.cuts.load(Ordering::SeqCst)
    }
}

impl Drop for CommitCut {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

/// Пересылка сообщений протокола PostgreSQL (`sslmode=disable`) с обрывом после COMMIT.
async fn cut_after_commit(
    client: TcpStream,
    server: TcpStream,
    pattern: Arc<Mutex<Option<String>>>,
    cuts: Arc<AtomicUsize>,
) -> std::io::Result<()> {
    let (mut cr, mut cw) = client.into_split();
    let (mut sr, mut sw) = server.into_split();
    // Ответ на оборванный COMMIT клиенту не передаётся: получен — соединение закрывается.
    let hold = Arc::new(AtomicBool::new(false));
    let replied = Arc::new(tokio::sync::Notify::new());
    let (h, r) = (hold.clone(), replied.clone());
    let s2c = tokio::spawn(async move {
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = match sr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if h.load(Ordering::SeqCst) {
                r.notify_one();
                break;
            }
            if cw.write_all(&buf[..n]).await.is_err() {
                break;
            }
        }
    });
    // Стартовое сообщение — без типа.
    let mut len = [0u8; 4];
    cr.read_exact(&mut len).await?;
    let mut body = vec![0u8; (u32::from_be_bytes(len) as usize).saturating_sub(4)];
    cr.read_exact(&mut body).await?;
    sw.write_all(&len).await?;
    sw.write_all(&body).await?;
    let mut marked = false;
    loop {
        let mut head = [0u8; 5];
        cr.read_exact(&mut head).await?;
        let n = u32::from_be_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let mut body = vec![0u8; n.saturating_sub(4)];
        cr.read_exact(&mut body).await?;
        let text = String::from_utf8_lossy(&body);
        let sql = text.trim_end_matches('\0').trim();
        let wanted = pattern.lock().expect("lock").clone();
        if matches!(head[0], b'Q' | b'P') && wanted.as_deref().is_some_and(|p| sql.contains(p)) {
            marked = true;
        }
        let end = head[0] == b'Q'
            && (sql.eq_ignore_ascii_case("COMMIT") || sql.eq_ignore_ascii_case("ROLLBACK"));
        if end
            && marked
            && sql.eq_ignore_ascii_case("COMMIT")
            && pattern.lock().expect("lock").take().is_some()
        {
            hold.store(true, Ordering::SeqCst);
            sw.write_all(&head).await?;
            sw.write_all(&body).await?;
            // Сервер ответил — COMMIT выполнен; соединение обрывается без ответа клиенту.
            let _ = tokio::time::timeout(Duration::from_secs(10), replied.notified()).await;
            cuts.fetch_add(1, Ordering::SeqCst);
            s2c.abort();
            return Ok(());
        }
        if end {
            marked = false;
        }
        sw.write_all(&head).await?;
        sw.write_all(&body).await?;
    }
}

/// META-40: обрыв соединения после COMMIT — исход записи с ключом устанавливается по ключу, повтор не
/// задваивает запись: вставка письма (одного и пачкой), журнал доставки, очередь модерации, позиция
/// подписчика журнала событий.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keyed_write_not_doubled_after_commit_cut() {
    let Some(direct) = common::meta().await else {
        return;
    };
    let url = common::pg_url().expect("url");
    let opts = PgConnectOptions::from_str(&url).expect("url");
    if opts.get_host().starts_with('/') {
        eprintln!("TEST_DATABASE_URL — сокет Unix: прокси TCP невозможен, тест пропущен");
        return;
    }
    let scope = common::account(&direct).await;
    let tenant = scope.tenant;
    let inbox = MailboxStore::list(&direct, scope)
        .await
        .expect("папки")
        .into_iter()
        .find(|m| m.role == Some(SpecialUse::Inbox))
        .expect("входящие")
        .id;

    let cut = CommitCut::start(format!("{}:{}", opts.get_host(), opts.get_port())).await;
    let mut cfg = common::dev_pool_config(&via(&url, cut.addr));
    cfg.acquire_timeout = Duration::from_secs(5);
    cfg.failover = FailoverConfig {
        retry_initial: Duration::from_millis(50),
        retry_max: Duration::from_millis(500),
        retry_for: Duration::from_secs(30),
        probe_timeout: Duration::from_secs(5),
    };
    let fo = connect_failover(&cfg, common::secrets(), Arc::new(NoAcquireObserver))
        .await
        .expect("connect_failover");
    let meta = PgMeta::from_failover(&fo, MetaConfig::default());

    // Вставка письма.
    cut.arm("INSERT INTO messages");
    let one = meta
        .insert(scope, inbox, new_message(scope))
        .await
        .expect("вставка переживает обрыв после COMMIT");
    assert_eq!(cut.cuts(), 1, "COMMIT вставки оборван");
    cut.arm("INSERT INTO messages");
    let batch = meta
        .insert_batch(
            tenant,
            vec![
                (scope, inbox, new_message(scope)),
                (scope, inbox, new_message(scope)),
            ],
        )
        .await
        .expect("пачка переживает обрыв после COMMIT");
    assert_eq!(cut.cuts(), 2, "COMMIT пачки оборван");
    let listed = MessageStore::list(&direct, scope, inbox, MessageQuery::All, 100)
        .await
        .expect("письма");
    let got: Vec<_> = listed.iter().map(|m| (m.id, m.uid)).collect();
    let want: Vec<_> = std::iter::once(&one)
        .chain(&batch)
        .map(|i| (i.id, i.uid))
        .collect();
    assert_eq!(
        got, want,
        "каждое письмо — один раз, результат — применённой вставки"
    );
    assert!(
        listed
            .iter()
            .zip(std::iter::once(&one).chain(&batch))
            .all(|(m, i)| m.modseq == i.modseq)
    );

    // Журнал доставки.
    let message_id = format!("{}@meta40.test", Uuid::now_v7().simple());
    cut.arm("INSERT INTO delivery_log");
    meta.record(DeliveryEvent {
        tenant,
        at: OffsetDateTime::now_utc(),
        node: NodeId::new("meta40-node").expect("node"),
        kind: DeliveryEventKind::Deliver,
        sender: "a@example.org".into(),
        recipients: vec!["b@example.org".into()],
        message_id: Some(message_id.clone()),
        queue_id: None,
        decision: DeliveryDecision::Accept,
        reason: None,
    })
    .await
    .expect("журнал доставки переживает обрыв после COMMIT");
    assert_eq!(cut.cuts(), 3, "COMMIT журнала доставки оборван");
    let mut q = DeliveryQuery::all(10);
    q.message_id = Some(message_id);
    let page = direct.search(tenant, &q).await.expect("поиск");
    assert_eq!(page.events.len(), 1, "запись журнала доставки — одна");

    // Очередь модерации.
    let group = DistributionGroup {
        id: GroupId::new(),
        tenant,
        name: "meta40".into(),
        address: None,
        external_id: None,
        members: Vec::new(),
        senders: SenderRestriction::Anyone,
        moderators: Vec::new(),
        hidden_from_gal: false,
    };
    GroupStore::put(&direct, group.clone())
        .await
        .expect("группа");
    let held = HeldMessage {
        id: HeldId::new(),
        group: Some(group.id),
        source: Default::default(),
        moderators: Vec::new(),
        recipients: Vec::new(),
        sender: "a@example.org".into(),
        sender_account: None,
        message_id: None,
        blob: blob_key(scope),
        size: 10,
        held_at: OffsetDateTime::now_utc(),
    };
    cut.arm("INSERT INTO group_held");
    meta.hold(tenant, held.clone())
        .await
        .expect("очередь модерации переживает обрыв после COMMIT");
    assert_eq!(cut.cuts(), 4, "COMMIT очереди модерации оборван");
    let list = direct
        .held(tenant, Some(group.id), 10)
        .await
        .expect("очередь");
    assert_eq!(list.iter().map(|h| h.id).collect::<Vec<_>>(), vec![held.id]);

    // Позиция подписчика журнала событий.
    let subscriber = format!("meta40-{}", Uuid::now_v7().simple());
    cut.arm("INSERT INTO event_cursors");
    EventLog::commit(&meta, &subscriber, Partition(0), EventPos(7))
        .await
        .expect("позиция переживает обрыв после COMMIT");
    assert_eq!(cut.cuts(), 5, "COMMIT позиции оборван");
    assert_eq!(
        direct
            .position(&subscriber, Partition(0))
            .await
            .expect("позиция"),
        EventPos(7)
    );

    // Очередь отправки (META-62).
    cut.arm("INSERT INTO queue_rcpts");
    let queued = meta
        .enqueue(None, out_message(scope))
        .await
        .expect("постановка в очередь переживает обрыв после COMMIT");
    assert_eq!(cut.cuts(), 6, "COMMIT постановки оборван");
    assert_eq!(
        queue_rows(&direct, tenant).await,
        vec![queued],
        "запись очереди — одна"
    );

    // Пароль приложения (META-62).
    let password = app_password();
    cut.arm("INSERT INTO app_passwords");
    tp_contracts::app_password::AppPasswordStore::insert(&meta, scope, password.clone())
        .await
        .expect("пароль приложения переживает обрыв после COMMIT");
    assert_eq!(cut.cuts(), 7, "COMMIT пароля оборван");
    assert_eq!(password_ids(&direct, scope).await, vec![password.id]);

    let m = fo.metrics();
    assert!(m.retries >= 7, "{m:?}");
    assert_eq!((m.switches, m.gave_up), (0, 0), "{m:?}");
    fo.pool().close().await;
}

/// META-62: сеанс записи оборван на сервере (`pg_terminate_backend`) до COMMIT — запись повторена и
/// выполнена ровно один раз: постановка в очередь, пароль приложения, запись, идемпотентная по сути
/// (`set_policy`). Запись задерживается блокировкой таблицы в другом сеансе, пока её сеанс не оборван.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn write_retried_after_backend_terminated() {
    let Some(direct) = common::meta().await else {
        return;
    };
    let url = common::pg_url().expect("url");
    let scope = common::account(&direct).await;
    let tenant = scope.tenant;
    let mut cfg = common::dev_pool_config(&url);
    cfg.acquire_timeout = Duration::from_secs(5);
    cfg.failover = FailoverConfig {
        retry_initial: Duration::from_millis(50),
        retry_max: Duration::from_millis(500),
        retry_for: Duration::from_secs(30),
        probe_timeout: Duration::from_secs(5),
    };
    let fo = connect_failover(&cfg, common::secrets(), Arc::new(NoAcquireObserver))
        .await
        .expect("connect_failover");
    let meta = PgMeta::from_failover(&fo, MetaConfig::default());

    let queued = terminated_during(&direct, "queue", "INSERT INTO queue", {
        let meta = meta.clone();
        async move { meta.enqueue(None, out_message(scope)).await }
    })
    .await
    .expect("постановка в очередь переживает обрыв сеанса");
    assert_eq!(
        queue_rows(&direct, tenant).await,
        vec![queued],
        "запись очереди — одна"
    );

    let password = app_password();
    terminated_during(&direct, "app_passwords", "INSERT INTO app_passwords", {
        let (meta, password) = (meta.clone(), password.clone());
        async move { tp_contracts::app_password::AppPasswordStore::insert(&meta, scope, password).await }
    })
    .await
    .expect("пароль приложения переживает обрыв сеанса");
    assert_eq!(password_ids(&direct, scope).await, vec![password.id]);

    let policy = Policy {
        quota_bytes: Some(4096),
        ..Policy::default()
    };
    terminated_during(&direct, "policies", "INSERT INTO policies", {
        let (meta, policy) = (meta.clone(), policy.clone());
        async move {
            meta.set_policy(tenant, PolicyTarget::Account(scope.account), &policy)
                .await
        }
    })
    .await
    .expect("политика переживает обрыв сеанса");
    assert_eq!(
        direct
            .policy(tenant, PolicyTarget::Account(scope.account))
            .await
            .expect("политика"),
        policy
    );

    let m = fo.metrics();
    assert!(m.retries >= 3, "{m:?}");
    fo.pool().close().await;
}

/// Выполняет `op`, пока таблица `table` заблокирована в отдельном сеансе; сеанс `op`, ждущий блокировку
/// на запросе `query`, обрывается `pg_terminate_backend`, затем блокировка снимается.
async fn terminated_during<T: Send + 'static>(
    direct: &PgMeta,
    table: &str,
    query: &str,
    op: impl std::future::Future<Output = tp_contracts::Result<T>> + Send + 'static,
) -> tp_contracts::Result<T> {
    let mut lock = common::conn(direct.pool()).await;
    lock.execute(AssertSqlSafe(format!(
        "BEGIN; LOCK TABLE {table} IN EXCLUSIVE MODE"
    )))
    .await
    .expect("блокировка");
    let task = tokio::spawn(op);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut conn = common::conn(direct.pool()).await;
    loop {
        let killed: Vec<bool> = sqlx::query_scalar(
            "SELECT pg_terminate_backend(pid) FROM pg_stat_activity
             WHERE pid <> pg_backend_pid() AND wait_event_type = 'Lock' AND query LIKE $1",
        )
        .bind(format!("{query}%"))
        .fetch_all(&mut *conn)
        .await
        .expect("pg_terminate_backend");
        if killed.iter().any(|k| *k) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "запись не дошла до блокировки"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    lock.execute("ROLLBACK").await.expect("снять блокировку");
    task.await.expect("задача")
}

fn out_message(scope: AccountScope) -> OutMessage {
    OutMessage {
        tenant: scope.tenant,
        blob: blob_key(scope),
        size: 10,
        from: "a@example.org".into(),
        dsn: DsnParams::default(),
        rcpts: vec![OutRcpt::new("b@example.org")],
        next_attempt: OffsetDateTime::now_utc(),
    }
}

fn app_password() -> AppPassword {
    AppPassword {
        id: Uuid::now_v7().simple().to_string(),
        label: "Телефон".into(),
        hash: format!("$argon2id$v=19$m=19456,t=2,p=1${}", Uuid::now_v7().simple()),
        created_at: OffsetDateTime::now_utc(),
        revoked_at: None,
    }
}

/// Записи очереди организации.
async fn queue_rows(direct: &PgMeta, tenant: TenantId) -> Vec<QueueId> {
    let mut tx = tp_meta::schema::begin(direct.pool(), tenant)
        .await
        .expect("begin");
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT queue_id FROM queue WHERE tenant_id = $1")
        .bind(tenant.as_uuid())
        .fetch_all(&mut *tx)
        .await
        .expect("очередь");
    tx.commit().await.expect("commit");
    ids.into_iter().map(QueueId::from_uuid).collect()
}

/// Идентификаторы паролей приложения учётной записи.
async fn password_ids(direct: &PgMeta, scope: AccountScope) -> Vec<String> {
    tp_contracts::app_password::AppPasswordStore::list(direct, scope)
        .await
        .expect("пароли")
        .into_iter()
        .map(|p| p.id)
        .collect()
}

fn blob_key(scope: AccountScope) -> BlobKey {
    let mut h = [0u8; 32];
    h[..16].copy_from_slice(Uuid::now_v7().as_bytes());
    h[16..].copy_from_slice(Uuid::now_v7().as_bytes());
    BlobKey {
        tenant: scope.tenant,
        hash: BlobHash(h),
        generation: Gen::INITIAL,
    }
}

fn new_message(scope: AccountScope) -> NewMessage {
    NewMessage {
        skeleton: blob_key(scope),
        parts: Vec::new(),
        size: 100,
        received_at: OffsetDateTime::now_utc(),
        flags: Flags::default(),
        meta: Default::default(),
    }
}
