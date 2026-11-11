//! In-memory списки отправителей ящиков (CONTRACTS.md#SenderLists; CTR-35).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tp_contracts::Result;
use tp_contracts::meta::AccountScope;
use tp_contracts::senders::{SenderList, SenderLists};

use crate::infra::lock;

/// Списки отправителей в памяти; клоны разделяют данные.
#[derive(Debug, Clone, Default)]
pub struct MemSenderLists {
    lists: Arc<Mutex<HashMap<AccountScope, (u64, SenderList)>>>,
}

impl MemSenderLists {
    /// Пустое хранилище.
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl SenderLists for MemSenderLists {
    async fn sender_list(&self, scope: AccountScope) -> Result<SenderList> {
        Ok(lock(&self.lists)
            .get(&scope)
            .map(|(_, l)| l.clone())
            .unwrap_or_default())
    }

    async fn put_sender_list(&self, scope: AccountScope, list: SenderList) -> Result<u64> {
        let list = list.normalized()?;
        let mut st = lock(&self.lists);
        let e = st.entry(scope).or_default();
        e.0 += 1;
        e.1 = list;
        Ok(e.0)
    }

    async fn sender_list_state(&self, scope: AccountScope) -> Result<u64> {
        Ok(lock(&self.lists).get(&scope).map_or(0, |(s, _)| *s))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tp_contracts::{AccountId, TenantId};

    #[tokio::test]
    async fn passes_contract_suite() {
        let scope = |tenant| AccountScope {
            tenant,
            account: AccountId::new(),
        };
        crate::suite::sender_lists(
            &MemSenderLists::new(),
            scope(TenantId::DEFAULT),
            scope(TenantId::new()),
        )
        .await;
    }
}
