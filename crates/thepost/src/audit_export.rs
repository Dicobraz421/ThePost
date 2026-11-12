//! Выгрузка журнала аудита Admin API из базы во внешнюю SIEM (APP-59, SRS 5.2, 6.8; решение владельца 12.11: база +
//! выгрузка из неё). Секция `admin.audit` — syslog (RFC 5424 поверх TLS) и OTLP, приёмники собирает
//! `tp_admin::AuditExportConfig::start`, проход — `tp_admin::export_pass`.
//!
//! Admin API пишет записи только в журнал (`audit_log`). Роль maintenance читает их от отметки доставки приёмника
//! (`audit_export`) и сдвигает её после доставки: приёмник недоступен — записи ждут в журнале и уходят по порядку,
//! когда он снова доступен. Выгружает один узел кластера — держатель лизы [`LEASE`]; остальные узлы с ролью
//! maintenance ждут. Новая секция `admin.audit` из перечитанной конфигурации применяется без перезапуска
//! (приёмники собираются заново; ошибка — прежние остаются в силе, повтор через `admin.audit.retry`). Доступность
//! приёмников показывает `thepost doctor` (`admin.audit.syslog`, `admin.audit.otlp`).

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use tp_admin::{AuditExportConfig, ExportSink, export_pass};
use tp_contracts::admin_audit::AuditLog;
use tp_contracts::lease::{Lease, LeaseStore};
use tp_contracts::role::CancellationToken;
use tp_contracts::secrets::SecretStore;
use tp_contracts::{Error, Result};
use tp_platform::Config;

use crate::sections::{self, AdminSection};

/// Лиза выгрузки: одна на кластер.
pub const LEASE: &str = "audit-export";

/// Срок лизы; продление — через треть срока.
const LEASE_TTL: Duration = Duration::from_secs(30);

/// Секция `admin.audit` версии конфигурации.
fn section(cfg: &Config) -> Result<AuditExportConfig> {
    Ok(cfg
        .section::<AdminSection>(sections::ADMIN)?
        .0
        .audit
        .clone())
}

/// Освобождает приёмники вне async-контекста: блокирующий клиент OTLP/HTTP не закрывается в задаче tokio.
fn release(sinks: Vec<Arc<dyn ExportSink>>) {
    if !sinks.is_empty() {
        drop(tokio::task::spawn_blocking(move || drop(sinks)));
    }
}

/// Действующие приёмники и секция, по которой они собраны.
#[derive(Default)]
struct Active {
    cfg: AuditExportConfig,
    sinks: Vec<Arc<dyn ExportSink>>,
    /// Секция, которую не удалось применить, и момент попытки.
    failed: Option<(AuditExportConfig, Instant)>,
}

/// Выгрузка журнала аудита роли maintenance.
pub struct AuditExport {
    log: Arc<dyn AuditLog>,
    leases: Arc<dyn LeaseStore>,
    holder: String,
    secrets: Arc<dyn SecretStore>,
    updates: watch::Receiver<Arc<Config>>,
}

impl std::fmt::Debug for AuditExport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuditExport")
            .field("holder", &self.holder)
            .finish_non_exhaustive()
    }
}

impl AuditExport {
    /// Выгрузка из `log` под лизой [`LEASE`] держателя `holder`; секция `admin.audit` — из версий `updates`, ключ
    /// клиента syslog — из `secrets`.
    pub fn new(
        log: Arc<dyn AuditLog>,
        leases: Arc<dyn LeaseStore>,
        holder: String,
        secrets: Arc<dyn SecretStore>,
        updates: watch::Receiver<Arc<Config>>,
    ) -> Self {
        Self {
            log,
            leases,
            holder,
            secrets,
            updates,
        }
    }

    /// Проходы выгрузки до остановки: новых записей нет — пауза `interval`, ошибка доставки — `retry`, полная
    /// пачка — следующий проход сразу. После остановки лиза отдаётся.
    pub async fn run(&self, shutdown: CancellationToken) {
        let mut updates = self.updates.clone();
        let mut open = true;
        let mut active = Active::default();
        let mut lease: Option<(Lease, Instant)> = None;
        loop {
            let cfg = updates.borrow_and_update().clone();
            self.apply(&cfg, &mut active).await;
            let wait = if active.sinks.is_empty() {
                self.release_lease(&mut lease).await;
                active.failed.as_ref().map(|_| active.cfg.retry)
            } else if !self.lead(&mut lease).await {
                Some(active.cfg.retry)
            } else {
                Some(self.pass(&active).await)
            };
            let watching = open;
            let changed = async {
                if watching {
                    updates.changed().await.is_ok()
                } else {
                    std::future::pending().await
                }
            };
            let sleep = async {
                match wait {
                    Some(d) => tokio::time::sleep(d).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                () = shutdown.cancelled() => break,
                ok = changed => open = ok,
                () = sleep => {}
            }
        }
        self.release_lease(&mut lease).await;
        release(std::mem::take(&mut active.sinks));
    }

    /// Проход по всем приёмникам; пауза до следующего.
    async fn pass(&self, active: &Active) -> Duration {
        let (mut busy, mut failed) = (false, false);
        for sink in &active.sinks {
            match export_pass(self.log.as_ref(), sink.as_ref(), active.cfg.batch).await {
                Ok(n) => busy |= n >= active.cfg.batch,
                Err(e) => {
                    failed = true;
                    // Недоступность приёмника уже записал он сам (`delivery failed`).
                    if !matches!(e, Error::Unavailable { .. }) {
                        tracing::warn!(sink = sink.name(), error = %e, "журнал аудита: проход выгрузки не удался");
                    }
                }
            }
        }
        if failed {
            active.cfg.retry
        } else if busy {
            Duration::ZERO
        } else {
            active.cfg.interval
        }
    }

    /// Применяет секцию `admin.audit` версии `cfg`: изменилась — приёмники собираются заново; ошибка — прежние
    /// остаются, та же секция повторяется не чаще `retry`.
    async fn apply(&self, cfg: &Config, active: &mut Active) {
        let section = match section(cfg) {
            Ok(s) => s,
            Err(e) => {
                tracing::error!(error = %e, "журнал аудита: секция admin.audit не прочитана");
                return;
            }
        };
        if section == active.cfg && active.failed.is_none() {
            return;
        }
        if let Some((failed, at)) = &active.failed
            && *failed == section
            && at.elapsed() < section.retry
        {
            return;
        }
        match section.start(cfg.profile(), self.secrets.as_ref()).await {
            Ok(sinks) => {
                tracing::info!(
                    sinks = sinks.len(),
                    "журнал аудита: выгрузка по секции admin.audit"
                );
                release(std::mem::replace(&mut active.sinks, sinks));
                active.cfg = section;
                active.failed = None;
            }
            Err(e) => {
                tracing::error!(error = %e, "журнал аудита: секция admin.audit не применена, выгрузка — по прежней");
                active.failed = Some((section, Instant::now()));
            }
        }
    }

    /// Держит лизу выгрузки: продлевает через треть срока, без лизы — пытается получить. `false` — выгружает
    /// другой узел или лиза недоступна.
    async fn lead(&self, lease: &mut Option<(Lease, Instant)>) -> bool {
        if let Some((held, renewed)) = lease.take() {
            if renewed.elapsed() < LEASE_TTL / 3 {
                *lease = Some((held, renewed));
                return true;
            }
            match self.leases.renew(&held, LEASE_TTL).await {
                Ok(Some(next)) => {
                    *lease = Some((next, Instant::now()));
                    return true;
                }
                Ok(None) => tracing::warn!(lease = LEASE, "журнал аудита: лиза выгрузки потеряна"),
                Err(e) => {
                    tracing::warn!(lease = LEASE, error = %e, "журнал аудита: лиза выгрузки не продлена");
                    return false;
                }
            }
        }
        match self
            .leases
            .try_acquire(LEASE, &self.holder, LEASE_TTL)
            .await
        {
            Ok(Some(next)) => {
                tracing::info!(lease = LEASE, "журнал аудита: выгрузку ведёт этот узел");
                *lease = Some((next, Instant::now()));
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(lease = LEASE, error = %e, "журнал аудита: лиза выгрузки не получена");
                false
            }
        }
    }

    async fn release_lease(&self, lease: &mut Option<(Lease, Instant)>) {
        if let Some((held, _)) = lease.take()
            && let Err(e) = self.leases.release(&held).await
        {
            tracing::warn!(lease = LEASE, error = %e, "журнал аудита: лиза выгрузки не отдана");
        }
    }
}
