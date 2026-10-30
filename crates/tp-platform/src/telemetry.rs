//! Телеметрия (SRS 6.4): структурированные логи `tracing` (JSON), уровень из конфигурации с hot reload,
//! опциональный OTLP-экспорт трейсов, реестр метрик Prometheus (`prometheus-client`).

use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use opentelemetry::trace::TracerProvider as _;
use opentelemetry_otlp::WithExportConfig as _;
use opentelemetry_sdk::Resource;
use opentelemetry_sdk::trace::SdkTracerProvider;
use prometheus_client::collector::Collector;
use prometheus_client::encoding::{DescriptorEncoder, EncodeMetric};
use prometheus_client::metrics::counter::ConstCounter;
use prometheus_client::metrics::gauge::ConstGauge;
use prometheus_client::registry::{Metric, Registry};
use schemars::JsonSchema;
use serde::Deserialize;
use tp_contracts::Error;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;
use tracing_subscriber::{EnvFilter, reload};

/// Секция `telemetry`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct TelemetryConfig {
    /// Фильтр логов в синтаксисе `EnvFilter`: `info`, `warn,tp_smtp=debug`. Применяется без перезапуска.
    pub log_level: String,
    /// Формат логов. Требует перезапуска.
    pub log_format: LogFormat,
    /// OTLP-экспорт трейсов (OpenTelemetry); не задан — выключен. Требует перезапуска.
    pub otlp: Option<OtlpConfig>,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            log_level: "info".to_owned(),
            log_format: LogFormat::Json,
            otlp: None,
        }
    }
}

/// Формат логов.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    /// JSON, строка на событие.
    #[default]
    Json,
    /// Человекочитаемый текст (разработка).
    Text,
}

/// Параметры OTLP-экспорта.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct OtlpConfig {
    /// Адрес приёмника трейсов OTLP/HTTP (protobuf).
    pub endpoint: String,
    /// Атрибут `service.name`.
    pub service_name: String,
}

impl Default for OtlpConfig {
    fn default() -> Self {
        Self {
            endpoint: "http://localhost:4318/v1/traces".to_owned(),
            service_name: "thepost".to_owned(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum TelemetryError {
    #[error("telemetry.log_level: {0}")]
    Filter(#[from] tracing_subscriber::filter::ParseError),
    #[error("telemetry.otlp: {0}")]
    Otlp(#[from] opentelemetry_otlp::ExporterBuildError),
    #[error("tracing subscriber: {0}")]
    Init(#[from] tracing_subscriber::util::TryInitError),
    #[error("telemetry: {0}")]
    Reload(#[from] reload::Error),
    #[error("metrics encoding: {0}")]
    Encode(#[from] fmt::Error),
}

impl From<TelemetryError> for Error {
    fn from(e: TelemetryError) -> Self {
        match e {
            TelemetryError::Filter(_) | TelemetryError::Otlp(_) => Error::Invalid(e.to_string()),
            _ => Error::Internal(e.to_string()),
        }
    }
}

/// Реестр метрик узла. Клонируется; крейты регистрируют метрики при запуске, `/metrics` кодирует снимок.
#[derive(Clone, Default)]
pub struct Metrics(Arc<RwLock<Registry>>);

impl fmt::Debug for Metrics {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Metrics")
    }
}

impl Metrics {
    /// Регистрирует метрику (`Counter`, `Gauge`, `Histogram`, `Family<…>`); значение обновляет владелец клона.
    pub fn register(&self, name: &str, help: &str, metric: impl Metric) {
        self.0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .register(name, help, metric);
    }

    /// Регистрирует метрики шины уведомлений (решение #39, PLT-10): `snapshot` вызывается на каждый `/metrics`.
    /// Приложение переносит в [`BusSample`] снимок `tp_cluster::BusCounters::snapshot` (CLU-05) — платформа
    /// и tp-cluster не зависят друг от друга. Имена: `tp_bus_published_total`, `tp_bus_received_total`,
    /// `tp_bus_reconnects_total`, `tp_bus_decode_errors_total` и др., `tp_bus_connected` (0/1).
    pub fn register_bus(&self, snapshot: impl Fn() -> BusSample + Send + Sync + 'static) {
        self.0
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .register_collector(Box::new(BusCollector(Box::new(snapshot))));
    }

    /// Текстовый формат OpenMetrics/Prometheus.
    pub fn encode(&self) -> tp_contracts::Result<String> {
        let mut out = String::new();
        prometheus_client::encoding::text::encode(
            &mut out,
            &self.0.read().unwrap_or_else(PoisonError::into_inner),
        )
        .map_err(TelemetryError::from)?;
        Ok(out)
    }
}

/// Снимок счётчиков шины уведомлений; поля — как у `tp_cluster::BusMetrics` (CLU-05).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BusSample {
    /// Есть связь с шиной.
    pub connected: bool,
    /// Опубликовано уведомлений.
    pub published: u64,
    /// Неудачных публикаций.
    pub publish_errors: u64,
    /// Получено уведомлений из шины.
    pub received: u64,
    /// Выдано уведомлений подписчикам узла.
    pub delivered: u64,
    /// Получено нераспознанных сообщений.
    pub decode_errors: u64,
    /// Потерь связи (переподключений).
    pub reconnects: u64,
    /// Подписчиков, отключённых за отставание.
    pub lagged: u64,
}

/// Снимает [`BusSample`] при каждом кодировании реестра.
struct BusCollector(Box<dyn Fn() -> BusSample + Send + Sync>);

impl fmt::Debug for BusCollector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BusCollector")
    }
}

impl Collector for BusCollector {
    fn encode(&self, mut encoder: DescriptorEncoder) -> Result<(), fmt::Error> {
        let s = (self.0)();
        let counters = [
            (
                "tp_bus_published",
                "Bus notifications published",
                s.published,
            ),
            (
                "tp_bus_publish_errors",
                "Bus publish failures",
                s.publish_errors,
            ),
            ("tp_bus_received", "Bus notifications received", s.received),
            (
                "tp_bus_delivered",
                "Bus notifications delivered to local subscribers",
                s.delivered,
            ),
            (
                "tp_bus_decode_errors",
                "Unrecognized bus messages",
                s.decode_errors,
            ),
            (
                "tp_bus_reconnects",
                "Bus connection losses (reconnects)",
                s.reconnects,
            ),
            ("tp_bus_lagged", "Subscribers dropped for lagging", s.lagged),
        ];
        for (name, help, value) in counters {
            let counter = ConstCounter::new(value);
            let metric = encoder.encode_descriptor(name, help, None, counter.metric_type())?;
            counter.encode(metric)?;
        }
        let gauge = ConstGauge::new(i64::from(s.connected));
        let metric = encoder.encode_descriptor(
            "tp_bus_connected",
            "Bus connection is up (1) or down (0)",
            None,
            gauge.metric_type(),
        )?;
        gauge.encode(metric)
    }
}

/// Установленная телеметрия процесса: смена уровня логов, остановка экспорта.
pub struct Telemetry {
    metrics: Metrics,
    filter: reload::Handle<EnvFilter, tracing_subscriber::Registry>,
    tracer: Option<SdkTracerProvider>,
}

impl fmt::Debug for Telemetry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Telemetry")
            .field("otlp", &self.tracer.is_some())
            .finish_non_exhaustive()
    }
}

impl Telemetry {
    /// Устанавливает глобальный подписчик `tracing`. Один раз на процесс; повтор — `Error::Internal`.
    pub fn init(cfg: &TelemetryConfig) -> tp_contracts::Result<Self> {
        Ok(Self::try_init(cfg)?)
    }

    fn try_init(cfg: &TelemetryConfig) -> Result<Self, TelemetryError> {
        let (filter, handle) = reload::Layer::new(EnvFilter::try_new(&cfg.log_level)?);
        let tracer = match &cfg.otlp {
            Some(otlp) => {
                let exporter = opentelemetry_otlp::SpanExporter::builder()
                    .with_http()
                    .with_endpoint(&otlp.endpoint)
                    .build()?;
                Some(
                    SdkTracerProvider::builder()
                        .with_batch_exporter(exporter)
                        .with_resource(
                            Resource::builder()
                                .with_service_name(otlp.service_name.clone())
                                .build(),
                        )
                        .build(),
                )
            }
            None => None,
        };
        let json = cfg.log_format == LogFormat::Json;
        tracing_subscriber::registry()
            .with(filter)
            .with(json.then(|| tracing_subscriber::fmt::layer().json()))
            .with((!json).then(tracing_subscriber::fmt::layer))
            .with(
                tracer
                    .as_ref()
                    .map(|p| tracing_opentelemetry::layer().with_tracer(p.tracer("thepost"))),
            )
            .try_init()?;
        Ok(Self {
            metrics: Metrics::default(),
            filter: handle,
            tracer,
        })
    }

    /// Реестр метрик процесса.
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Применяет новую версию секции (hot reload): уровень логов. Формат и OTLP — после перезапуска.
    pub fn apply(&self, cfg: &TelemetryConfig) -> tp_contracts::Result<()> {
        let filter = EnvFilter::try_new(&cfg.log_level).map_err(TelemetryError::from)?;
        self.filter.reload(filter).map_err(TelemetryError::from)?;
        Ok(())
    }

    /// Отправляет накопленные трейсы и останавливает экспорт.
    pub fn shutdown(self) {
        if let Some(p) = self.tracer
            && let Err(e) = p.shutdown()
        {
            tracing::warn!(error = %e, "otlp shutdown failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use prometheus_client::metrics::counter::Counter;
    use prometheus_client::metrics::gauge::Gauge;

    use super::*;

    #[test]
    fn metric_registers_and_encodes_as_text() {
        let metrics = Metrics::default();
        let requests = Counter::<u64>::default();
        let conns = Gauge::<i64>::default();
        metrics.register("tp_requests", "Processed requests", requests.clone());
        metrics
            .clone()
            .register("tp_connections", "Open connections", conns.clone());
        requests.inc_by(3);
        conns.set(2);
        let text = metrics.encode().expect("encode");
        assert!(
            text.contains("# HELP tp_requests Processed requests."),
            "{text}"
        );
        assert!(text.contains("# TYPE tp_requests counter"), "{text}");
        assert!(text.contains("tp_requests_total 3"), "{text}");
        assert!(text.contains("tp_connections 2"), "{text}");
        assert!(text.ends_with("# EOF\n"), "{text}");
    }

    #[test]
    fn bus_counters_in_shared_registry() {
        let metrics = Metrics::default();
        let sample = Arc::new(RwLock::new(BusSample::default()));
        let source = sample.clone();
        metrics.register_bus(move || *source.read().unwrap_or_else(PoisonError::into_inner));
        let text = metrics.encode().expect("encode");
        assert!(text.contains("tp_bus_connected 0"), "{text}");
        assert!(text.contains("tp_bus_published_total 0"), "{text}");

        *sample.write().unwrap_or_else(PoisonError::into_inner) = BusSample {
            connected: true,
            published: 5,
            publish_errors: 1,
            received: 4,
            delivered: 8,
            decode_errors: 2,
            reconnects: 3,
            lagged: 6,
        };
        let text = metrics.encode().expect("encode");
        for line in [
            "tp_bus_connected 1",
            "tp_bus_published_total 5",
            "tp_bus_publish_errors_total 1",
            "tp_bus_received_total 4",
            "tp_bus_delivered_total 8",
            "tp_bus_decode_errors_total 2",
            "tp_bus_reconnects_total 3",
            "tp_bus_lagged_total 6",
            "# TYPE tp_bus_reconnects counter",
            "# TYPE tp_bus_connected gauge",
        ] {
            assert!(text.contains(line), "{line}: {text}");
        }
    }

    #[test]
    fn config_defaults_and_invalid_level() {
        let cfg: TelemetryConfig =
            serde_yaml_ng::from_str("log_format: text\notlp: {}\n").expect("parse");
        assert_eq!(cfg.log_level, "info");
        assert_eq!(cfg.log_format, LogFormat::Text);
        assert_eq!(cfg.otlp, Some(OtlpConfig::default()));
        let bad = TelemetryConfig {
            log_level: "tp_smtp=loud".to_owned(),
            ..TelemetryConfig::default()
        };
        assert!(matches!(Telemetry::init(&bad), Err(Error::Invalid(_))));
    }
}
