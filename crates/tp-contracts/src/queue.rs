//! Очередь исходящей почты (CONTRACTS.md#Queue (Ф1); SRS 4.1).
//!
//! Очередь — в PostgreSQL (`tp-meta`), тела писем — в BLOB. Обработку распределяют узлы роли smtp-out:
//! [`QueueStore::claim`] выдаёт записи под лизу записи (срок и номер захвата); узел без состояния, при его отказе
//! лиза истекает и запись берёт другой узел. Расписание повторов, истечение срока доставки и уведомления
//! о недоставке (DSN, RFC 3464) — `tp-smtp`; очередь только хранит состояние получателей.
//!
//! Очередь общая для всех организаций: `claim` — системный вызов роли smtp-out, организация — поле записи.
//! Время передаёт вызывающий (`now`), чтобы повторы проверялись тестовыми часами.
//!
//! # Тело письма
//!
//! Вызывающий загружает письмо одним объектом и учитывает одну ссылку на него в `BlobIndex` до `enqueue`
//! (ошибка `enqueue` — освободить ссылку). Когда все получатели в конечном статусе, [`QueueStore::complete`]
//! возвращает `true`: запись удалена, вызывающий освобождает ссылку (`BlobIndex::release`) отдельной транзакцией.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::Result;
use crate::ids::{BlobKey, QueueId, TenantId};

/// Что вернуть в уведомлении о недоставке (ESMTP `RET`, RFC 3461).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DsnRet {
    /// Письмо целиком (`RET=FULL`).
    Full,
    /// Только заголовки (`RET=HDRS`).
    Headers,
}

/// Когда уведомлять отправителя (ESMTP `NOTIFY`, RFC 3461); все `false` — `NOTIFY=NEVER`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct DsnNotify {
    /// Об успешной доставке.
    pub success: bool,
    /// Об отказе.
    pub failure: bool,
    /// О задержке.
    pub delay: bool,
}

/// Параметры DSN письма (RFC 3461).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct DsnParams {
    /// Идентификатор конверта (`ENVID`).
    pub envid: Option<String>,
    /// `RET`; `None` — не задан.
    pub ret: Option<DsnRet>,
}

/// Получатель письма в очереди.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OutRcpt {
    /// Адрес (`RCPT TO`); уникален в пределах записи.
    pub address: String,
    /// `NOTIFY`; `None` — не задан (поведение по умолчанию `tp-smtp`).
    pub notify: Option<DsnNotify>,
    /// Исходный получатель (`ORCPT`, вместе с типом адреса: `rfc822;user@example.org`).
    pub orcpt: Option<String>,
}

impl OutRcpt {
    /// Получатель без параметров DSN.
    pub fn new(address: impl Into<String>) -> Self {
        OutRcpt {
            address: address.into(),
            notify: None,
            orcpt: None,
        }
    }
}

/// Письмо для постановки в очередь.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutMessage {
    /// Организация отправителя.
    pub tenant: TenantId,
    /// Объект с письмом (RFC 5322, как отправляется); ссылка учтена вызывающим.
    pub blob: BlobKey,
    /// Размер письма, байт (ESMTP `SIZE`).
    pub size: u64,
    /// Отправитель конверта (`MAIL FROM`); пустой — уведомление о недоставке (без DSN в ответ).
    pub from: String,
    /// Параметры DSN.
    pub dsn: DsnParams,
    /// Получатели; пустой список — `Error::Invalid`.
    pub rcpts: Vec<OutRcpt>,
    /// Первая попытка доставки (обычно — момент постановки).
    pub next_attempt: OffsetDateTime,
}

/// Конечный статус получателя ([`QueueStore::complete`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RcptStatus {
    /// Доставлено (принимающий сервер ответил 2xx).
    Delivered {
        /// Ответ принимающего сервера (для DSN `NOTIFY=SUCCESS` и журнала).
        reply: String,
    },
    /// Окончательный отказ: ответ 5xx или истёк срок доставки. DSN `tp-smtp` ставит в очередь до `complete`.
    Failed {
        /// Ответ сервера или причина.
        reply: String,
    },
}

/// Захват записи узлом: идентификатор и номер захвата (fencing). Номер растёт при каждом `claim` записи;
/// вызов с устаревшим номером (лиза истекла, запись взял другой узел) — `Error::Conflict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct QueueClaim {
    /// Запись.
    pub id: QueueId,
    /// Номер захвата.
    pub fencing: u64,
}

/// Получатель, подлежащий попытке.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedRcpt {
    /// Получатель.
    pub rcpt: OutRcpt,
    /// Число неудачных попыток (вызовов `reschedule`).
    pub attempts: u32,
    /// Причина последней неудачи.
    pub last_error: Option<String>,
}

/// Захваченная запись очереди.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claimed {
    /// Захват: передаётся в `renew`, `reschedule`, `complete`.
    pub claim: QueueClaim,
    /// Лиза записи действует до этого момента.
    pub until: OffsetDateTime,
    /// Момент постановки в очередь (срок доставки считает `tp-smtp`).
    pub queued_at: OffsetDateTime,
    /// Организация.
    pub tenant: TenantId,
    /// Объект с письмом.
    pub blob: BlobKey,
    /// Размер письма, байт.
    pub size: u64,
    /// Отправитель конверта.
    pub from: String,
    /// Параметры DSN.
    pub dsn: DsnParams,
    /// Получатели без конечного статуса, срок попытки которых наступил (не пусто).
    pub rcpts: Vec<QueuedRcpt>,
}

/// Очередь исходящей почты. Реализация — `tp-meta` (PostgreSQL), локальная очередь узла edge — `tp-smtp`;
/// in-memory — `tp-testkit`.
///
/// Каждый захваченный получатель завершается `reschedule` или `complete`; не завершённые до истечения лизы
/// становятся доступны другим узлам без увеличения `attempts` (отказ узла).
#[async_trait]
pub trait QueueStore: Send + Sync + 'static {
    /// Ставит письмо в очередь; все получатели ждут попытки с `next_attempt`.
    /// Пустой список или повтор адреса получателя — `Error::Invalid`.
    async fn enqueue(&self, msg: OutMessage) -> Result<QueueId>;

    /// Захватывает до `limit` записей, у которых есть получатели без конечного статуса со сроком попытки
    /// не позже `now` и нет действующей лизы; лиза — до `now + lease`, номер захвата увеличивается.
    /// Порядок — по сроку попытки.
    async fn claim(
        &self,
        now: OffsetDateTime,
        limit: usize,
        lease: Duration,
    ) -> Result<Vec<Claimed>>;

    /// Продлевает лизу записи до `until` (долгая доставка); захват устарел — `Error::Conflict`.
    async fn renew(&self, claim: &QueueClaim, until: OffsetDateTime) -> Result<()>;

    /// Временная неудача доставки получателю: следующая попытка в `at`, `attempts` + 1, причина сохраняется.
    /// Захват устарел — `Error::Conflict`; получателя нет или он в конечном статусе — `Error::NotFound`.
    async fn reschedule(
        &self,
        claim: &QueueClaim,
        rcpt: &str,
        at: OffsetDateTime,
        reason: &str,
    ) -> Result<()>;

    /// Конечный статус получателя. `true` — все получатели записи в конечном статусе: запись удалена,
    /// вызывающий освобождает ссылку на объект письма. Ошибки — как у `reschedule`.
    async fn complete(&self, claim: &QueueClaim, rcpt: &str, status: RcptStatus) -> Result<bool>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{BlobHash, Gen};
    use std::sync::Arc;

    #[test]
    fn object_safe() {
        fn _queue(_: Arc<dyn QueueStore>) {}
    }

    #[test]
    fn serde_roundtrip() {
        let msg = OutMessage {
            tenant: TenantId::DEFAULT,
            blob: BlobKey {
                tenant: TenantId::DEFAULT,
                hash: BlobHash([7; 32]),
                generation: Gen::INITIAL,
            },
            size: 1024,
            from: "a@example.org".into(),
            dsn: DsnParams {
                envid: Some("QQ314159".into()),
                ret: Some(DsnRet::Headers),
            },
            rcpts: vec![
                OutRcpt::new("b@example.com"),
                OutRcpt {
                    address: "c@example.com".into(),
                    notify: Some(DsnNotify {
                        failure: true,
                        ..DsnNotify::default()
                    }),
                    orcpt: Some("rfc822;c@example.com".into()),
                },
            ],
            next_attempt: OffsetDateTime::UNIX_EPOCH,
        };
        let s = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(serde_json::from_str::<OutMessage>(&s).expect("parse"), msg);
        let st = RcptStatus::Failed {
            reply: "550 5.1.1 no such user".into(),
        };
        let s = serde_json::to_string(&st).expect("serialize");
        assert_eq!(serde_json::from_str::<RcptStatus>(&s).expect("parse"), st);
    }
}
