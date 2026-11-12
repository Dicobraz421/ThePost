//! In-memory журнал аудита Admin API (CONTRACTS.md#AdminAuditLog; CTR-25).
//!
//! [`MemAuditLog`] хранит записи в порядке добавления; [`MemAuditLog::entries`] — для проверок в тестах полос.
//! Клоны разделяют данные.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tp_contracts::admin_audit::{
    AdminAuditEntry, AdminAuditPage, AdminAuditQuery, AdminAuditRecord, AuditLog,
};
use tp_contracts::{Result, TenantId};

use crate::infra::lock;

/// [`AuditLog`] в памяти.
#[derive(Debug, Clone, Default)]
pub struct MemAuditLog(Arc<Mutex<Vec<AdminAuditEntry>>>);

impl MemAuditLog {
    /// Пустой журнал.
    pub fn new() -> Self {
        Self::default()
    }

    /// Все записи (включая уровень инсталляции) в порядке добавления.
    pub fn entries(&self) -> Vec<AdminAuditEntry> {
        lock(&self.0).clone()
    }
}

#[async_trait]
impl AuditLog for MemAuditLog {
    async fn append(&self, record: &AdminAuditRecord) -> Result<u64> {
        let mut entries = lock(&self.0);
        let seq = entries.last().map_or(1, |e| e.seq + 1);
        entries.push(AdminAuditEntry {
            seq,
            record: record.clone(),
        });
        Ok(seq)
    }

    async fn search(&self, tenant: TenantId, query: &AdminAuditQuery) -> Result<AdminAuditPage> {
        query.validate()?;
        let entries: Vec<AdminAuditEntry> = lock(&self.0)
            .iter()
            .filter(|e| e.record.tenant == Some(tenant))
            .filter(|e| query.after.is_none_or(|a| e.seq > a))
            .filter(|e| query.since.is_none_or(|s| e.record.at >= s))
            .filter(|e| query.until.is_none_or(|u| e.record.at < u))
            .take(query.limit)
            .cloned()
            .collect();
        let next = (entries.len() == query.limit)
            .then(|| entries.last().map(|e| e.seq))
            .flatten();
        Ok(AdminAuditPage { entries, next })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::admin_audit_log;

    #[tokio::test]
    async fn suite() {
        admin_audit_log(&MemAuditLog::new(), TenantId::new(), TenantId::new()).await;
    }
}
