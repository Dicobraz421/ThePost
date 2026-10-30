//! Платформа: конфигурация и hot reload, роли, телеметрия, health, слушатели TCP/TLS/PROXY v2, rate limit (SRS 2.1, 5.5, 6).

pub mod config;
pub mod reload;
pub mod service_http;
pub mod telemetry;

pub use config::{Config, Section, SectionRegistry};
pub use reload::{ReloadConfig, ReloadTrigger, Reloader, SectionWatch};
pub use service_http::{Health, ServiceHttpConfig};
pub use telemetry::{Metrics, Telemetry, TelemetryConfig};
