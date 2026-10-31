//! ACL папок (RFC 4314) и делегирование отправки на PostgreSQL (SRS 4.5).
//!
//! Реализация [`AclStore`] (CONTRACTS.md#Auth): записи ACL папок учётной записи-владельца (`mailbox_acl`) и права
//! отправки от её адресов (`send_delegations`). Идентификаторы — как у [`Principal`](tp_contracts::auth::Principal):
//! имя входа, имя группы, `anyone`; ACL с `-` в начале идентификатора — отрицательные права (RFC 4314 §2).
//! Владелец учётной записи имеет все права без записей ACL — это правило `Authorizer`, здесь не хранится.

use async_trait::async_trait;
use sqlx::PgConnection;
use tp_contracts::acl::{AclStore, effective_rights, validate_identifier};
use tp_contracts::auth::{Rights, SendRight};
use tp_contracts::meta::AccountScope;
use tp_contracts::{AccountId, Error, MailboxId, Result, TenantId};
use uuid::Uuid;

pub use tp_contracts::acl::{ANYONE, AclEntry, SendDelegation, SharedMailbox};

use crate::PgMeta;
use crate::schema::{begin, db_err};

fn parse_rights(s: &str) -> Result<Rights> {
    s.parse()
        .map_err(|_| Error::internal(format!("права в mailbox_acl: {s}")))
}

fn kind_str(r: SendRight) -> &'static str {
    match r {
        SendRight::SendAs => "send_as",
        SendRight::SendOnBehalf => "send_on_behalf",
    }
}

fn parse_kind(s: &str) -> Result<SendRight> {
    match s {
        "send_as" => Ok(SendRight::SendAs),
        "send_on_behalf" => Ok(SendRight::SendOnBehalf),
        other => Err(Error::internal(format!("send_delegations.kind: {other}"))),
    }
}

/// Идентификаторы субъекта для выборки: свои, `anyone` и их отрицательные формы.
fn with_negatives(identifiers: &[String]) -> Result<(Vec<String>, Vec<String>)> {
    let mut pos: Vec<String> = Vec::with_capacity(identifiers.len() + 1);
    for id in identifiers {
        validate_identifier(id, false)?;
        pos.push(id.clone());
    }
    if !pos.iter().any(|i| i == ANYONE) {
        pos.push(ANYONE.to_owned());
    }
    let neg = pos.iter().map(|i| format!("-{i}")).collect();
    Ok((pos, neg))
}

/// Действующие права по строкам `(identifier, rights)` выборки.
fn effective(rows: Vec<(String, String)>) -> Result<Rights> {
    let rows = rows
        .into_iter()
        .map(|(id, rights)| Ok((id, parse_rights(&rights)?)))
        .collect::<Result<Vec<_>>>()?;
    Ok(effective_rights(rows))
}

async fn mailbox_exists(
    tx: &mut PgConnection,
    owner: AccountScope,
    mailbox: MailboxId,
) -> Result<bool> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM mailboxes WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = $3",
    )
    .bind(owner.tenant.as_uuid())
    .bind(owner.account.as_uuid())
    .bind(mailbox.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(found.is_some())
}

#[async_trait]
impl AclStore for PgMeta {
    async fn acl(&self, owner: AccountScope, mailbox: MailboxId) -> Result<Vec<AclEntry>> {
        let mut tx = begin(&self.pool, owner.tenant).await?;
        if !mailbox_exists(&mut tx, owner, mailbox).await? {
            return Err(Error::NotFound);
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT identifier, rights FROM mailbox_acl
             WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = $3 ORDER BY identifier COLLATE \"C\"",
        )
        .bind(owner.tenant.as_uuid())
        .bind(owner.account.as_uuid())
        .bind(mailbox.as_uuid())
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        rows.into_iter()
            .map(|(identifier, rights)| {
                Ok(AclEntry {
                    rights: parse_rights(&rights)?,
                    identifier,
                })
            })
            .collect()
    }

    async fn set_acl(
        &self,
        owner: AccountScope,
        mailbox: MailboxId,
        identifier: &str,
        rights: Rights,
    ) -> Result<()> {
        validate_identifier(identifier, true)?;
        let mut tx = begin(&self.pool, owner.tenant).await?;
        if !mailbox_exists(&mut tx, owner, mailbox).await? {
            return Err(Error::NotFound);
        }
        if rights.is_empty() {
            sqlx::query(
                "DELETE FROM mailbox_acl
                 WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = $3 AND identifier = $4",
            )
            .bind(owner.tenant.as_uuid())
            .bind(owner.account.as_uuid())
            .bind(mailbox.as_uuid())
            .bind(identifier)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        } else {
            sqlx::query(
                "INSERT INTO mailbox_acl (tenant_id, account_id, mailbox_id, identifier, rights)
                 VALUES ($1, $2, $3, $4, $5)
                 ON CONFLICT (tenant_id, account_id, mailbox_id, identifier)
                 DO UPDATE SET rights = EXCLUDED.rights, updated_at = now()",
            )
            .bind(owner.tenant.as_uuid())
            .bind(owner.account.as_uuid())
            .bind(mailbox.as_uuid())
            .bind(identifier)
            .bind(rights.to_string())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn acl_rights(
        &self,
        owner: AccountScope,
        mailbox: MailboxId,
        identifiers: &[String],
    ) -> Result<Rights> {
        let (pos, neg) = with_negatives(identifiers)?;
        let mut tx = begin(&self.pool, owner.tenant).await?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT identifier, rights FROM mailbox_acl
             WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = $3
               AND (identifier = ANY($4) OR identifier = ANY($5))",
        )
        .bind(owner.tenant.as_uuid())
        .bind(owner.account.as_uuid())
        .bind(mailbox.as_uuid())
        .bind(&pos)
        .bind(&neg)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        effective(rows)
    }

    async fn shared_mailboxes(
        &self,
        tenant: TenantId,
        identifiers: &[String],
    ) -> Result<Vec<SharedMailbox>> {
        let (pos, neg) = with_negatives(identifiers)?;
        let mut tx = begin(&self.pool, tenant).await?;
        // Папки с положительной записью субъекта, затем все его записи на эти папки.
        let rows: Vec<(Uuid, Uuid, String, String)> = sqlx::query_as(
            "WITH boxes AS (
                 SELECT DISTINCT account_id, mailbox_id FROM mailbox_acl
                 WHERE tenant_id = $1 AND identifier = ANY($2)
             )
             SELECT a.account_id, a.mailbox_id, a.identifier, a.rights
             FROM mailbox_acl a JOIN boxes b USING (account_id, mailbox_id)
             WHERE a.tenant_id = $1 AND (a.identifier = ANY($2) OR a.identifier = ANY($3))
             ORDER BY a.account_id, a.mailbox_id",
        )
        .bind(tenant.as_uuid())
        .bind(&pos)
        .bind(&neg)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        let mut out: Vec<SharedMailbox> = Vec::new();
        let mut group: Vec<(String, String)> = Vec::new();
        let mut current: Option<(Uuid, Uuid)> = None;
        let mut flush = |key: Option<(Uuid, Uuid)>, group: &mut Vec<(String, String)>| {
            if let Some((account, mailbox)) = key {
                let rights = effective(std::mem::take(group))?;
                if rights.contains(Rights::LOOKUP) {
                    out.push(SharedMailbox {
                        account: AccountId::from_uuid(account),
                        mailbox: MailboxId::from_uuid(mailbox),
                        rights,
                    });
                }
            }
            Ok::<(), Error>(())
        };
        for (account, mailbox, id, rights) in rows {
            if current != Some((account, mailbox)) {
                flush(current, &mut group)?;
                current = Some((account, mailbox));
            }
            group.push((id, rights));
        }
        flush(current, &mut group)?;
        Ok(out)
    }

    async fn send_delegations(&self, owner: AccountScope) -> Result<Vec<SendDelegation>> {
        let mut tx = begin(&self.pool, owner.tenant).await?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT delegate, kind FROM send_delegations
             WHERE tenant_id = $1 AND account_id = $2 ORDER BY delegate COLLATE \"C\"",
        )
        .bind(owner.tenant.as_uuid())
        .bind(owner.account.as_uuid())
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        rows.into_iter()
            .map(|(delegate, kind)| {
                Ok(SendDelegation {
                    delegate,
                    right: parse_kind(&kind)?,
                })
            })
            .collect()
    }

    async fn set_send_delegation(
        &self,
        owner: AccountScope,
        delegate: &str,
        right: Option<SendRight>,
    ) -> Result<()> {
        validate_identifier(delegate, false)?;
        let mut tx = begin(&self.pool, owner.tenant).await?;
        let found: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM accounts WHERE tenant_id = $1 AND account_id = $2")
                .bind(owner.tenant.as_uuid())
                .bind(owner.account.as_uuid())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
        if found.is_none() {
            return Err(Error::NotFound);
        }
        match right {
            None => {
                sqlx::query(
                    "DELETE FROM send_delegations WHERE tenant_id = $1 AND account_id = $2 AND delegate = $3",
                )
                .bind(owner.tenant.as_uuid())
                .bind(owner.account.as_uuid())
                .bind(delegate)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            }
            Some(r) => {
                sqlx::query(
                    "INSERT INTO send_delegations (tenant_id, account_id, delegate, kind) VALUES ($1, $2, $3, $4)
                     ON CONFLICT (tenant_id, account_id, delegate)
                     DO UPDATE SET kind = EXCLUDED.kind, updated_at = now()",
                )
                .bind(owner.tenant.as_uuid())
                .bind(owner.account.as_uuid())
                .bind(delegate)
                .bind(kind_str(r))
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            }
        }
        tx.commit().await.map_err(db_err)?;
        Ok(())
    }

    async fn send_right_of(
        &self,
        owner: AccountScope,
        identifiers: &[String],
    ) -> Result<Option<SendRight>> {
        for id in identifiers {
            validate_identifier(id, false)?;
        }
        let mut tx = begin(&self.pool, owner.tenant).await?;
        let kinds: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT kind FROM send_delegations
             WHERE tenant_id = $1 AND account_id = $2 AND delegate = ANY($3)",
        )
        .bind(owner.tenant.as_uuid())
        .bind(owner.account.as_uuid())
        .bind(identifiers)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        let rights = kinds
            .iter()
            .map(|k| parse_kind(k))
            .collect::<Result<Vec<_>>>()?;
        Ok(if rights.contains(&SendRight::SendAs) {
            Some(SendRight::SendAs)
        } else {
            rights.first().copied()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_identifiers() {
        let (pos, neg) = with_negatives(&["fred".into()]).expect("ids");
        assert_eq!(pos, ["fred", ANYONE]);
        assert_eq!(neg, ["-fred", "-anyone"]);
        let (pos, _) = with_negatives(&[ANYONE.into()]).expect("ids");
        assert_eq!(pos, [ANYONE]);
        assert!(with_negatives(&["-fred".into()]).is_err());
        let rows = vec![
            ("fred".to_owned(), "lrswi".to_owned()),
            ("-fred".to_owned(), "w".to_owned()),
        ];
        assert_eq!(effective(rows).expect("rights").to_string(), "lrsi");
        assert!(effective(vec![("fred".into(), "?".into())]).is_err());
    }

    #[test]
    fn kinds() {
        for r in [SendRight::SendAs, SendRight::SendOnBehalf] {
            assert_eq!(parse_kind(kind_str(r)).expect("kind"), r);
        }
        assert!(parse_kind("x").is_err());
    }
}
