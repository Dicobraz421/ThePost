//! [`AppPasswordStore`] в памяти поверх [`MemMetadata`] (CONTRACTS.md#Auth).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::app_password::{AppPassword, AppPasswordStore};
use tp_contracts::meta::{AccountScope, AccountStore};
use tp_contracts::{Error, Result};

use crate::infra::lock;
use crate::meta::MemMetadata;

/// Пароли приложений учётных записей `meta`. Клоны разделяют данные.
#[derive(Debug, Clone)]
pub struct MemAppPasswords {
    meta: MemMetadata,
    state: Arc<Mutex<HashMap<AccountScope, Vec<AppPassword>>>>,
}

impl MemAppPasswords {
    /// Пароли приложений учётных записей `meta`.
    pub fn new(meta: &MemMetadata) -> Self {
        Self {
            meta: meta.clone(),
            state: Arc::default(),
        }
    }
}

#[async_trait]
impl AppPasswordStore for MemAppPasswords {
    async fn list(&self, scope: AccountScope) -> Result<Vec<AppPassword>> {
        let mut list = lock(&self.state).get(&scope).cloned().unwrap_or_default();
        list.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        Ok(list)
    }

    async fn insert(&self, scope: AccountScope, password: AppPassword) -> Result<()> {
        password.validate()?;
        if AccountStore::get(&self.meta, scope.tenant, scope.account)
            .await?
            .is_none()
        {
            return Err(Error::NotFound);
        }
        let mut st = lock(&self.state);
        let list = st.entry(scope).or_default();
        if list.iter().any(|p| p.id == password.id) {
            return Err(Error::Conflict);
        }
        list.push(password);
        Ok(())
    }

    async fn revoke(&self, scope: AccountScope, id: &str, at: OffsetDateTime) -> Result<()> {
        let mut st = lock(&self.state);
        let p = st
            .get_mut(&scope)
            .and_then(|list| list.iter_mut().find(|p| p.id == id))
            .ok_or(Error::NotFound)?;
        p.revoked_at.get_or_insert(at);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use tp_contracts::meta::AccountUpsert;
    use tp_contracts::{DomainId, TenantId};

    use super::*;
    use crate::suite::{AppPasswordFixture, app_password_store};

    #[tokio::test]
    async fn passes_contract_suite() {
        let meta = MemMetadata::default();
        let owner = meta.account("owner@example.com").await.expect("owner");
        let peer = meta.account("peer@example.com").await.expect("peer");
        let foreign = meta
            .upsert(
                TenantId::new(),
                AccountUpsert {
                    domain: DomainId::new(),
                    login: "owner@example.com".into(),
                    display_name: "Foreign".into(),
                    external_id: None,
                    enabled: true,
                },
            )
            .await
            .expect("foreign");
        let fx = AppPasswordFixture {
            owner,
            peer,
            foreign_tenant: foreign.tenant,
        };
        app_password_store(&MemAppPasswords::new(&meta), &fx).await;
    }
}
