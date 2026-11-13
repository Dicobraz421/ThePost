//! In-memory архив ящика (CONTRACTS.md#Archive; CTR-76).
//!
//! [`MemArchive`] поверх [`MemMetadata`] и [`MemPolicies`]: ветка архива — папка `\Archive` и вложенные, срок письма —
//! `received_at + Policy::archive_after` по цепочке до учётной записи, квота архива — `Policy::archive_quota_bytes`.

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::Result;
use tp_contracts::archive::{ArchiveItem, ArchiveStore};
use tp_contracts::meta::{AccountScope, PolicyResolver, QuotaUsage};

use crate::meta::MemMetadata;
use crate::policies::MemPolicies;

/// [`ArchiveStore`] в памяти; клоны разделяют данные метаданных и политик.
#[derive(Debug, Clone)]
pub struct MemArchive {
    meta: MemMetadata,
    policies: MemPolicies,
}

impl MemArchive {
    /// Архив ящиков `meta` по политикам `policies`.
    pub fn new(meta: &MemMetadata, policies: &MemPolicies) -> Self {
        Self {
            meta: meta.clone(),
            policies: policies.clone(),
        }
    }
}

#[async_trait]
impl ArchiveStore for MemArchive {
    async fn archive_due(&self, now: OffsetDateTime, limit: usize) -> Result<Vec<ArchiveItem>> {
        let mut out = Vec::new();
        for (account, messages) in self.meta.archive_candidates() {
            let policy = self
                .policies
                .resolve(account.tenant, Some(account.domain), Some(account.id), None)
                .await?;
            let Some(age) = policy
                .archive_after
                .and_then(|a| time::Duration::try_from(a).ok())
            else {
                continue;
            };
            let scope = AccountScope {
                tenant: account.tenant,
                account: account.id,
            };
            out.extend(messages.into_iter().filter_map(|(message, mailbox, at)| {
                let due = at.checked_add(age)?;
                (due <= now).then_some(ArchiveItem {
                    scope,
                    message,
                    mailbox,
                    due,
                })
            }));
        }
        out.sort_unstable_by_key(|i| (i.due, i.message));
        out.truncate(limit);
        Ok(out)
    }

    async fn archive_usage(&self, scope: AccountScope) -> Result<QuotaUsage> {
        let (mut usage, domain) = self.meta.archive_usage_of(scope)?;
        usage.limit_bytes = self
            .policies
            .resolve(scope.tenant, Some(domain), Some(scope.account), None)
            .await?
            .archive_quota_bytes;
        Ok(usage)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MemQuota;
    use crate::suite::AccountStateFixture;
    use crate::suite::lifecycle::{ArchiveStores, archive};
    use tp_contracts::{DomainId, TenantId};

    /// CTR-76: общий набор архива.
    #[tokio::test]
    async fn archive_suite() {
        let meta = MemMetadata::default();
        let policies = MemPolicies::new(&meta);
        let store = MemArchive::new(&meta, &policies);
        let quota = MemQuota::new(&meta);
        let fx = AccountStateFixture {
            tenant: TenantId::DEFAULT,
            domain: DomainId::new(),
            domain_name: "example.org".into(),
        };
        let stores = ArchiveStores {
            archive: &store,
            quota: &quota,
            accounts: &meta,
            mailboxes: &meta,
            messages: &meta,
            policies: &policies,
        };
        archive(&stores, &fx).await;
    }
}
