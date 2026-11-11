//! [`LocalAdminTokens`] в памяти (CONTRACTS.md#Auth, CTR-41).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::admin_token::{LocalAdminToken, LocalAdminTokens, LocalTokenState};
use tp_contracts::{Error, Result, TenantId};

use crate::infra::lock;

/// Локальные токены Admin API в памяти. Клоны разделяют данные.
#[derive(Debug, Clone, Default)]
pub struct MemLocalAdminTokens {
    state: Arc<Mutex<Vec<LocalAdminToken>>>,
}

impl MemLocalAdminTokens {
    /// Пустое хранилище.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl LocalAdminTokens for MemLocalAdminTokens {
    async fn issue(&self, token: LocalAdminToken) -> Result<()> {
        token.validate()?;
        if token.used_at.is_some() || token.revoked_at.is_some() {
            return Err(Error::invalid("новый токен уже погашен или отозван"));
        }
        let mut st = lock(&self.state);
        if st.iter().any(|t| t.id == token.id || t.hash == token.hash) {
            return Err(Error::Conflict);
        }
        st.push(token);
        Ok(())
    }

    async fn consume(&self, hash: &str, now: OffsetDateTime) -> Result<LocalAdminToken> {
        let mut st = lock(&self.state);
        let t = st
            .iter_mut()
            .find(|t| t.hash == hash)
            .ok_or(Error::Forbidden)?;
        if t.state(now) != LocalTokenState::Active {
            return Err(Error::Forbidden);
        }
        t.used_at = Some(now);
        Ok(t.clone())
    }

    async fn revoke(&self, id: &str, at: OffsetDateTime) -> Result<()> {
        let mut st = lock(&self.state);
        let t = st.iter_mut().find(|t| t.id == id).ok_or(Error::NotFound)?;
        t.revoked_at.get_or_insert(at);
        Ok(())
    }

    async fn list(&self, tenant: TenantId) -> Result<Vec<LocalAdminToken>> {
        let mut list: Vec<_> = lock(&self.state)
            .iter()
            .filter(|t| t.tenant == tenant)
            .cloned()
            .collect();
        list.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        Ok(list)
    }

    async fn prune(&self, before: OffsetDateTime) -> Result<u64> {
        let mut st = lock(&self.state);
        let was = st.len();
        st.retain(|t| t.expires_at >= before);
        Ok((was - st.len()) as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::suite::local_admin_tokens;

    #[tokio::test]
    async fn passes_contract_suite() {
        local_admin_tokens(&MemLocalAdminTokens::new(), TenantId::new()).await;
    }
}
