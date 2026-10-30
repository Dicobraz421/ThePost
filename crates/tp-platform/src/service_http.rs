//! Служебный HTTP узла (SRS 6.1, 6.4): `/metrics`, `/healthz/live`, `/healthz/ready`.
//!
//! Готовность узла — все зарегистрированные роли готовы ([`ReadyHandle`]) и узел не выводится из пула (drain);
//! по `/healthz/ready` балансировщик включает и исключает узел.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use axum::Router;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use schemars::JsonSchema;
use serde::Deserialize;
use tokio::net::TcpListener;
use tp_contracts::Error;
use tp_contracts::role::{ReadyHandle, Shutdown};

use crate::telemetry::Metrics;

/// Секция `service_http`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ServiceHttpConfig {
    /// Адрес служебного HTTP (метрики и проверки доступности). Требует перезапуска.
    pub listen: SocketAddr,
}

impl Default for ServiceHttpConfig {
    fn default() -> Self {
        Self {
            listen: SocketAddr::from(([0, 0, 0, 0], 8081)),
        }
    }
}

/// Состояние готовности узла. Клонируется; роли получают [`ReadyHandle`] через [`Health::register`].
#[derive(Debug, Clone, Default)]
pub struct Health(Arc<HealthInner>);

#[derive(Debug, Default)]
struct HealthInner {
    roles: Mutex<BTreeMap<String, ReadyHandle>>,
    draining: AtomicBool,
}

impl Health {
    /// Регистрирует роль; признак её готовности изначально `false`. Повторная регистрация возвращает тот же признак.
    pub fn register(&self, role: &str) -> ReadyHandle {
        self.roles().entry(role.to_owned()).or_default().clone()
    }

    /// Выводит узел из пула: `/healthz/ready` отвечает 503 до конца работы процесса.
    pub fn drain(&self) {
        self.0.draining.store(true, Ordering::Release);
    }

    /// Узел выводится из пула.
    pub fn is_draining(&self) -> bool {
        self.0.draining.load(Ordering::Acquire)
    }

    /// Узел готов: не drain, есть роли, все роли готовы.
    pub fn is_ready(&self) -> bool {
        let roles = self.roles();
        !self.is_draining() && !roles.is_empty() && roles.values().all(ReadyHandle::is_ready)
    }

    /// Готовность по ролям.
    pub fn roles_status(&self) -> Vec<(String, bool)> {
        self.roles()
            .iter()
            .map(|(name, h)| (name.clone(), h.is_ready()))
            .collect()
    }

    fn roles(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, ReadyHandle>> {
        self.0.roles.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Debug, Clone)]
struct AppState {
    health: Health,
    metrics: Metrics,
}

/// Маршруты служебного HTTP.
pub fn router(health: Health, metrics: Metrics) -> Router {
    Router::new()
        .route("/metrics", get(metrics_handler))
        .route("/healthz/live", get(|| async { "live\n" }))
        .route("/healthz/ready", get(ready_handler))
        .with_state(AppState { health, metrics })
}

/// Слушающий сокет для `cfg.listen`. Требует контекста tokio-рантайма.
pub fn bind(cfg: &ServiceHttpConfig) -> tp_contracts::Result<TcpListener> {
    tp_sys::bind_tcp(cfg.listen, &tp_sys::ListenerConfig::default())
        .map_err(|e| Error::Invalid(format!("service_http.listen {}: {e}", cfg.listen)))
}

/// Обслуживает служебный HTTP до сигнала остановки.
pub async fn serve(
    listener: TcpListener,
    router: Router,
    shutdown: Shutdown,
) -> tp_contracts::Result<()> {
    axum::serve(listener, router)
        .with_graceful_shutdown(async move { shutdown.triggered().await })
        .await
        .map_err(|e| Error::Internal(format!("service http: {e}")))
}

async fn metrics_handler(State(s): State<AppState>) -> Response {
    match s.metrics.encode() {
        Ok(text) => (
            [(
                header::CONTENT_TYPE,
                "application/openmetrics-text; version=1.0.0; charset=utf-8",
            )],
            text,
        )
            .into_response(),
        Err(e) => {
            tracing::error!(error = %e, "metrics encoding failed");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn ready_handler(State(s): State<AppState>) -> Response {
    let mut body = String::new();
    if s.health.is_draining() {
        body.push_str("draining\n");
    }
    for (role, ready) in s.health.roles_status() {
        body.push_str(&role);
        body.push_str(if ready { " ready\n" } else { " not-ready\n" });
    }
    let status = if s.health.is_ready() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, body).into_response()
}

#[cfg(test)]
mod tests {
    use prometheus_client::metrics::counter::Counter;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::*;

    async fn get(addr: SocketAddr, path: &str) -> (u16, String) {
        let mut s = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let req = format!("GET {path} HTTP/1.1\r\nHost: t\r\nConnection: close\r\n\r\n");
        s.write_all(req.as_bytes()).await.expect("write");
        let mut resp = String::new();
        s.read_to_string(&mut resp).await.expect("read");
        let code = resp
            .split(' ')
            .nth(1)
            .and_then(|c| c.parse().ok())
            .expect("status");
        let body = resp.split("\r\n\r\n").nth(1).unwrap_or_default().to_owned();
        (code, body)
    }

    #[test]
    fn ready_follows_roles_and_drain() {
        let health = Health::default();
        assert!(!health.is_ready(), "no roles yet");
        let imap = health.register("imap");
        let smtp = health.register("smtp-in");
        assert!(!health.is_ready());
        imap.set(true);
        assert!(!health.is_ready());
        smtp.set(true);
        assert!(health.is_ready());
        health.register("imap").set(false);
        assert!(!health.is_ready(), "same handle on re-register");
        imap.set(true);
        assert!(health.is_ready());
        health.drain();
        assert!(!health.is_ready());
        assert_eq!(
            health.roles_status(),
            [("imap".to_owned(), true), ("smtp-in".to_owned(), true)]
        );
    }

    #[tokio::test]
    async fn http_endpoints() {
        let health = Health::default();
        let metrics = Metrics::default();
        let hits = Counter::<u64>::default();
        metrics.register("tp_hits", "Hits", hits.clone());
        hits.inc();

        let cfg = ServiceHttpConfig {
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
        };
        let listener = bind(&cfg).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (shutdown, trigger) = Shutdown::new();
        let server = tokio::spawn(serve(listener, router(health.clone(), metrics), shutdown));

        assert_eq!(get(addr, "/healthz/live").await, (200, "live\n".to_owned()));
        let role = health.register("jmap");
        assert_eq!(
            get(addr, "/healthz/ready").await,
            (503, "jmap not-ready\n".to_owned())
        );
        role.set(true);
        assert_eq!(
            get(addr, "/healthz/ready").await,
            (200, "jmap ready\n".to_owned())
        );
        health.drain();
        assert_eq!(
            get(addr, "/healthz/ready").await,
            (503, "draining\njmap ready\n".to_owned())
        );
        assert_eq!(get(addr, "/healthz/live").await.0, 200);
        let (code, body) = get(addr, "/metrics").await;
        assert_eq!(code, 200);
        assert!(body.contains("tp_hits_total 1"), "{body}");

        trigger.trigger();
        server.await.expect("join").expect("serve");
    }
}
