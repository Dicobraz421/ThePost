//! Политики доменов назначения для smtp-out (SRS 4.1, модель коннекторов отправки Exchange; CONTRACTS.md#Transport).
//!
//! [`Routes`] держит снимки настроек транспорта: политики организаций ([`TenantTransport::routes`], шаблон `*` —
//! все внешние домены, relayhost) и принятые домены кластера ([`ClusterTransport`]: они — внутренняя доставка,
//! политики к ним не применяются). Снимок читается из [`TransportStore`] при первом обращении и перечитывается
//! после сигнала [`EventKind::TransportChanged`] (организации или кластера) и [`EventKind::Resync`]
//! ([`Routes::watch`]). Перечитать не удалось — действует прежний снимок; снимка нет — ошибка (доставка
//! откладывается: политика может требовать TLS).
//!
//! Таблица переписывания адресов конверта организации ([`TenantTransport::rewrites`], SMTP-29) — из того же
//! снимка: её применяет доставка ([`crate::Delivery::with_transport`]) ролей smtp-in и submission.
//!
//! Применение политики — [`crate::queue::send::MxTransport`]: обязательный TLS, закреплённый ключ, промежуточный
//! узел (smart host), egress-IP; с MTA-STS и DANE действуют требования обоих (строже из двух).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::StreamExt;
use tp_contracts::bus::{EventBus, EventKind};
use tp_contracts::role::CancellationToken;
use tp_contracts::transport::{ClusterTransport, TenantTransport, TransportStore};
use tp_contracts::{Result, TenantId};

#[derive(Debug)]
struct Snap<T> {
    value: Arc<T>,
    /// Пришёл сигнал изменения: перечитать при следующем обращении.
    stale: bool,
}

#[derive(Debug, Default)]
struct State {
    cluster: Option<Snap<ClusterTransport>>,
    tenants: HashMap<TenantId, Snap<TenantTransport>>,
}

/// Снимки настроек транспорта узла (smtp-out — политики, smtp-in и submission — переписывание адресов).
pub struct Routes {
    store: Arc<dyn TransportStore>,
    state: Mutex<State>,
}

impl fmt::Debug for Routes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Routes").finish_non_exhaustive()
    }
}

impl Routes {
    /// Снимки из хранилища `store`.
    pub fn new(store: Arc<dyn TransportStore>) -> Self {
        Self {
            store,
            state: Mutex::default(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Политики организации `tenant`.
    pub async fn tenant(&self, tenant: TenantId) -> Result<Arc<TenantTransport>> {
        let cached = self
            .lock()
            .tenants
            .get(&tenant)
            .map(|s| (s.value.clone(), s.stale));
        if let Some((value, false)) = &cached {
            return Ok(value.clone());
        }
        match self.store.tenant(tenant).await {
            Ok(t) => {
                let value = Arc::new(t);
                self.lock().tenants.insert(
                    tenant,
                    Snap {
                        value: value.clone(),
                        stale: false,
                    },
                );
                Ok(value)
            }
            Err(e) => match cached {
                Some((value, _)) => {
                    tracing::warn!(%tenant, error = %e, "smtp: политики доменов не перечитаны, действует прежний снимок");
                    Ok(value)
                }
                None => Err(e),
            },
        }
    }

    /// Снимок уровня кластера (принятые домены).
    pub async fn cluster(&self) -> Result<Arc<ClusterTransport>> {
        let cached = self
            .lock()
            .cluster
            .as_ref()
            .map(|s| (s.value.clone(), s.stale));
        if let Some((value, false)) = &cached {
            return Ok(value.clone());
        }
        match self.store.cluster().await {
            Ok(c) => {
                let value = Arc::new(c);
                self.lock().cluster = Some(Snap {
                    value: value.clone(),
                    stale: false,
                });
                Ok(value)
            }
            Err(e) => match cached {
                Some((value, _)) => {
                    tracing::warn!(error = %e, "smtp: принятые домены не перечитаны, действует прежний снимок");
                    Ok(value)
                }
                None => Err(e),
            },
        }
    }

    /// Сигнал изменения: `None` — уровень кластера, иначе — организация.
    pub fn changed(&self, tenant: Option<TenantId>) {
        let mut st = self.lock();
        match tenant {
            None => {
                if let Some(s) = &mut st.cluster {
                    s.stale = true;
                }
            }
            Some(t) => {
                if let Some(s) = st.tenants.get_mut(&t) {
                    s.stale = true;
                }
            }
        }
    }

    /// Сигналы потеряны: перечитать всё.
    pub fn resync(&self) {
        let mut st = self.lock();
        if let Some(s) = &mut st.cluster {
            s.stale = true;
        }
        for s in st.tenants.values_mut() {
            s.stale = true;
        }
    }

    /// Следит за сигналами шины до `shutdown`; поток завершился или подписка не удалась — повтор через `retry`
    /// со сверкой (перечитать всё).
    pub async fn watch(
        self: Arc<Self>,
        bus: Arc<dyn EventBus>,
        shutdown: CancellationToken,
        retry: Duration,
    ) {
        while !shutdown.is_cancelled() {
            match bus.subscribe_system().await {
                Ok(mut events) => {
                    // Сигналы до подписки могли потеряться.
                    self.resync();
                    loop {
                        let ev = tokio::select! {
                            () = shutdown.cancelled() => return,
                            ev = events.next() => ev,
                        };
                        match ev.map(|e| e.kind) {
                            Some(EventKind::TransportChanged { tenant }) => self.changed(tenant),
                            Some(EventKind::Resync) => self.resync(),
                            Some(_) => {}
                            None => break,
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "smtp: подписка на сигналы настроек транспорта не удалась");
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(retry) => {}
            }
        }
    }
}
