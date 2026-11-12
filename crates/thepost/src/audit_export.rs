//! Экспорт журнала аудита Admin API во внешнюю SIEM (APP-59, SRS 5.2, 6.8): секция `admin.audit` — syslog
//! (RFC 5424 поверх TLS) и OTLP, приёмники собирает `tp_admin::AuditExportConfig::start`.
//!
//! Узел с ролью admin запускает экспорт при запуске ([`AuditExport::start`]: ошибка секции, секрета `tls.key` или
//! TLS — ошибка запуска) и применяет новую секцию `admin.audit` из перечитанной конфигурации без перезапуска
//! ([`AuditExport::watch`]): приёмники собираются заново, ошибка — прежние остаются в силе. Подключение к приёмнику
//! syslog — при первой записи и после обрыва (в фоне, `tp_admin::export`); недоступность приёмника показывает
//! `thepost doctor` (`admin.audit.syslog`, `admin.audit.otlp`).

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::watch;
use tp_admin::{AuditExportConfig, AuditSink, Fanout};
use tp_contracts::Result;
use tp_contracts::admin_audit::AdminAuditRecord;
use tp_contracts::role::CancellationToken;
use tp_contracts::secrets::SecretStore;
use tp_platform::Config;

use crate::sections::{self, AdminSection};

/// Действующие приёмники и секция, по которой они собраны.
#[derive(Default)]
struct Active {
    cfg: AuditExportConfig,
    fanout: Arc<Fanout>,
}

/// Экспорт журнала аудита узла: приёмники текущей секции `admin.audit` (без подсекций — без приёмников).
pub struct AuditExport {
    current: watch::Sender<Active>,
}

impl std::fmt::Debug for AuditExport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let current = self.current.borrow();
        f.debug_struct("AuditExport")
            .field("cfg", &current.cfg)
            .field("sinks", &current.fanout.0.len())
            .finish()
    }
}

/// Секция `admin.audit` версии конфигурации.
fn section(cfg: &Config) -> Result<AuditExportConfig> {
    Ok(cfg
        .section::<AdminSection>(sections::ADMIN)?
        .0
        .audit
        .clone())
}

/// Освобождает прежние приёмники вне async-контекста: остановка экспорта OTLP ждёт отправки накопленного
/// (блокирующий клиент HTTP); задача syslog завершается, отправив очередь.
fn release(old: Active) {
    drop(tokio::task::spawn_blocking(move || drop(old)));
}

impl AuditExport {
    /// Запускает экспорт по секции `admin.audit` версии `cfg`; ключ клиента syslog (`tls.key`) — из `secrets`.
    pub async fn start(cfg: &Config, secrets: &dyn SecretStore) -> Result<Arc<Self>> {
        let section = section(cfg)?;
        let fanout = Arc::new(Fanout(section.start(cfg.profile(), secrets).await?));
        if !fanout.0.is_empty() {
            tracing::info!(
                sinks = fanout.0.len(),
                "журнал аудита: экспорт в SIEM запущен"
            );
        }
        Ok(Arc::new(Self {
            current: watch::Sender::new(Active {
                cfg: section,
                fanout,
            }),
        }))
    }

    /// Применяет секцию `admin.audit` версии `cfg`: изменилась — приёмники собираются заново и заменяют прежние
    /// (`true`); та же — ничего (`false`); ошибка — прежние приёмники остаются.
    pub async fn apply(&self, cfg: &Config, secrets: &dyn SecretStore) -> Result<bool> {
        let section = section(cfg)?;
        if self.current.borrow().cfg == section {
            return Ok(false);
        }
        let fanout = Arc::new(Fanout(section.start(cfg.profile(), secrets).await?));
        let sinks = fanout.0.len();
        release(self.current.send_replace(Active {
            cfg: section,
            fanout,
        }));
        tracing::info!(sinks, "журнал аудита: применена новая секция admin.audit");
        Ok(true)
    }

    /// Применяет секцию `admin.audit` каждой новой версии конфигурации из `updates`, пока не `shutdown`; затем
    /// останавливает экспорт.
    pub async fn watch(
        self: Arc<Self>,
        mut updates: watch::Receiver<Arc<Config>>,
        secrets: Arc<dyn SecretStore>,
        shutdown: CancellationToken,
    ) {
        loop {
            tokio::select! {
                () = shutdown.cancelled() => break,
                changed = updates.changed() => if changed.is_err() { break },
            }
            let cfg = updates.borrow_and_update().clone();
            if let Err(e) = self.apply(&cfg, secrets.as_ref()).await {
                tracing::error!(error = %e, "журнал аудита: секция admin.audit не применена, экспорт — по прежней");
            }
        }
        release(self.current.send_replace(Active::default()));
    }
}

#[async_trait]
impl AuditSink for AuditExport {
    async fn record(&self, record: &AdminAuditRecord) -> Result<()> {
        let fanout = self.current.borrow().fanout.clone();
        fanout.record(record).await
    }
}
