//! [`BrandingStore`] в памяти (CTR-91, CONTRACTS.md#Branding).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tp_contracts::branding::{Branding, BrandingLevels, BrandingScope, BrandingStore};
use tp_contracts::{BlobHash, Result, TenantId};

use crate::infra::lock;

/// Брендинг установки и организаций в памяти. Клоны разделяют данные.
#[derive(Debug, Clone, Default)]
pub struct MemBranding {
    state: Arc<Mutex<HashMap<BrandingScope, Branding>>>,
}

#[async_trait]
impl BrandingStore for MemBranding {
    async fn branding(&self, scope: BrandingScope) -> Result<Option<Branding>> {
        Ok(lock(&self.state).get(&scope).cloned())
    }

    async fn set_branding(&self, scope: BrandingScope, branding: &Branding) -> Result<()> {
        branding.validate()?;
        lock(&self.state).insert(scope, branding.clone());
        Ok(())
    }

    async fn reset_branding(&self, scope: BrandingScope) -> Result<Option<Branding>> {
        Ok(lock(&self.state).remove(&scope))
    }

    async fn branding_levels(&self, tenant: Option<TenantId>) -> Result<BrandingLevels> {
        let st = lock(&self.state);
        Ok(BrandingLevels {
            installation: st.get(&BrandingScope::Installation).cloned(),
            org: tenant.and_then(|t| st.get(&BrandingScope::Org(t)).cloned()),
        })
    }

    async fn org_brandings(&self) -> Result<Vec<(TenantId, Branding)>> {
        let mut out: Vec<(TenantId, Branding)> = lock(&self.state)
            .iter()
            .filter_map(|(scope, b)| match scope {
                BrandingScope::Org(t) => Some((*t, b.clone())),
                BrandingScope::Installation => None,
            })
            .collect();
        out.sort_by_key(|(t, _)| *t);
        Ok(out)
    }

    async fn logo_in_use(&self, hash: &BlobHash) -> Result<bool> {
        Ok(lock(&self.state)
            .values()
            .any(|b| b.logo.is_some_and(|l| l.hash == *hash)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite;

    #[tokio::test]
    async fn passes_contract_suite() {
        suite::branding(&MemBranding::default(), TenantId::new(), TenantId::new()).await;
    }
}
