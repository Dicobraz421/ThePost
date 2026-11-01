//! Платформа: конфигурация и hot reload, секреты (OpenBao/Vault), роли, телеметрия, health, слушатели TCP/TLS/PROXY v2,
//! rate limit, поведение под нагрузкой (SRS 2.1, 5.5, 6).

pub mod acme;
pub mod config;
pub mod listen;
pub mod load;
pub mod ratelimit;
pub mod reload;
pub mod roles;
pub mod secrets;
pub mod service_http;
pub mod telemetry;
pub mod tls;

pub use acme::{Acme, AcmeConfig, Http01Challenges};
pub use config::{CheckMode, Config, Section, SectionRegistry};
pub use listen::{Cidr, Conn, ConnInfo, EndpointConfig, Listener};
pub use load::{LoadConfig, LoadInputs, NodeLoad, background_allowed};
pub use ratelimit::{RateLimitConfig, RateLimiter, Verdict};
pub use reload::{ReloadConfig, ReloadTrigger, Reloader, section_watch};
pub use roles::{RoleLimits, RoleSpec, Roles, RolesReport, RuntimeConfig};
pub use secrets::{SecretsConfig, VaultStore};
pub use service_http::{Health, ServiceHttpConfig};
pub use telemetry::{BusSample, Metrics, Telemetry, TelemetryConfig};
pub use tls::{TlsCerts, TlsConfig};
