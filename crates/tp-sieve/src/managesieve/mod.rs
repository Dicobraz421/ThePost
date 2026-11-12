//! ManageSieve (RFC 5804; SIEVE-08, SIEVE-09): управление скриптами учётной записи по сети.
//!
//! Работает в роли `imap` (CONTRACTS.md#Sieve): слушатели (`sieve.managesieve.listen`) открывает приложение и передаёт
//! их [`ManageSieve`] ([`RoleService`]) либо само вызывает [`serve`] на каждый слушатель. Сеанс — [`Session`] поверх
//! [`Transport`] (соединение платформы или поток в тестах): приветствие с возможностями, `STARTTLS`, вход SASL
//! (контракт [`SaslServer`]), команды скриптов поверх [`SieveStore`] — в контексте организации и учётной записи
//! вошедшего субъекта.
//!
//! Вход без TLS запрещён (`NO (ENCRYPT-NEEDED)`), кроме `profile: dev`. Лимиты строки и литерала, таймаут бездействия,
//! число соединений — секция `sieve.managesieve` ([`ManageSieveConfig`](crate::ManageSieveConfig)).

mod proto;
mod scripts;
mod session;

use std::fmt;
use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tp_contracts::Result;
use tp_contracts::role::{RoleCtx, RoleService};
use tp_contracts::sasl::SaslServer;
use tp_contracts::sieve::SieveStore;
use tp_contracts::tls::Profile;
use tp_platform::listen::{Conn, Listener};

pub use session::Session;

use crate::SieveConfig;

/// Канал сеанса: поток байтов, признак TLS, апгрейд `STARTTLS`.
pub trait Transport: Send + Sized + 'static {
    /// Поток данных.
    type Io: AsyncRead + AsyncWrite + Unpin + Send;

    /// Поток данных.
    fn io(&mut self) -> &mut Self::Io;

    /// Канал защищён TLS.
    fn tls(&self) -> bool;

    /// Можно предложить `STARTTLS`.
    fn can_starttls(&self) -> bool;

    /// Рукопожатие TLS после ответа на `STARTTLS`.
    fn starttls(self) -> impl Future<Output = Result<Self>> + Send;
}

impl Transport for Conn {
    type Io = tp_platform::listen::Stream;

    fn io(&mut self) -> &mut Self::Io {
        &mut self.stream
    }

    fn tls(&self) -> bool {
        self.info.tls
    }

    fn can_starttls(&self) -> bool {
        Conn::can_starttls(self)
    }

    fn starttls(self) -> impl Future<Output = Result<Self>> + Send {
        Conn::starttls(self)
    }
}

/// Реализации контрактов, с которыми работает ManageSieve.
#[derive(Clone)]
pub struct Deps {
    /// Механизмы SASL (`AUTHENTICATE`); в приложении — с защитой от подбора.
    pub sasl: Arc<dyn SaslServer>,
    /// Скрипты учётных записей.
    pub store: Arc<dyn SieveStore>,
}

impl fmt::Debug for Deps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deps").finish_non_exhaustive()
    }
}

/// Общий контекст сеансов.
#[derive(Debug, Clone)]
pub struct SessionCtx {
    /// Секция `sieve` (лимиты проверки скриптов и `sieve.managesieve`).
    pub config: Arc<SieveConfig>,
    /// Контракты.
    pub deps: Deps,
    /// Профиль узла: при `dev` вход разрешён без TLS.
    pub profile: Profile,
    /// Остановка узла: сеансы завершаются с `BYE`.
    pub shutdown: CancellationToken,
}

/// Принимает соединения слушателя до остановки (`ctx.shutdown`), затем ждёт сеансы не дольше `drain` и прерывает
/// оставшиеся. Сверх `sieve.managesieve.max_connections` — `BYE (TRYLATER)` и закрытие.
pub async fn serve(ctx: Arc<SessionCtx>, listener: Listener, drain: Duration) {
    let mut sessions = JoinSet::new();
    loop {
        tokio::select! {
            _ = ctx.shutdown.cancelled() => break,
            Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
            accepted = listener.accept() => {
                let incoming = match accepted {
                    Ok(incoming) => incoming,
                    Err(e) => {
                        tracing::warn!(error = %e, "managesieve: ошибка приёма соединения");
                        continue;
                    }
                };
                let full = sessions.len() >= ctx.config.managesieve.max_connections;
                let ctx = ctx.clone();
                let task = async move {
                    let mut conn = match incoming.establish().await {
                        Ok(conn) => conn,
                        Err(e) => {
                            tracing::debug!(error = %e, "managesieve: соединение не установлено");
                            return;
                        }
                    };
                    if full {
                        let _ = conn
                            .stream
                            .write_all(b"BYE (TRYLATER) \"Too many connections\"\r\n")
                            .await;
                        let _ = conn.stream.shutdown().await;
                        return;
                    }
                    Session::new(ctx, conn).run().await;
                };
                if full {
                    tokio::spawn(task);
                } else {
                    sessions.spawn(task);
                }
            }
        }
    }
    if tokio::time::timeout(drain, async {
        while sessions.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        sessions.abort_all();
    }
}

/// Сервис ManageSieve: слушатели открывает приложение (`Listener::open` по `sieve.managesieve.listen`), секция роли —
/// [`SieveConfig`]. Без слушателей готов, но соединений не принимает.
pub struct ManageSieve {
    deps: Deps,
    profile: Profile,
    listeners: Mutex<Vec<Listener>>,
}

impl fmt::Debug for ManageSieve {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ManageSieve")
            .field("profile", &self.profile)
            .finish_non_exhaustive()
    }
}

impl ManageSieve {
    /// Сервис с контрактами `deps`, профилем узла и открытыми слушателями.
    pub fn new(deps: Deps, profile: Profile, listeners: Vec<Listener>) -> Self {
        Self {
            deps,
            profile,
            listeners: Mutex::new(listeners),
        }
    }
}

#[async_trait]
impl RoleService for ManageSieve {
    fn name(&self) -> &str {
        "managesieve"
    }

    async fn run(&self, ctx: RoleCtx) -> Result<()> {
        let config = ctx.config::<SieveConfig>()?;
        let listeners = std::mem::take(
            &mut *self
                .listeners
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        let session = Arc::new(SessionCtx {
            config,
            deps: self.deps.clone(),
            profile: self.profile,
            shutdown: ctx.shutdown.clone(),
        });
        let mut loops = JoinSet::new();
        for listener in listeners {
            loops.spawn(serve(session.clone(), listener, ctx.drain_deadline));
        }
        ctx.ready.set(true);
        ctx.shutdown.cancelled().await;
        while loops.join_next().await.is_some() {}
        Ok(())
    }
}
