//! In-memory журнал аудита Admin API (CONTRACTS.md#AdminAuditLog; CTR-25).
//!
//! [`MemAuditLog`] хранит записи в порядке добавления и отметки выгрузки приёмников; [`MemAuditLog::entries`] — для
//! проверок в тестах полос. Клоны разделяют данные.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tp_contracts::admin_audit::{
    AdminAuditEntry, AdminAuditPage, AdminAuditQuery, AdminAuditRecord, AuditLog, check_export_mark,
};
use tp_contracts::{Error, Result, TenantId};

use crate::infra::lock;

/// [`AuditLog`] в памяти.
#[derive(Debug, Clone, Default)]
pub struct MemAuditLog {
    entries: Arc<Mutex<Vec<AdminAuditEntry>>>,
    marks: Arc<Mutex<HashMap<String, u64>>>,
}

impl MemAuditLog {
    /// Пустой журнал.
    pub fn new() -> Self {
        Self::default()
    }

    /// Все записи (включая уровень инсталляции) в порядке добавления.
    pub fn entries(&self) -> Vec<AdminAuditEntry> {
        lock(&self.entries).clone()
    }
}

#[async_trait]
impl AuditLog for MemAuditLog {
    async fn append(&self, record: &AdminAuditRecord) -> Result<u64> {
        let mut entries = lock(&self.entries);
        let seq = entries.last().map_or(1, |e| e.seq + 1);
        entries.push(AdminAuditEntry {
            seq,
            record: record.clone(),
        });
        Ok(seq)
    }

    async fn search(&self, tenant: TenantId, query: &AdminAuditQuery) -> Result<AdminAuditPage> {
        query.validate()?;
        let entries: Vec<AdminAuditEntry> = lock(&self.entries)
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

    async fn export_page(&self, after: u64, limit: usize) -> Result<Vec<AdminAuditEntry>> {
        if limit == 0 {
            return Err(Error::invalid("audit export: limit must be at least 1"));
        }
        Ok(lock(&self.entries)
            .iter()
            .filter(|e| e.seq > after)
            .take(limit)
            .cloned()
            .collect())
    }

    async fn export_mark(&self, sink: &str) -> Result<u64> {
        Ok(lock(&self.marks).get(sink).copied().unwrap_or(0))
    }

    async fn advance_export_mark(&self, sink: &str, from: u64, to: u64) -> Result<bool> {
        check_export_mark(sink, from, to)?;
        let mut marks = lock(&self.marks);
        let mark = marks.entry(sink.to_owned()).or_insert(0);
        if *mark != from {
            return Ok(false);
        }
        *mark = to;
        Ok(true)
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
