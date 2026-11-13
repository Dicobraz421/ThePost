//! Учётные записи на PostgreSQL (CONTRACTS.md#Metadata).

use std::collections::HashMap;

use async_trait::async_trait;
use sqlx::postgres::PgArguments;
use sqlx::postgres::PgRow;
use sqlx::query::QueryAs;
use sqlx::{FromRow, PgConnection, Postgres, Row};
use time::OffsetDateTime;
use tp_contracts::meta::{
    Account, AccountKind, AccountScope, AccountState, AccountStore, AccountUpsert, AddressTarget,
    DisabledAccount, ExistingContent, QuotaUsage, account_aliases, account_disabled,
    disabled_mailbox_changed,
};
use tp_contracts::{AccountId, DomainId, Error, Result, TenantId};
use uuid::Uuid;

use crate::PgMeta;
use crate::mailboxes;
use crate::schema::{begin, db_err};
use crate::shards::{ReadIntent, ShardId};

/// `AccountStore::list` без верхней границы префикса (пустой префикс или одни `U+10FFFF`): `$1` — организация,
/// `$2` — префикс, `$3` — курсор (строго больше), `$4` — лимит. Открыт для проверки плана (`EXPLAIN`) в тестах.
pub const LIST_SQL: &str = r#"SELECT account_id, tenant_id, domain_id, login, display_name, external_id, enabled, aliases, kind, hidden_from_gal
    FROM accounts
    WHERE tenant_id = $1 AND lower(login) COLLATE "C" >= $2 AND starts_with(lower(login), $2)
      AND lower(login) COLLATE "C" > $3
    ORDER BY lower(login) COLLATE "C" LIMIT $4"#;

/// Как [`LIST_SQL`], префикс — диапазоном `>= $2 AND < $5` (`$5` — `prefix_next`), не `starts_with` и не `LIKE`
/// (`%` и `_` в префиксе — обычные знаки).
pub const LIST_PREFIX_SQL: &str = r#"SELECT account_id, tenant_id, domain_id, login, display_name, external_id, enabled, aliases, kind, hidden_from_gal
    FROM accounts
    WHERE tenant_id = $1 AND lower(login) COLLATE "C" >= $2 AND lower(login) COLLATE "C" < $5
      AND lower(login) COLLATE "C" > $3
    ORDER BY lower(login) COLLATE "C" LIMIT $4"#;

/// `AccountStore::by_address` (CTR-62, META-68): `$1` — организация, `$2` — адрес в нижнем регистре без пробелов по
/// краям. Строка учётной записи (`group_id` — `NULL`) или группы (столбцы учётной записи — `NULL`); ранг — основной
/// адрес, псевдоним, группа.
const BY_ADDRESS_SQL: &str = r#"SELECT * FROM (
        SELECT account_id, tenant_id, domain_id, login, display_name, external_id, enabled, aliases, kind, hidden_from_gal,
               NULL::uuid AS group_id, CASE WHEN lower(login) = lower($2) THEN 0 ELSE 1 END AS rank
        FROM accounts WHERE tenant_id = $1 AND (lower(login) = lower($2) OR $2 = ANY(aliases))
        UNION ALL
        SELECT NULL::uuid, NULL::uuid, NULL::uuid, NULL::text, NULL::text, NULL::text, NULL::boolean, NULL::text[],
               NULL::text, NULL::boolean, group_id, 2
        FROM groups WHERE tenant_id = $1 AND address IS NOT NULL AND lower(address) = lower($2)
    ) t ORDER BY rank LIMIT 1"#;

/// Наименьшая строка больше всех строк с префиксом `prefix` в побайтном порядке (он совпадает с порядком кодовых
/// точек UTF-8): последний знак меньше `U+10FFFF` заменяется следующим (суррогаты пропускаются), хвост
/// отбрасывается. `None` — границы нет (пустой префикс или одни `U+10FFFF`).
fn prefix_next(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(c) = chars.pop() {
        let next = match c {
            '\u{D7FF}' => Some('\u{E000}'),
            c => char::from_u32(u32::from(c) + 1),
        };
        if let Some(next) = next {
            chars.push(next);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

#[derive(FromRow)]
pub(crate) struct AccountRow {
    pub(crate) account_id: Uuid,
    pub(crate) tenant_id: Uuid,
    pub(crate) domain_id: Uuid,
    pub(crate) login: String,
    pub(crate) display_name: String,
    pub(crate) external_id: Option<String>,
    pub(crate) enabled: bool,
    pub(crate) aliases: Vec<String>,
    pub(crate) kind: String,
    pub(crate) hidden_from_gal: bool,
}

/// Строка `upsert`: учётная запись и её шард.
#[derive(FromRow)]
struct UpsertRow {
    #[sqlx(flatten)]
    account: AccountRow,
    shard_id: i32,
}

/// Общие параметры `upsert` (`$3`–`$10`).
fn bind_upsert<'q>(
    q: QueryAs<'q, Postgres, UpsertRow, PgArguments>,
    data: &'q AccountUpsert,
) -> QueryAs<'q, Postgres, UpsertRow, PgArguments> {
    q.bind(data.domain.as_uuid())
        .bind(&data.login)
        .bind(&data.display_name)
        .bind(&data.external_id)
        .bind(data.enabled)
        .bind(account_aliases(
            &data.login,
            data.aliases.iter().map(String::as_str),
        ))
        .bind(kind_str(data.kind))
        .bind(data.hidden_from_gal)
}

fn kind_str(kind: AccountKind) -> &'static str {
    match kind {
        AccountKind::User => "user",
        AccountKind::Resource => "resource",
    }
}

impl TryFrom<AccountRow> for Account {
    type Error = Error;

    fn try_from(r: AccountRow) -> Result<Self> {
        let kind = match r.kind.as_str() {
            "user" => AccountKind::User,
            "resource" => AccountKind::Resource,
            other => {
                return Err(Error::internal(format!(
                    "вид учётной записи в БД: {other:?}"
                )));
            }
        };
        Ok(Account {
            id: AccountId::from_uuid(r.account_id),
            tenant: TenantId::from_uuid(r.tenant_id),
            domain: DomainId::from_uuid(r.domain_id),
            login: r.login,
            display_name: r.display_name,
            external_id: r.external_id,
            enabled: r.enabled,
            aliases: r.aliases,
            kind,
            hidden_from_gal: r.hidden_from_gal,
        })
    }
}

/// Сопоставление при синхронизации: по `external_id`, иначе по имени входа; найденная запись — с прежним
/// `enabled` (строка заблокирована до фиксации). Запись с тем же именем, но другим `external_id` — `Error::Conflict`.
async fn find_for_upsert(
    tx: &mut PgConnection,
    tenant: TenantId,
    data: &AccountUpsert,
) -> Result<Option<(Uuid, bool)>> {
    if let Some(ext) = &data.external_id {
        let id: Option<(Uuid, bool)> = sqlx::query_as(
            "SELECT account_id, enabled FROM accounts WHERE tenant_id = $1 AND external_id = $2 FOR UPDATE",
        )
        .bind(tenant.as_uuid())
        .bind(ext)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        if id.is_some() {
            return Ok(id);
        }
    }
    let row: Option<(Uuid, bool, Option<String>)> = sqlx::query_as(
        "SELECT account_id, enabled, external_id FROM accounts WHERE tenant_id = $1 AND lower(login) = lower($2) FOR UPDATE",
    )
    .bind(tenant.as_uuid())
    .bind(&data.login)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db_err)?;
    match row {
        Some((_, _, Some(other))) if data.external_id.as_ref().is_some_and(|ext| *ext != other) => {
            Err(Error::Conflict)
        }
        Some((id, enabled, _)) => Ok(Some((id, enabled))),
        None => Ok(None),
    }
}

impl PgMeta {
    /// Сигнал `AccountDisabled` (CTR-64) — после фиксации перехода `enabled` разрешённой записи в `false`;
    /// новая запись и повторное сохранение запрещённой — без сигнала.
    async fn upsert_once(&self, tenant: TenantId, data: &AccountUpsert) -> Result<Account> {
        self.rewrite(|| async move {
        if data.login.is_empty() {
            return Err(Error::invalid("пустое имя входа"));
        }
        // Шард новой учётной записи — до транзакции каталога (справочник читает свой пул).
        let place = match self.shard_resolver() {
            Some(r) => r.place(tenant).await?,
            None => ShardId::MAIN,
        };
        let mut tx = begin(&self.pool, tenant).await?;
        let found = find_for_upsert(&mut tx, tenant, data).await?;
        let disabled = found.is_some_and(|(_, was)| was) && !data.enabled;
        // Шард прежней учётной записи не меняется (перенос — модуль «Шардирование»).
        let account: UpsertRow = match found {
            Some((id, _)) => bind_upsert(
                sqlx::query_as(
                    "UPDATE accounts SET domain_id = $3, login = $4, display_name = $5, external_id = $6,
                                         enabled = $7, aliases = $8, kind = $9,
                                         hidden_from_gal = $10, updated_at = now()
                     WHERE tenant_id = $1 AND account_id = $2
                     RETURNING account_id, tenant_id, domain_id, login, display_name, external_id, enabled, aliases, kind,
                               hidden_from_gal, shard_id",
                )
                .bind(tenant.as_uuid())
                .bind(id),
                data,
            ),
            None => bind_upsert(
                sqlx::query_as(
                    "INSERT INTO accounts (tenant_id, account_id, domain_id, login, display_name, external_id,
                                           enabled, aliases, kind, hidden_from_gal, shard_id)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)
                     RETURNING account_id, tenant_id, domain_id, login, display_name, external_id, enabled, aliases, kind,
                               hidden_from_gal, shard_id",
                )
                .bind(tenant.as_uuid())
                .bind(*AccountId::new().as_uuid()),
                data,
            )
            .bind(place.0),
        }
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let shard = ShardId(account.shard_id);
        let account = Account::try_from(account.account)?;

        // Новая учётная запись — системные папки; прежняя без части папок — дополняется при входе.
        let scope = AccountScope {
            tenant,
            account: account.id,
        };
        let created = if shard == ShardId::MAIN {
            let created =
                mailboxes::ensure_system(&mut tx, scope, self.cfg.event_partitions).await?;
            crate::commits::commit(tx).await?;
            if disabled {
                self.signal(account_disabled(scope)).await;
            }
            created
        } else {
            // Ящик на другом шарде: каталог фиксируется первым, состояние ящика и папки — на шарде
            // (повтор upsert дополняет недостающее). Сигнал — сразу после фиксации каталога: повтор после
            // отказа шарда видит запрещённую запись и сигнал не повторит.
            crate::commits::commit(tx).await?;
            if disabled {
                self.signal(account_disabled(scope)).await;
            }
            let pool = self.shard_pool(shard, ReadIntent::Primary).await?;
            let mut tx = begin(&pool, tenant).await?;
            mailboxes::ensure_state(&mut tx, scope).await?;
            let created =
                mailboxes::ensure_system(&mut tx, scope, self.cfg.event_partitions).await?;
            crate::commits::commit(tx).await?;
            created
        };
        if created > 0 {
            self.signal_log([scope.account]).await;
        }
        Ok(account)
}).await
    }
}

#[async_trait]
impl AccountStore for PgMeta {
    async fn get(&self, tenant: TenantId, id: AccountId) -> Result<Option<Account>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let row: Option<AccountRow> = sqlx::query_as(
                "SELECT account_id, tenant_id, domain_id, login, display_name, external_id, enabled, aliases, kind, hidden_from_gal
             FROM accounts WHERE tenant_id = $1 AND account_id = $2",
            )
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            row.map(Account::try_from).transpose()
        })
        .await
    }

    /// Одним запросом (`account_id = ANY($2)`, по первичному ключу); порядок запроса и пропуск повторов — после
    /// чтения. Чужая организация не видна (условие `tenant_id` и RLS).
    async fn by_ids(&self, tenant: TenantId, ids: &[AccountId]) -> Result<Vec<Account>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let keys: Vec<Uuid> = ids.iter().map(|id| *id.as_uuid()).collect();
        let keys = &keys;
        let rows: Vec<AccountRow> = self
            .idempotent(|| async move {
                let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
                let rows: Vec<AccountRow> = sqlx::query_as(
                    "SELECT account_id, tenant_id, domain_id, login, display_name, external_id, enabled, aliases, kind, hidden_from_gal
                 FROM accounts WHERE tenant_id = $1 AND account_id = ANY($2)",
                )
                .bind(tenant.as_uuid())
                .bind(keys)
                .fetch_all(&mut *tx)
                .await
                .map_err(db_err)?;
                crate::commits::commit(tx).await?;
                Ok(rows)
            })
            .await?;
        let mut found: HashMap<Uuid, AccountRow> =
            rows.into_iter().map(|r| (r.account_id, r)).collect();
        // `remove` — повтор идентификатора уже не найдёт запись (первое вхождение).
        keys.iter()
            .filter_map(|k| found.remove(k))
            .map(Account::try_from)
            .collect()
    }

    /// Одним запросом (META-68): основной адрес (`accounts_login`), псевдоним (`aliases` — в нижнем регистре) и адрес
    /// группы (`groups_address`), по рангу — основной, псевдоним, группа; группа дочитывается в той же транзакции.
    /// Чужая организация не видна (условие `tenant_id` и RLS).
    async fn by_address(&self, tenant: TenantId, address: &str) -> Result<Option<AddressTarget>> {
        let address = address.trim().to_lowercase();
        let address = &address;
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let row: Option<PgRow> = sqlx::query(BY_ADDRESS_SQL)
                .bind(tenant.as_uuid())
                .bind(address)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
            let target = match row {
                None => None,
                Some(row) => match row.try_get::<Option<Uuid>, _>("group_id").map_err(db_err)? {
                    Some(id) => crate::groups::load(&mut tx, tenant, Some(vec![id]))
                        .await?
                        .into_iter()
                        .next()
                        .map(AddressTarget::Group),
                    None => Some(AddressTarget::Account(Account::try_from(
                        AccountRow::from_row(&row).map_err(db_err)?,
                    )?)),
                },
            };
            crate::commits::commit(tx).await?;
            Ok(target)
        })
        .await
    }

    async fn by_login(&self, tenant: TenantId, login: &str) -> Result<Option<Account>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let row: Option<AccountRow> = sqlx::query_as(
                "SELECT account_id, tenant_id, domain_id, login, display_name, external_id, enabled, aliases, kind, hidden_from_gal
             FROM accounts WHERE tenant_id = $1 AND lower(login) = lower($2)",
            )
            .bind(tenant.as_uuid())
            .bind(login)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            row.map(Account::try_from).transpose()
        })
        .await
    }

    async fn by_external_id(&self, tenant: TenantId, external_id: &str) -> Result<Option<Account>> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let row: Option<AccountRow> = sqlx::query_as(
                "SELECT account_id, tenant_id, domain_id, login, display_name, external_id, enabled, aliases, kind, hidden_from_gal
             FROM accounts WHERE tenant_id = $1 AND external_id = $2",
            )
            .bind(tenant.as_uuid())
            .bind(external_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            row.map(Account::try_from).transpose()
        })
        .await
    }

    /// Побайтный порядок (`COLLATE "C"`, как хранилище в памяти) по индексу `accounts_login_c`: курсор и префикс —
    /// границы диапазона по нему ([`LIST_SQL`], [`LIST_PREFIX_SQL`]). `after = None` — пустая строка: имена входа
    /// не пусты.
    async fn list(
        &self,
        tenant: TenantId,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Account>> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let next = prefix_next(prefix);
        let (next, after) = (next.as_deref(), after.unwrap_or(""));
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let sql = if next.is_some() {
                LIST_PREFIX_SQL
            } else {
                LIST_SQL
            };
            let mut q = sqlx::query_as(sql)
                .bind(tenant.as_uuid())
                .bind(prefix)
                .bind(after)
                .bind(limit);
            if let Some(next) = next {
                q = q.bind(next);
            }
            let rows: Vec<AccountRow> = q.fetch_all(&mut *tx).await.map_err(db_err)?;
            crate::commits::commit(tx).await?;
            rows.into_iter().map(Account::try_from).collect()
        })
        .await
    }

    async fn upsert(&self, tenant: TenantId, data: AccountUpsert) -> Result<Account> {
        // Параллельное создание той же учётной записи: проигравший повторяет как обновление.
        match self.upsert_once(tenant, &data).await {
            Err(Error::Conflict) => self.upsert_once(tenant, &data).await,
            r => r,
        }
    }

    /// Лимит (`limit_bytes`) разрешает `PolicyResolver`; здесь — только использование основной квоты (письма вне
    /// ветки архива, CTR-76).
    async fn quota(&self, tenant: TenantId, id: AccountId) -> Result<QuotaUsage> {
        self.idempotent(|| async move {
            let scope = AccountScope {
                tenant,
                account: id,
            };
            let db = self.box_db(scope).await?;
            if db.shard == ShardId::MAIN {
                // Без справочника шардов `box_db` учётную запись не ищет: её нет — `NotFound`.
                let mut tx = begin(&self.pool, tenant).await?;
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM accounts WHERE tenant_id = $1 AND account_id = $2)",
                )
                .bind(tenant.as_uuid())
                .bind(id.as_uuid())
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
                crate::commits::commit(tx).await?;
                if !exists {
                    return Err(Error::NotFound);
                }
            }
            let (used_bytes, messages) =
                crate::retention::box_usage(&db.pool, scope, false).await?;
            Ok(QuotaUsage {
                used_bytes,
                messages,
                limit_bytes: None,
            })
        })
        .await
    }

    /// Состояние и срок хранения — [`crate::account_state`].
    async fn account_state(&self, tenant: TenantId, id: AccountId) -> Result<AccountState> {
        self.account_state_of(tenant, id).await
    }

    /// После фиксации — сигнал `AccountDisabled` (CTR-64): вход в отключённый ящик запрещён.
    async fn disable_account(
        &self,
        tenant: TenantId,
        id: AccountId,
        until: OffsetDateTime,
    ) -> Result<()> {
        self.set_disabled(tenant, id, Some(until)).await?;
        let scope = AccountScope {
            tenant,
            account: id,
        };
        self.signal(account_disabled(scope)).await;
        self.signal(disabled_mailbox_changed(scope)).await;
        Ok(())
    }

    /// Отключённый восстановлен — сигнал `DisabledMailboxChanged` (CTR-77).
    async fn restore_account(&self, tenant: TenantId, id: AccountId) -> Result<()> {
        if self.set_disabled(tenant, id, None).await? {
            self.signal(disabled_mailbox_changed(AccountScope {
                tenant,
                account: id,
            }))
            .await;
        }
        Ok(())
    }

    async fn begin_removal(
        &self,
        tenant: TenantId,
        id: AccountId,
        now: OffsetDateTime,
    ) -> Result<()> {
        self.mark_removing(tenant, id, now).await
    }

    async fn disabled_accounts(&self, tenant: TenantId) -> Result<Vec<DisabledAccount>> {
        self.disabled_of(tenant).await
    }

    async fn expired_accounts(
        &self,
        now: OffsetDateTime,
        limit: usize,
    ) -> Result<Vec<AccountScope>> {
        self.expired(now, limit).await
    }

    async fn attach_account(
        &self,
        tenant: TenantId,
        disabled: AccountId,
        to: AccountId,
        existing: Option<ExistingContent>,
    ) -> Result<Account> {
        let account = self.attach(tenant, disabled, to, existing).await?;
        self.signal(disabled_mailbox_changed(AccountScope {
            tenant,
            account: disabled,
        }))
        .await;
        Ok(account)
    }

    async fn remove_disabled(&self, tenant: TenantId, id: AccountId) -> Result<()> {
        self.remove_disabled_box(tenant, id).await
    }

    /// Одним запросом (таблица `installation_admins`, миграция 0040): вставка из строки учётной записи, повтор —
    /// `ON CONFLICT DO NOTHING`; нет строки учётной записи — `NotFound`.
    async fn grant_installation_admin(&self, tenant: TenantId, id: AccountId) -> Result<()> {
        self.rewrite(|| async move {
            let mut tx = begin(&self.pool, tenant).await?;
            let (found,): (i64,) = sqlx::query_as(
                "WITH a AS (SELECT tenant_id, account_id FROM accounts WHERE tenant_id = $1 AND account_id = $2),
                      ins AS (INSERT INTO installation_admins (tenant_id, account_id)
                              SELECT tenant_id, account_id FROM a ON CONFLICT DO NOTHING)
                 SELECT count(*) FROM a",
            )
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            if found == 0 {
                return Err(Error::NotFound);
            }
            crate::commits::commit(tx).await?;
            Ok(())
        })
        .await
    }

    async fn is_installation_admin(&self, tenant: TenantId, id: AccountId) -> Result<bool> {
        self.idempotent(|| async move {
            let mut tx = begin(&self.read_pool(ShardId::MAIN).await?, tenant).await?;
            let (found,): (bool,) = sqlx::query_as(
                "SELECT EXISTS (SELECT 1 FROM installation_admins WHERE tenant_id = $1 AND account_id = $2)",
            )
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            crate::commits::commit(tx).await?;
            Ok(found)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::prefix_next;

    #[test]
    fn prefix_next_is_least_upper_bound() {
        assert_eq!(prefix_next(""), None);
        assert_eq!(prefix_next("ab").as_deref(), Some("ac"));
        assert_eq!(prefix_next("a_").as_deref(), Some("a`"));
        assert_eq!(prefix_next("a%").as_deref(), Some("a&"));
        assert_eq!(prefix_next("é").as_deref(), Some("ê"));
        assert_eq!(prefix_next("z\u{D7FF}").as_deref(), Some("z\u{E000}"));
        assert_eq!(prefix_next("a\u{10FFFF}").as_deref(), Some("b"));
        assert_eq!(prefix_next("\u{10FFFF}\u{10FFFF}"), None);
        // Граница больше любой строки с префиксом и не больше следующей за ними.
        for p in ["ab", "a.", "ю", "a\u{10FFFF}"] {
            let next = prefix_next(p).expect("граница");
            for s in [
                p.to_owned(),
                format!("{p}\u{10FFFF}zz"),
                format!("{p}@d.test"),
            ] {
                assert!(
                    s.as_bytes() >= p.as_bytes() && s.as_bytes() < next.as_bytes(),
                    "{p:?} {s:?}"
                );
            }
        }
    }
}
