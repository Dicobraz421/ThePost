//! In-memory настройки транспорта (CONTRACTS.md#Transport; CTR-24).
//!
//! [`MemTransport`] — снимок кластера и снимки организаций с версиями; с шиной ([`MemTransport::with_bus`])
//! запись публикует `TransportChanged`, как реализация `tp-meta`.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tp_contracts::bus::EventBus;
use tp_contracts::transport::{
    AcceptedDomain, ClusterTransport, DestinationPolicy, TenantTransport, TransportStore,
    TrustedSource, transport_changed,
};
use tp_contracts::{DomainId, Error, Result, TenantId};

use crate::infra::lock;

#[derive(Debug, Default)]
struct Routes {
    version: u64,
    routes: BTreeMap<String, DestinationPolicy>,
}

#[derive(Debug, Default)]
struct State {
    version: u64,
    domains: HashMap<DomainId, AcceptedDomain>,
    sources: BTreeMap<String, TrustedSource>,
    tenants: HashMap<TenantId, Routes>,
    faults: VecDeque<Error>,
}

/// Настройки транспорта в памяти; клоны разделяют состояние.
#[derive(Clone, Default)]
pub struct MemTransport {
    state: Arc<Mutex<State>>,
    bus: Option<Arc<dyn EventBus>>,
}

impl fmt::Debug for MemTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MemTransport")
            .field("state", &self.state)
            .field("bus", &self.bus.is_some())
            .finish()
    }
}

impl MemTransport {
    /// Пустые настройки (версии 0).
    pub fn new() -> Self {
        Self::default()
    }

    /// Публиковать `TransportChanged` в `bus` после записи.
    pub fn with_bus(mut self, bus: Arc<dyn EventBus>) -> Self {
        self.bus = Some(bus);
        self
    }

    /// Следующая операция вернёт `err` (очередь, по одной на операцию).
    pub fn fail_next(&self, err: Error) {
        lock(&self.state).faults.push_back(err);
    }

    async fn changed(&self, tenant: Option<TenantId>) {
        if let Some(bus) = &self.bus {
            // Сигнал после «фиксации»; потеря сигнала не отменяет изменение.
            let _ = bus.publish(transport_changed(tenant)).await;
        }
    }

    /// Изменение уровня кластера под проверкой версии; `f` — проверка ключа и запись.
    async fn cluster_write(
        &self,
        expected: u64,
        f: impl FnOnce(&mut State) -> Result<()>,
    ) -> Result<u64> {
        let version = {
            let mut st = lock(&self.state);
            fault(&mut st)?;
            if st.version != expected {
                return Err(Error::Conflict);
            }
            f(&mut st)?;
            st.version += 1;
            st.version
        };
        self.changed(None).await;
        Ok(version)
    }

    /// Изменение политик организации под проверкой версии.
    async fn tenant_write(
        &self,
        tenant: TenantId,
        expected: u64,
        f: impl FnOnce(&mut BTreeMap<String, DestinationPolicy>) -> Result<()>,
    ) -> Result<u64> {
        let version = {
            let mut st = lock(&self.state);
            fault(&mut st)?;
            let t = st.tenants.entry(tenant).or_default();
            if t.version != expected {
                return Err(Error::Conflict);
            }
            f(&mut t.routes)?;
            t.version += 1;
            t.version
        };
        self.changed(Some(tenant)).await;
        Ok(version)
    }
}

fn fault(st: &mut State) -> Result<()> {
    st.faults.pop_front().map_or(Ok(()), Err)
}

#[async_trait]
impl TransportStore for MemTransport {
    async fn cluster(&self) -> Result<ClusterTransport> {
        let mut st = lock(&self.state);
        fault(&mut st)?;
        let mut domains: Vec<_> = st.domains.values().cloned().collect();
        domains.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(ClusterTransport {
            version: st.version,
            domains,
            sources: st.sources.values().cloned().collect(),
        })
    }

    async fn tenant(&self, tenant: TenantId) -> Result<TenantTransport> {
        let mut st = lock(&self.state);
        fault(&mut st)?;
        Ok(st
            .tenants
            .get(&tenant)
            .map(|t| TenantTransport {
                version: t.version,
                routes: t.routes.values().cloned().collect(),
            })
            .unwrap_or_default())
    }

    async fn put_domain(&self, expected: u64, domain: &AcceptedDomain) -> Result<u64> {
        domain.validate()?;
        self.cluster_write(expected, |st| {
            if st
                .domains
                .values()
                .any(|d| d.name == domain.name && d.id != domain.id)
            {
                return Err(Error::Conflict);
            }
            if st
                .domains
                .get(&domain.id)
                .is_some_and(|d| d.tenant != domain.tenant)
            {
                return Err(Error::invalid("domain tenant cannot change"));
            }
            st.domains.insert(domain.id, domain.clone());
            Ok(())
        })
        .await
    }

    async fn delete_domain(&self, expected: u64, id: DomainId) -> Result<u64> {
        self.cluster_write(expected, |st| {
            st.domains.remove(&id).map(|_| ()).ok_or(Error::NotFound)
        })
        .await
    }

    async fn put_source(&self, expected: u64, source: &TrustedSource) -> Result<u64> {
        source.validate()?;
        self.cluster_write(expected, |st| {
            if st
                .sources
                .get(&source.name)
                .is_some_and(|s| s.tenant != source.tenant)
            {
                return Err(Error::invalid("source tenant cannot change"));
            }
            st.sources.insert(source.name.clone(), source.clone());
            Ok(())
        })
        .await
    }

    async fn delete_source(&self, expected: u64, name: &str) -> Result<u64> {
        self.cluster_write(expected, |st| {
            st.sources.remove(name).map(|_| ()).ok_or(Error::NotFound)
        })
        .await
    }

    async fn put_route(
        &self,
        tenant: TenantId,
        expected: u64,
        policy: &DestinationPolicy,
    ) -> Result<u64> {
        policy.validate()?;
        self.tenant_write(tenant, expected, |routes| {
            routes.insert(policy.pattern.clone(), policy.clone());
            Ok(())
        })
        .await
    }

    async fn delete_route(&self, tenant: TenantId, expected: u64, pattern: &str) -> Result<u64> {
        self.tenant_write(tenant, expected, |routes| {
            routes.remove(pattern).map(|_| ()).ok_or(Error::NotFound)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use tp_contracts::bus::{Event, EventKind};
    use tp_contracts::transport::{DomainKind, TlsRequirement};

    use super::*;
    use crate::{MemEventBus, suite};

    #[tokio::test]
    async fn passes_contract_suite() {
        suite::transport_store(&MemTransport::new()).await;
    }

    #[tokio::test]
    async fn publishes_changes_and_faults() {
        let bus = MemEventBus::default();
        let store = MemTransport::new().with_bus(Arc::new(bus.clone()));
        let mut sys = bus.subscribe_system().await.expect("subscribe");
        let tenant = TenantId::new();
        let domain = AcceptedDomain {
            id: DomainId::new(),
            tenant,
            name: "example.org".into(),
            kind: DomainKind::Authoritative,
        };
        assert_eq!(store.put_domain(0, &domain).await, Ok(1));
        assert_eq!(
            sys.next().await,
            Some(Event {
                tenant: TenantId::DEFAULT,
                account: None,
                kind: EventKind::TransportChanged { tenant: None },
            })
        );
        let route = DestinationPolicy {
            pattern: "*".into(),
            tls: TlsRequirement::Required,
            smart_host: Some("relay.example.org:25".into()),
            egress_ip: None,
        };
        assert_eq!(store.put_route(tenant, 0, &route).await, Ok(1));
        assert_eq!(
            sys.next().await,
            Some(Event {
                tenant,
                account: None,
                kind: EventKind::TransportChanged {
                    tenant: Some(tenant)
                },
            })
        );
        // Отказ записи — без сигнала: следующим приходит сигнал успешной записи.
        assert_eq!(
            store.put_route(tenant, 0, &route).await,
            Err(Error::Conflict)
        );
        store.fail_next(Error::UNAVAILABLE);
        assert_eq!(store.cluster().await, Err(Error::UNAVAILABLE));
        assert_eq!(store.delete_route(tenant, 1, "*").await, Ok(2));
        assert_eq!(
            sys.next().await.map(|e| e.kind),
            Some(EventKind::TransportChanged {
                tenant: Some(tenant)
            })
        );
    }
}
