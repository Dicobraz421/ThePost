//! Квоты (CONTRACTS.md#Trash, Retention, Quota (Ф1); SRS 4.7, 3.8).
//!
//! Использование считает реализация [`Quota`](tp_contracts::retention::Quota) по метаданным: сумма размеров писем
//! учётной записи, включая корзину; лимит — из `PolicyResolver::quota_bytes`. Поэтому учёт размера здесь —
//! проверка добавляемых байтов перед изменением:
//! - `ingest` — размер копии каждой доставки (с заголовками получателя), суммарно по учётной записи; превышение
//!   у одного получателя — `Error::QuotaExceeded` для всего запроса, до разбора и загрузки объектов;
//! - `copy` — сумма размеров копируемых писем.
//!
//! `move_to`, `delete_to_trash` и `restore` использование не меняют (корзина входит в квоту); окончательное
//! удаление (`expunge_final`, истечение корзины) уменьшает его в хранилище метаданных.
//! Проверка и изменение — не одна транзакция: параллельные доставки могут превысить лимит на размер письма.

use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, MessageId, Result, TenantId};

use crate::Store;

impl Store {
    /// Проверяет квоты: `demands` — (учётная запись, добавляемые байты), повторы суммируются.
    pub(crate) async fn check_quota(
        &self,
        tenant: TenantId,
        demands: &[(AccountId, u64)],
    ) -> Result<()> {
        let mut total: Vec<(AccountId, u64)> = Vec::new();
        for &(account, bytes) in demands {
            match total.iter_mut().find(|(a, _)| *a == account) {
                Some((_, sum)) => *sum = sum.saturating_add(bytes),
                None => total.push((account, bytes)),
            }
        }
        for (account, bytes) in total {
            self.d
                .quota
                .check(AccountScope { tenant, account }, bytes)
                .await?;
        }
        Ok(())
    }

    /// Проверяет квоту на копии писем (отсутствующие пропускаются, как в `copy`).
    pub(crate) async fn check_copy_quota(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
    ) -> Result<()> {
        let mut bytes = 0u64;
        for id in ids {
            if let Some(m) = self.d.messages.get(scope, *id).await? {
                bytes = bytes.saturating_add(m.size);
            }
        }
        if bytes == 0 {
            return Ok(());
        }
        self.check_quota(scope.tenant, &[(scope.account, bytes)])
            .await
    }
}

#[cfg(test)]
mod tests {
    use time::OffsetDateTime;
    use tp_contracts::Error;
    use tp_contracts::mailstore::{Delivery, IngestRequest, Ingested, MailStore};
    use tp_contracts::meta::{
        Flags, MailboxStore, MessageQuery, MessageStore, NewMailbox, SpecialUse,
    };
    use tp_contracts::{BlobKey, MailboxId};

    use super::*;
    use crate::testenv::{Env, stream};

    struct Fixture {
        env: Env,
        scope: AccountScope,
        inbox: MailboxId,
        raw: Vec<u8>,
    }

    async fn fixture() -> Fixture {
        let env = Env::new();
        let scope = env.meta.account("a@example.org").await.expect("acc");
        let inbox = env
            .meta
            .mailbox_by_role(scope, SpecialUse::Inbox)
            .expect("inbox");
        let raw = tp_testkit::corpus::corpus_mail("mixed-pdf")
            .expect("mail")
            .raw;
        Fixture {
            env,
            scope,
            inbox,
            raw,
        }
    }

    impl Fixture {
        async fn ingest(&self, copies: usize) -> Result<Ingested> {
            let delivery = Delivery {
                account: self.scope.account,
                mailbox: self.inbox,
                extra_headers: vec![],
                flags: Flags::new(),
            };
            self.env
                .store
                .ingest(IngestRequest {
                    tenant: TenantId::DEFAULT,
                    mail_from: String::new(),
                    raw: stream(&self.raw, 4096),
                    deliveries: vec![delivery; copies],
                    received_at: OffsetDateTime::now_utc(),
                })
                .await
        }

        async fn count(&self, mailbox: MailboxId) -> usize {
            MessageStore::list(&self.env.meta, self.scope, mailbox, MessageQuery::All, 100)
                .await
                .expect("list")
                .len()
        }

        fn size(&self) -> u64 {
            self.raw.len() as u64
        }
    }

    #[tokio::test]
    async fn ingest_at_threshold() {
        let f = fixture().await;
        f.env.quota.set_default_limit(Some(2 * f.size()));
        let first = f.ingest(1).await.expect("ingest").delivered[0].message;
        f.ingest(1).await.expect("ровно на пороге");
        let m = MessageStore::get(&f.env.meta, f.scope, first)
            .await
            .expect("get")
            .expect("msg");
        let keys: Vec<BlobKey> = std::iter::once(m.skeleton)
            .chain(m.parts.iter().map(|p| p.blob))
            .collect();
        let refs = |f: &Fixture| {
            keys.iter()
                .map(|k| f.env.index.row(k.tenant, &k.hash).expect("row").refcnt)
                .collect::<Vec<_>>()
        };
        let before = refs(&f);

        // Сверх порога — отказ без метаданных и новых ссылок.
        assert_eq!(f.ingest(1).await, Err(Error::QuotaExceeded));
        assert_eq!(f.count(f.inbox).await, 2);
        assert_eq!(refs(&f), before);

        // Корзина входит в квоту; окончательное удаление освобождает место.
        f.env
            .store
            .delete_to_trash(f.scope, &[first])
            .await
            .expect("trash");
        assert_eq!(f.ingest(1).await, Err(Error::QuotaExceeded));
        MessageStore::expunge_final(&f.env.meta, f.scope, &[first])
            .await
            .expect("expunge");
        f.ingest(1).await.expect("место освободилось");
    }

    #[tokio::test]
    async fn ingest_sums_deliveries_of_account() {
        let f = fixture().await;
        f.env.quota.set_default_limit(Some(2 * f.size() - 1));
        assert_eq!(f.ingest(2).await, Err(Error::QuotaExceeded));
        assert_eq!(f.count(f.inbox).await, 0);
        f.ingest(1).await.expect("одна копия помещается");
    }

    #[tokio::test]
    async fn copy_checks_quota() {
        let f = fixture().await;
        let a = f.ingest(1).await.expect("ingest").delivered[0].message;
        let b = f.ingest(1).await.expect("ingest").delivered[0].message;
        let to = f
            .env
            .meta
            .create(
                f.scope,
                NewMailbox {
                    parent: None,
                    name: "Archive".into(),
                    role: None,
                },
            )
            .await
            .expect("create")
            .id;
        f.env.quota.set_default_limit(Some(3 * f.size()));
        assert_eq!(
            f.env.store.copy(f.scope, &[a, b], to).await,
            Err(Error::QuotaExceeded)
        );
        assert_eq!(f.count(to).await, 0);
        f.env.store.copy(f.scope, &[a], to).await.expect("copy");
        assert_eq!(f.count(to).await, 1);
        // Перенос квоту не меняет.
        f.env.store.move_to(f.scope, &[b], to).await.expect("move");
    }
}
