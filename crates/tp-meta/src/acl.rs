//! ACL папок (RFC 4314) и делегирование отправки на PostgreSQL (SRS 4.5).
//!
//! Хранилище для `Authorizer` (`tp-auth`): записи ACL папок учётной записи-владельца (`mailbox_acl`) и права
//! отправки от её адресов (`send_delegations`). Идентификаторы — как у [`Principal`](tp_contracts::auth::Principal):
//! имя входа, имя группы, `anyone`; ACL с `-` в начале идентификатора — отрицательные права (RFC 4314 §2).
//! Владелец учётной записи имеет все права без записей ACL — это правило `Authorizer`, здесь не хранится.

use serde::{Deserialize, Serialize};
use sqlx::PgConnection;
use tp_contracts::auth::{Rights, SendRight};
use tp_contracts::{AccountId, Error, MailboxId, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::schema::{begin, db_err};

/// Идентификатор «все пользователи организации» (RFC 4314 §2).
pub const ANYONE: &str = "anyone";

/// Запись ACL папки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AclEntry {
    /// Идентификатор (с `-` в начале — отрицательные права).
    pub identifier: String,
    /// Права.
    pub rights: Rights,
}

/// Папка другой учётной записи, доступная субъекту по ACL (пространство имён общих папок IMAP).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedMailbox {
    /// Владелец.
    pub account: AccountId,
    /// Папка.
    pub mailbox: MailboxId,
    /// Действующие права субъекта (без прав владельца).
    pub rights: Rights,
}

/// Право отправки, выданное представителю.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SendDelegation {
    /// Представитель: имя входа или группы.
    pub delegate: String,
    /// Вид права.
    pub right: SendRight,
}

fn check_identifier(id: &str, negative_allowed: bool) -> Result<()> {
    let bad = id.is_empty()
        || id.len() > 255
        || id == "-"
        || id.chars().any(|c| c.is_whitespace() || c.is_control())
        || (!negative_allowed && id.starts_with('-'));
    if bad {
        return Err(Error::invalid(format!(
            "недопустимый идентификатор: {id:?}"
        )));
    }
    Ok(())
}

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
        check_identifier(id, false)?;
        pos.push(id.clone());
    }
    if !pos.iter().any(|i| i == ANYONE) {
        pos.push(ANYONE.to_owned());
    }
    let neg = pos.iter().map(|i| format!("-{i}")).collect();
    Ok((pos, neg))
}

/// Действующие права: объединение положительных записей минус объединение отрицательных.
fn effective(rows: impl IntoIterator<Item = (String, String)>) -> Result<Rights> {
    let (mut plus, mut minus) = (Rights::NONE, Rights::NONE);
    for (id, rights) in rows {
        let r = parse_rights(&rights)?;
        if id.starts_with('-') {
            minus |= r;
        } else {
            plus |= r;
        }
    }
    Ok(plus - minus)
}

async fn mailbox_exists(
    tx: &mut PgConnection,
    tenant: TenantId,
    owner: AccountId,
    mailbox: MailboxId,
) -> Result<bool> {
    let found: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM mailboxes WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = $3",
    )
    .bind(tenant.as_uuid())
    .bind(owner.as_uuid())
    .bind(mailbox.as_uuid())
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    Ok(found.is_some())
}

impl PgMeta {
    /// ACL папки (IMAP GETACL), по идентификатору. Папки нет — `NotFound`.
    pub async fn acl(
        &self,
        tenant: TenantId,
        owner: AccountId,
        mailbox: MailboxId,
    ) -> Result<Vec<AclEntry>> {
        let mut tx = begin(&self.pool, tenant).await?;
        if !mailbox_exists(&mut tx, tenant, owner, mailbox).await? {
            return Err(Error::NotFound);
        }
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT identifier, rights FROM mailbox_acl
             WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = $3 ORDER BY identifier COLLATE \"C\"",
        )
        .bind(tenant.as_uuid())
        .bind(owner.as_uuid())
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

    /// Задаёт права идентификатора на папку целиком (IMAP SETACL); пустые права удаляют запись
    /// (DELETEACL). Папки нет — `NotFound`; недопустимый идентификатор — `Invalid`.
    pub async fn set_acl(
        &self,
        tenant: TenantId,
        owner: AccountId,
        mailbox: MailboxId,
        identifier: &str,
        rights: Rights,
    ) -> Result<()> {
        check_identifier(identifier, true)?;
        let mut tx = begin(&self.pool, tenant).await?;
        if !mailbox_exists(&mut tx, tenant, owner, mailbox).await? {
            return Err(Error::NotFound);
        }
        if rights.is_empty() {
            sqlx::query(
                "DELETE FROM mailbox_acl
                 WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = $3 AND identifier = $4",
            )
            .bind(tenant.as_uuid())
            .bind(owner.as_uuid())
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
            .bind(tenant.as_uuid())
            .bind(owner.as_uuid())
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

    /// Права субъекта с идентификаторами `identifiers` (имя входа, группы) на папку по ACL: записи
    /// идентификаторов и `anyone`, минус их отрицательные записи. Права владельца не учитываются.
    pub async fn acl_rights(
        &self,
        tenant: TenantId,
        owner: AccountId,
        mailbox: MailboxId,
        identifiers: &[String],
    ) -> Result<Rights> {
        let (pos, neg) = with_negatives(identifiers)?;
        let mut tx = begin(&self.pool, tenant).await?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT identifier, rights FROM mailbox_acl
             WHERE tenant_id = $1 AND account_id = $2 AND mailbox_id = $3
               AND (identifier = ANY($4) OR identifier = ANY($5))",
        )
        .bind(tenant.as_uuid())
        .bind(owner.as_uuid())
        .bind(mailbox.as_uuid())
        .bind(&pos)
        .bind(&neg)
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        effective(rows)
    }

    /// Папки других учётных записей, на которые у субъекта есть право `l` (RFC 4314 LIST), с действующими правами.
    pub async fn shared_mailboxes(
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
                let rights = effective(group.drain(..))?;
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

    /// Делегирования отправки учётной записи-владельца, по представителю.
    pub async fn send_delegations(
        &self,
        tenant: TenantId,
        owner: AccountId,
    ) -> Result<Vec<SendDelegation>> {
        let mut tx = begin(&self.pool, tenant).await?;
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT delegate, kind FROM send_delegations
             WHERE tenant_id = $1 AND account_id = $2 ORDER BY delegate COLLATE \"C\"",
        )
        .bind(tenant.as_uuid())
        .bind(owner.as_uuid())
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

    /// Выдаёт представителю право отправки от адресов владельца (Send As / Send on Behalf);
    /// `None` — отзывает. Учётной записи нет — `NotFound`.
    pub async fn set_send_delegation(
        &self,
        tenant: TenantId,
        owner: AccountId,
        delegate: &str,
        right: Option<SendRight>,
    ) -> Result<()> {
        check_identifier(delegate, false)?;
        let mut tx = begin(&self.pool, tenant).await?;
        let found: Option<i32> =
            sqlx::query_scalar("SELECT 1 FROM accounts WHERE tenant_id = $1 AND account_id = $2")
                .bind(tenant.as_uuid())
                .bind(owner.as_uuid())
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
                .bind(tenant.as_uuid())
                .bind(owner.as_uuid())
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
                .bind(tenant.as_uuid())
                .bind(owner.as_uuid())
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

    /// Право субъекта с идентификаторами `identifiers` отправлять от адресов владельца: сильнейшее из
    /// выданных (Send As сильнее Send on Behalf); `None` — нет права.
    pub async fn send_right_of(
        &self,
        tenant: TenantId,
        owner: AccountId,
        identifiers: &[String],
    ) -> Result<Option<SendRight>> {
        for id in identifiers {
            check_identifier(id, false)?;
        }
        let mut tx = begin(&self.pool, tenant).await?;
        let kinds: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT kind FROM send_delegations
             WHERE tenant_id = $1 AND account_id = $2 AND delegate = ANY($3)",
        )
        .bind(tenant.as_uuid())
        .bind(owner.as_uuid())
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
    fn identifiers() {
        for ok in ["fred", "group:sales", "anyone", "a@b.test"] {
            assert!(check_identifier(ok, false).is_ok(), "{ok}");
        }
        assert!(check_identifier("-fred", true).is_ok());
        for bad in ["", "-", "a b", "a\tb", "a\u{7}"] {
            assert!(check_identifier(bad, true).is_err(), "{bad:?}");
        }
        assert!(check_identifier("-fred", false).is_err());
        assert!(check_identifier(&"x".repeat(256), true).is_err());
    }

    #[test]
    fn negative_rights_subtract() {
        let rows = vec![
            ("fred".to_owned(), "lrswi".to_owned()),
            ("anyone".to_owned(), "lr".to_owned()),
            ("-fred".to_owned(), "w".to_owned()),
        ];
        assert_eq!(effective(rows).expect("rights").to_string(), "lrsi");
        let (pos, neg) = with_negatives(&["fred".into()]).expect("ids");
        assert_eq!(pos, ["fred", "anyone"]);
        assert_eq!(neg, ["-fred", "-anyone"]);
        assert!(with_negatives(&["-fred".into()]).is_err());
    }

    #[test]
    fn kinds() {
        for r in [SendRight::SendAs, SendRight::SendOnBehalf] {
            assert_eq!(parse_kind(kind_str(r)).expect("kind"), r);
        }
        assert!(parse_kind("x").is_err());
    }
}
