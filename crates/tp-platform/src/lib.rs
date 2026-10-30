//! Платформа: конфигурация и hot reload, роли, телеметрия, health, слушатели TCP/TLS/PROXY v2, rate limit (SRS 2.1, 5.5, 6).

pub mod acme;
pub mod config;
pub mod listen;
pub mod ratelimit;
pub mod reload;
pub mod roles;
pub mod service_http;
pub mod telemetry;
pub mod tls;

pub use acme::{Acme, AcmeConfig, Http01Challenges};
pub use config::{Config, Section, SectionRegistry};
pub use listen::{Cidr, Conn, ConnInfo, EndpointConfig, Listener};
pub use ratelimit::{RateLimitConfig, RateLimiter, Verdict};
pub use reload::{ReloadConfig, ReloadTrigger, Reloader, section_watch};
pub use roles::{RoleLimits, RoleSpec, Roles, RolesReport, RuntimeConfig};
pub use service_http::{Health, ServiceHttpConfig};
pub use telemetry::{BusSample, Metrics, Telemetry, TelemetryConfig};
pub use tls::{TlsCerts, TlsConfig};
