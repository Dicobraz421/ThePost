//! Окружение тестов: хранилище и фейки testkit, сессия над потоком в памяти, клиент по строкам.

use std::sync::Arc;

use time::OffsetDateTime;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader, DuplexStream};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tp_contracts::auth::Principal;
use tp_contracts::ext::DecisionHooks;
use tp_contracts::mailstore::{Delivered, Delivery, IngestRequest, MailStore};
use tp_contracts::meta::{AccountScope, AccountStore, AccountUpsert, Flags, SpecialUse};
use tp_contracts::{DomainId, Error, MailboxId, Result, TenantId};
use tp_mailstore::Store;
use tp_testkit::{
    Clock, FakeAuthenticator, FakeSasl, MemBlobIndex, MemBlobStore, MemEventBus, MemMetadata,
    MemQuota, MemTrash, SystemClock,
};

use crate::config::ImapConfig;
use crate::session::{Deps, Session, SessionCtx, Transport};

mod fetch;
mod mailbox;
mod session;
mod store;

pub(crate) const LOGIN: &str = "user@example.org";
pub(crate) const PASSWORD: &str = "secret";

/// Поток в памяти вместо соединения; STARTTLS недоступен.
pub(crate) struct TestConn {
    io: DuplexStream,
    tls: bool,
}

impl Transport for TestConn {
    type Io = DuplexStream;

    fn io(&mut self) -> &mut DuplexStream {
        &mut self.io
    }

    fn tls(&self) -> bool {
        self.tls
    }

    fn can_starttls(&self) -> bool {
        false
    }

    async fn starttls(self) -> Result<Self> {
        Err(Error::invalid("no tls in tests"))
    }
}

pub(crate) struct Env {
    pub meta: MemMetadata,
    pub store: Store,
    pub auth: FakeAuthenticator,
    pub principal: Principal,
    pub config: ImapConfig,
    pub shutdown: CancellationToken,
}

impl Env {
    pub async fn new() -> Self {
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let blobs = MemBlobStore::new(clock.clone());
        let index = MemBlobIndex::new(clock.clone());
        let meta = MemMetadata::new(clock).with_blob_index(index.clone());
        let store = Store::new(tp_mailstore::Deps {
            blobs: Arc::new(blobs),
            index: Arc::new(index),
            messages: Arc::new(meta.clone()),
            changes: Arc::new(meta.clone()),
            trash: Arc::new(MemTrash::new(&meta)),
            quota: Arc::new(MemQuota::new(&meta)),
            bus: Arc::new(MemEventBus::default()),
            hooks: DecisionHooks::default(),
        });
        let tenant = TenantId::DEFAULT;
        let account = meta
            .upsert(
                tenant,
                AccountUpsert {
                    domain: DomainId::new(),
                    login: LOGIN.into(),
                    display_name: "User".into(),
                    external_id: None,
                    enabled: true,
                },
            )
            .await
            .expect("account");
        let principal = Principal {
            tenant,
            account: account.id,
            login: LOGIN.into(),
            groups: Default::default(),
            scopes: None,
        };
        let auth = FakeAuthenticator::default();
        auth.add_password(LOGIN, PASSWORD, principal.clone());
        auth.add_token("good-token", principal.clone());
        Self {
            meta,
            store,
            auth,
            principal,
            config: ImapConfig::default(),
            shutdown: CancellationToken::new(),
        }
    }

    pub fn scope(&self) -> AccountScope {
        AccountScope {
            tenant: self.principal.tenant,
            account: self.principal.account,
        }
    }

    pub fn ctx(&self) -> Arc<SessionCtx> {
        Arc::new(SessionCtx {
            config: Arc::new(self.config.clone()),
            deps: Deps {
                sasl: Arc::new(FakeSasl::new(Arc::new(self.auth.clone()))),
                auth: Arc::new(self.auth.clone()),
                mailboxes: Arc::new(self.meta.clone()),
                messages: Arc::new(self.meta.clone()),
                store: Arc::new(self.store.clone()),
            },
            shutdown: self.shutdown.clone(),
        })
    }

    /// Клиент новой сессии; `tls` — канал считается защищённым.
    pub async fn client(&self, tls: bool) -> Client {
        let (server, client) = tokio::io::duplex(1 << 20);
        let session = Session::new(self.ctx(), TestConn { io: server, tls });
        let task = tokio::spawn(session.run());
        let mut c = Client {
            io: BufReader::new(client),
            task,
        };
        let greeting = c.line().await;
        assert!(greeting.starts_with("* OK [CAPABILITY "), "{greeting}");
        c
    }

    /// Клиент после `LOGIN` по защищённому каналу.
    pub async fn logged_in(&self) -> Client {
        let mut c = self.client(true).await;
        let r = c.cmd(&format!("L1 LOGIN {LOGIN} {PASSWORD}")).await;
        assert!(r.contains("L1 OK"), "{r}");
        c
    }

    pub fn mailbox(&self, role: SpecialUse) -> MailboxId {
        self.meta
            .mailbox_by_role(self.scope(), role)
            .expect("mailbox")
    }

    /// Доставляет письмо в папку.
    pub async fn deliver(&self, mailbox: MailboxId, raw: &[u8], flags: Flags) -> Delivered {
        let chunk = bytes::Bytes::copy_from_slice(raw);
        let req = IngestRequest {
            tenant: self.principal.tenant,
            mail_from: "sender@example.net".into(),
            raw: Box::pin(futures::stream::iter([Ok(chunk)])),
            deliveries: vec![Delivery {
                account: self.principal.account,
                mailbox,
                extra_headers: Vec::new(),
                flags,
            }],
            received_at: OffsetDateTime::from_unix_timestamp(1_790_000_000).expect("time"),
        };
        self.store.ingest(req).await.expect("ingest").delivered[0]
    }
}

pub(crate) struct Client {
    io: BufReader<DuplexStream>,
    pub task: JoinHandle<Result<()>>,
}

impl Client {
    /// Строка ответа (с CRLF); пустая — соединение закрыто.
    pub async fn line(&mut self) -> String {
        let mut buf = Vec::new();
        self.io.read_until(b'\n', &mut buf).await.expect("read");
        String::from_utf8_lossy(&buf).into_owned()
    }

    pub async fn send(&mut self, data: &str) {
        self.io
            .get_mut()
            .write_all(data.as_bytes())
            .await
            .expect("write");
    }

    /// Отправляет команду `tag ...` и читает ответы до завершающего с тем же тегом.
    pub async fn cmd(&mut self, line: &str) -> String {
        let tag = line.split(' ').next().unwrap_or_default().to_owned();
        self.send(&format!("{line}\r\n")).await;
        self.until(&tag).await
    }

    /// Читает ответы до строки с тегом `tag` (включительно).
    pub async fn until(&mut self, tag: &str) -> String {
        let mut out = String::new();
        loop {
            let line = self.line().await;
            assert!(!line.is_empty(), "connection closed, got: {out}");
            out.push_str(&line);
            if line.starts_with(&format!("{tag} ")) {
                return out;
            }
        }
    }

    /// Всё до закрытия соединения сервером.
    pub async fn rest(&mut self) -> String {
        let mut buf = Vec::new();
        self.io.read_to_end(&mut buf).await.expect("read");
        String::from_utf8_lossy(&buf).into_owned()
    }
}
