//! Роли smtp-in и submission (SRS 2.1, 4.1): слушатели из секции `smtp`, лимит одновременных соединений,
//! ограничение частоты по адресу клиента, остановка по `shutdown` с ожиданием сессий до `drain_deadline`.
//!
//! Submission (587 — STARTTLS, 465 — неявный TLS, `implicit_tls: true`) без сертификатов не запускается:
//! AUTH принимается только по TLS. smtp-in (25) предлагает STARTTLS, если сертификаты заданы.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use tokio::task::JoinSet;
use tp_contracts::role::{CancellationToken, ConfigRx, RoleCtx, RoleService};
use tp_contracts::sasl::SaslServer;
use tp_contracts::{Error, Result};
use tp_platform::listen::Incoming;
use tp_platform::{Listener, RateLimiter, TlsCerts};

use super::session::push;
use super::tls::serve_conn;
use super::{Handler, Mode, SessionCtx, reply};
use crate::config::SmtpConfig;

/// Зависимости ролей SMTP, выбранные приложением.
#[derive(Clone)]
pub struct ServerDeps {
    /// Обработчик транзакций (локальная доставка — [`crate::deliver::Delivery`]).
    pub handler: Arc<dyn Handler>,
    /// Механизмы SASL (submission); `None` — AUTH не предлагается.
    pub sasl: Option<Arc<dyn SaslServer>>,
    /// Ограничитель частоты узла (соединения, попытки AUTH, отправка).
    pub limiter: Option<Arc<RateLimiter>>,
    /// Сертификаты для STARTTLS и неявного TLS.
    pub tls: Option<Arc<TlsCerts>>,
}

impl fmt::Debug for ServerDeps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ServerDeps")
            .field("sasl", &self.sasl.is_some())
            .field("tls", &self.tls.is_some())
            .finish_non_exhaustive()
    }
}

/// Сервис роли smtp-in или submission.
#[derive(Debug, Clone)]
pub struct SmtpService {
    mode: Mode,
    deps: ServerDeps,
}

impl SmtpService {
    /// Сервис роли `mode`.
    pub fn new(mode: Mode, deps: ServerDeps) -> Self {
        Self { mode, deps }
    }

    /// Обслуживает готовые слушатели до `ctx.shutdown` (приложение — из [`RoleService::run`], тесты — на `127.0.0.1:0`).
    pub async fn serve(&self, listeners: Vec<Listener>, ctx: RoleCtx) -> Result<()> {
        let mut loops = JoinSet::new();
        for listener in listeners {
            loops.spawn(accept_loop(
                listener,
                self.mode,
                self.deps.clone(),
                ctx.config.clone(),
                ctx.shutdown.clone(),
                ctx.drain_deadline,
            ));
        }
        ctx.ready.set(true);
        while loops.join_next().await.is_some() {}
        ctx.ready.set(false);
        Ok(())
    }
}

#[async_trait]
impl RoleService for SmtpService {
    fn name(&self) -> &str {
        self.mode.name()
    }

    async fn run(&self, ctx: RoleCtx) -> Result<()> {
        let cfg = ctx.config::<SmtpConfig>()?;
        let endpoints = match self.mode {
            Mode::Inbound => &cfg.inbound,
            Mode::Submission => &cfg.submission,
        };
        if self.mode == Mode::Submission && self.deps.tls.is_none() && !endpoints.is_empty() {
            return Err(Error::invalid(
                "smtp.submission: TLS certificates are required (AUTH only over TLS)",
            ));
        }
        let listeners = endpoints
            .iter()
            .map(|ep| Listener::bind(ep, self.deps.tls.clone()))
            .collect::<Result<Vec<_>>>()?;
        self.serve(listeners, ctx).await
    }
}

/// Текущая секция `smtp` (последняя версия hot reload).
fn current(rx: &ConfigRx) -> Option<Arc<SmtpConfig>> {
    rx.borrow().clone().downcast::<SmtpConfig>().ok()
}

async fn accept_loop(
    listener: Listener,
    mode: Mode,
    deps: ServerDeps,
    config: ConfigRx,
    shutdown: CancellationToken,
    drain: Duration,
) {
    let mut sessions = JoinSet::new();
    let mut last = current(&config).unwrap_or_default();
    loop {
        tokio::select! {
            () = shutdown.cancelled() => break,
            Some(_) = sessions.join_next(), if !sessions.is_empty() => {}
            accepted = listener.accept() => match accepted {
                Ok(incoming) => {
                    if let Some(cfg) = current(&config) {
                        last = cfg;
                    }
                    let busy = sessions.len() >= last.limits.max_connections;
                    sessions.spawn(connection(incoming, mode, deps.clone(), last.clone(), busy));
                }
                Err(e) => {
                    // Нехватка дескрипторов и т. п.: не крутить цикл вхолостую.
                    tracing::warn!(error = %e, role = mode.name(), "smtp: ошибка приёма соединения");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            },
        }
    }
    drop(listener);
    let drained = tokio::time::timeout(drain, async {
        while sessions.join_next().await.is_some() {}
    })
    .await;
    if drained.is_err() {
        tracing::info!(
            role = mode.name(),
            left = sessions.len(),
            "smtp: срок завершения истёк, сессии закрываются"
        );
        sessions.abort_all();
    }
}

async fn connection(
    incoming: Incoming,
    mode: Mode,
    deps: ServerDeps,
    config: Arc<SmtpConfig>,
    busy: bool,
) {
    let peer = incoming.peer();
    let mut conn = match incoming.establish().await {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(%peer, error = %e, "smtp: соединение не установлено");
            return;
        }
    };
    let refuse = if busy {
        Some(reply(421, [4, 3, 2], "Too many connections, try later"))
    } else {
        deps.limiter
            .as_ref()
            .filter(|l| !l.check_ip(conn.info.client.ip()).is_allowed())
            .map(|_| reply(421, [4, 7, 0], "Too many connections from your address"))
    };
    if let Some(r) = refuse {
        let mut out = Vec::new();
        push(&mut out, &r);
        let _ = conn.stream.write_all(&out).await;
        let _ = conn.stream.shutdown().await;
        return;
    }
    let ctx = SessionCtx {
        mode,
        config,
        handler: deps.handler,
        sasl: deps.sasl,
        limiter: deps.limiter,
    };
    serve_conn(ctx, conn).await;
}
