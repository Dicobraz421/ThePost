//! Лизы на PostgreSQL (CONTRACTS.md#Lease).
//!
//! Одна строка `leases` на ресурс; каждая операция — один оператор вне транзакции организации (таблица
//! без RLS). Время истечения считает СУБД (`now()`): часы узлов приложения не участвуют.
//! Строка не удаляется и при освобождении — иначе номер захвата (`fencing`) начался бы заново.

use std::time::Duration;

use async_trait::async_trait;
use sqlx::FromRow;
use time::OffsetDateTime;
use tp_contracts::lease::{Lease, LeaseStore};
use tp_contracts::{Error, Result};

use crate::PgMeta;
use crate::schema::{db_err, to_i64, to_u64};

#[derive(FromRow)]
struct LeaseRow {
    name: String,
    holder: String,
    fencing: i64,
    expires_at: OffsetDateTime,
}

impl TryFrom<LeaseRow> for Lease {
    type Error = Error;

    fn try_from(r: LeaseRow) -> Result<Self> {
        Ok(Lease {
            name: r.name,
            holder: r.holder,
            fencing: to_u64(r.fencing)?,
            expires_at: r.expires_at,
        })
    }
}

/// Срок лизы в микросекундах (аргумент `$n * interval '1 microsecond'`).
fn ttl_arg(ttl: Duration) -> Result<i64> {
    i64::try_from(ttl.as_micros()).map_err(|_| Error::invalid("срок лизы слишком велик"))
}

fn check_names(name: &str, holder: &str) -> Result<()> {
    if name.is_empty() || holder.is_empty() {
        return Err(Error::invalid("имя лизы и держатель не могут быть пустыми"));
    }
    Ok(())
}

#[async_trait]
impl LeaseStore for PgMeta {
    async fn try_acquire(&self, name: &str, holder: &str, ttl: Duration) -> Result<Option<Lease>> {
        check_names(name, holder)?;
        // Свободная (истёкшая) лиза достаётся новому держателю с новым номером захвата;
        // действующая лиза того же держателя только продлевается.
        let row: Option<LeaseRow> = sqlx::query_as(
            "INSERT INTO leases (name, holder, fencing, expires_at)
             VALUES ($1, $2, 1, now() + $3 * interval '1 microsecond')
             ON CONFLICT (name) DO UPDATE SET
                 fencing = CASE WHEN leases.holder = EXCLUDED.holder AND leases.expires_at > now()
                                THEN leases.fencing ELSE leases.fencing + 1 END,
                 holder = EXCLUDED.holder,
                 expires_at = EXCLUDED.expires_at
             WHERE leases.holder = EXCLUDED.holder OR leases.expires_at <= now()
             RETURNING name, holder, fencing, expires_at",
        )
        .bind(name)
        .bind(holder)
        .bind(ttl_arg(ttl)?)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(Lease::try_from).transpose()
    }

    async fn renew(&self, lease: &Lease, ttl: Duration) -> Result<Option<Lease>> {
        // Истёкшую, но никем не захваченную лизу (номер захвата не изменился) можно продлить.
        let row: Option<LeaseRow> = sqlx::query_as(
            "UPDATE leases SET expires_at = now() + $4 * interval '1 microsecond'
             WHERE name = $1 AND holder = $2 AND fencing = $3
             RETURNING name, holder, fencing, expires_at",
        )
        .bind(&lease.name)
        .bind(&lease.holder)
        .bind(to_i64(lease.fencing)?)
        .bind(ttl_arg(ttl)?)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(Lease::try_from).transpose()
    }

    async fn release(&self, lease: &Lease) -> Result<()> {
        sqlx::query(
            "UPDATE leases SET expires_at = least(expires_at, now())
             WHERE name = $1 AND holder = $2 AND fencing = $3",
        )
        .bind(&lease.name)
        .bind(&lease.holder)
        .bind(to_i64(lease.fencing)?)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }
}
