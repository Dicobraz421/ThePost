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
//! # Без опроса (SRS 2.8, CTR-73)
//!
//! smtp-out ждёт сигнала [`EventKind::QueueReady`] ([`queue_ready`]) или ближайшего срока [`QueueStore::next_due`] —
//! один таймер. Сигнал публикует реализация после фиксации постановки и переноса; потерянный сигнал (at-most-once)
//! покрывает сверка `next_due` по `Resync` и при запуске.
//!
//! # Просмотр и повтор (CTR-85)
//!
//! Раздел «Очередь» консоли и текстового интерфейса: [`QueueStore::list`] — записи организации порциями с фильтром
//! по состоянию ([`QueueState`]) и адресу, [`QueueStore::retry`] — повтор доставки «сейчас» по запросу администратора.
//!
//! # Тело письма
//!
//! Вызывающий загружает письмо одним объектом и учитывает одну ссылку на него в `BlobIndex` до `enqueue`
//! (ошибка `enqueue` — освободить ссылку); учётная запись отправителя — одна и та же в `BlobIndex::acquire` и
//! [`QueueStore::enqueue`] (CTR-49: шард), её передаёт [`Outbound`] (CTR-74). Когда все получатели в конечном статусе, [`QueueStore::complete`]
//! возвращает `true`: запись удалена, вызывающий освобождает ссылку (`BlobIndex::release`) отдельной транзакцией.
//!
//! # Постановка письма
//!
//! [`Outbound`] — постановка письма в очередь протоколами (submission SMTP, пересылки модулей, JMAP
//! `EmailSubmission/set`) поверх `QueueStore`: загрузка тела, учёт ссылки, подпись DKIM — в реализации
//! (`tp_smtp::queue::Queue`). [`Outbound::submit`] возвращает статус по каждому получателю (`tp_smtp::Delivery`
//! доставляет локальных получателей сразу).

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::bus::{Event, EventKind};
use crate::error::{Error, Result};
use crate::ids::{AccountId, BlobKey, QueueId, TenantId};

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
    /// Срок выпуска (`release_at`, CTR-35): первая попытка доставки не раньше этого момента — обычно момент постановки,
    /// для отложенной отправки (JMAP `sendAt`) — срок отправки; до первого захвата запись можно отменить
    /// ([`QueueStore::cancel_queued`]).
    pub next_attempt: OffsetDateTime,
}

impl OutMessage {
    /// Срок выпуска письма (CTR-35) — [`OutMessage::next_attempt`].
    pub fn release_at(&self) -> OffsetDateTime {
        self.next_attempt
    }
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
    /// Число неудачных попыток (вызовов `reschedule`; `postpone` не считается).
    pub attempts: u32,
    /// Причина последней неудачи.
    pub last_error: Option<String>,
}

/// Захваченная запись очереди.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claimed {
    /// Захват: передаётся в `renew`, `reschedule`, `postpone`, `complete`.
    pub claim: QueueClaim,
    /// Лиза записи действует до этого момента.
    pub until: OffsetDateTime,
    /// Момент постановки в очередь, а для отложенного письма — срок выпуска, если он позже (CTR-35): от него
    /// `tp-smtp` считает срок доставки.
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

/// Состояние записи очереди в момент `now` ([`QueueStore::list`], фильтр [`QueueFilter::state`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QueueState {
    /// Ждёт попытки: лизы нет и неудачных попыток у получателей без конечного статуса не было (в том числе
    /// отложенная отправка до срока выпуска).
    Ready,
    /// Повтор: лизы нет, у получателя без конечного статуса была временная неудача (`attempts` > 0).
    Retry,
    /// Доставляется: действует лиза (`until > now`).
    Active,
}

/// Состояние получателя записи очереди ([`QueueItem::rcpts`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueRcptState {
    /// Ждёт попытки не раньше `next_attempt`.
    Pending {
        /// Срок следующей попытки.
        next_attempt: OffsetDateTime,
    },
    /// Конечный статус ([`QueueStore::complete`]); запись остаётся в очереди, пока не завершены все получатели.
    Done(RcptStatus),
}

/// Получатель записи очереди ([`QueueStore::list`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItemRcpt {
    /// Получатель.
    pub rcpt: OutRcpt,
    /// Состояние.
    pub state: QueueRcptState,
    /// Число неудачных попыток (как [`QueuedRcpt::attempts`]).
    pub attempts: u32,
    /// Причина последней неудачи.
    pub last_error: Option<String>,
}

/// Запись очереди для просмотра ([`QueueStore::list`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueItem {
    /// Запись.
    pub id: QueueId,
    /// Состояние в момент запроса.
    pub state: QueueState,
    /// Момент постановки (для отложенного письма — срок выпуска, если он позже), как [`Claimed::queued_at`].
    pub queued_at: OffsetDateTime,
    /// Ближайший срок попытки: наименьший среди получателей без конечного статуса (у записи всегда есть такой).
    pub next_attempt: OffsetDateTime,
    /// Отправитель конверта.
    pub from: String,
    /// Размер письма, байт.
    pub size: u64,
    /// Получатели в порядке постановки.
    pub rcpts: Vec<QueueItemRcpt>,
}

/// Фильтр [`QueueStore::list`]; поля по умолчанию — все записи организации.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct QueueFilter {
    /// Только записи в этом состоянии.
    pub state: Option<QueueState>,
    /// Только записи, у которых отправитель или хотя бы один получатель содержит эту строку без учёта регистра;
    /// пустая строка — без фильтра по адресу.
    pub address: Option<String>,
}

impl QueueFilter {
    /// Запись `item` проходит фильтр (правило для реализаций в памяти и проверки реализаций хранилища).
    pub fn matches(&self, item: &QueueItem) -> bool {
        let state = self.state.is_none_or(|s| s == item.state);
        let address = match self.address.as_deref() {
            None | Some("") => true,
            Some(needle) => {
                let needle = needle.to_lowercase();
                std::iter::once(item.from.as_str())
                    .chain(item.rcpts.iter().map(|r| r.rcpt.address.as_str()))
                    .any(|a| a.to_lowercase().contains(&needle))
            }
        };
        state && address
    }
}

/// Сигнал «очередь организации `tenant` пополнена или срок перенесён» (системный, `account = None`,
/// [`EventKind::QueueReady`]; тема `<tenant>/queue/outbound`, CTR-73). Публикует реализация [`QueueStore`] после
/// фиксации `enqueue`, `reschedule`, `postpone` и `retry`, перенёсшего срок.
pub fn queue_ready(tenant: TenantId) -> Event {
    Event {
        tenant,
        account: None,
        kind: EventKind::QueueReady,
    }
}

/// Очередь исходящей почты. Реализация — `tp-meta` (PostgreSQL), локальная очередь узла edge — `tp-smtp`;
/// in-memory — `tp-testkit`.
///
/// Каждый захваченный получатель завершается `reschedule`, `postpone` или `complete`; не завершённые до истечения лизы
/// становятся доступны другим узлам без увеличения `attempts` (отказ узла).
#[async_trait]
pub trait QueueStore: Send + Sync + 'static {
    /// Ставит письмо в очередь; все получатели ждут попытки с `next_attempt`.
    /// Пустой список или повтор адреса получателя — `Error::Invalid`.
    ///
    /// `owner` — учётная запись отправителя организации `msg.tenant` (CTR-49, решение 46): хранилище с шардами
    /// ставит запись на шард её ящика, рядом с «Отправленными»; `None` — отправителя-учётной записи нет (DSN,
    /// пересылка, письмо модуля) — шард по выбору хранилища. Хранилище без шардов `owner` не использует;
    /// существование учётной записи не проверяется.
    /// После фиксации — сигнал [`queue_ready`] организации `msg.tenant`.
    async fn enqueue(&self, owner: Option<AccountId>, msg: OutMessage) -> Result<QueueId>;

    /// Ближайший момент, когда [`QueueStore::claim`] выдаст запись (CTR-73; таймер smtp-out): наименьший срок
    /// попытки получателей без конечного статуса, а у записи с лизой — не раньше её конца. Записей нет — `None`.
    /// Системный вызов (все организации).
    async fn next_due(&self) -> Result<Option<OffsetDateTime>>;

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
    /// Захват устарел — `Error::Conflict`; получателя нет или он в конечном статусе — `Error::NotFound`. После
    /// фиксации — сигнал [`queue_ready`] организации записи.
    async fn reschedule(
        &self,
        claim: &QueueClaim,
        rcpt: &str,
        at: OffsetDateTime,
        reason: &str,
    ) -> Result<()>;

    /// Перенос получателя планировщиком без попытки доставки (лимит соединений или скорости домена назначения,
    /// нагрузка узла): следующая попытка в `at`, `attempts` и `last_error` не меняются. Ошибки и сигнал — как у
    /// `reschedule`. По умолчанию — `Error::Internal` (реализация без переноса).
    async fn postpone(&self, claim: &QueueClaim, rcpt: &str, at: OffsetDateTime) -> Result<()> {
        let _ = (claim, rcpt, at);
        Err(Error::internal("queue store does not support postpone"))
    }

    /// Конечный статус получателя. `true` — все получатели записи в конечном статусе: запись удалена,
    /// вызывающий освобождает ссылку на объект письма. Ошибки — как у `reschedule`.
    async fn complete(&self, claim: &QueueClaim, rcpt: &str, status: RcptStatus) -> Result<bool>;

    /// Отмена отправки до выпуска (CTR-35; JMAP `undoStatus: canceled`): удаляет запись организации `tenant`, которую
    /// ещё не захватывали (нет попыток и лизы), и возвращает объект письма — вызывающий освобождает ссылку. Запись уже
    /// захвачена или были попытки — `Error::Conflict` (JMAP `cannotUnsend`); нет записи (в том числе доставлена) —
    /// `Error::NotFound`. Отмена и `claim` атомарны между собой. По умолчанию — `Error::Internal`.
    async fn cancel_queued(&self, tenant: TenantId, id: QueueId) -> Result<BlobKey> {
        let _ = (tenant, id);
        Err(Error::internal(
            "queue store does not support cancel_queued",
        ))
    }

    /// Записи организации `tenant` (CTR-85; консоль и TUI) в порядке `id` строго после `after` (`None` — с начала),
    /// не более `limit`, прошедшие фильтр ([`QueueFilter::matches`]); состояние — в момент `now`. Курсор следующей
    /// порции — `id` последней записи; порция короче `limit` — последняя. Записи, поставленные или завершённые между
    /// порциями, не дают ни пропусков, ни повторов остальных. `limit` = 0 — `Error::Invalid`. По умолчанию —
    /// `Error::Internal`.
    async fn list(
        &self,
        tenant: TenantId,
        filter: &QueueFilter,
        after: Option<QueueId>,
        limit: usize,
        now: OffsetDateTime,
    ) -> Result<Vec<QueueItem>> {
        let _ = (tenant, filter, after, limit, now);
        Err(Error::internal("queue store does not support list"))
    }

    /// Повтор доставки по запросу администратора (CTR-85): срок попытки `now` у всех получателей без конечного
    /// статуса записи `id` организации `tenant` (`attempts` и `last_error` не меняются; отложенное письмо выпускается — `queued_at` не позже `now`)
    /// и сигнал [`queue_ready`] после фиксации — `next_due` не позже `now`. `true` — срок перенесён; запись под
    /// действующей лизой (доставляется) — без изменений и без сигнала, `false`. Нет записи (в том числе другой
    /// организации или уже завершена) — `Error::NotFound`. По умолчанию — `Error::Internal`.
    async fn retry(&self, tenant: TenantId, id: QueueId, now: OffsetDateTime) -> Result<bool> {
        let _ = (tenant, id, now);
        Err(Error::internal("queue store does not support retry"))
    }
}

/// Статус получателя при отправке ([`Outbound::submit`]); JMAP — `deliveryStatus` EmailSubmission (RFC 8621 §7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SubmitStatus {
    /// Доставлено в локальный ящик.
    Delivered,
    /// Поставлено в очередь исходящей почты (внешний адрес) или ждёт модерации.
    Queued,
    /// Не доставлено: ответ SMTP, например `550 5.1.1 No such user`; пустой — без подробностей.
    Failed(String),
}

/// Итог [`Outbound::submit`]: статус по каждому получателю в порядке запроса.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Submitted {
    /// Получатель → статус.
    pub rcpts: Vec<(String, SubmitStatus)>,
}

impl Submitted {
    /// Все получатели `rcpts` со статусом `status`.
    pub fn all(rcpts: &[String], status: SubmitStatus) -> Self {
        Submitted {
            rcpts: rcpts.iter().map(|r| (r.clone(), status.clone())).collect(),
        }
    }
}

/// Постановка исходящего письма в очередь: submission SMTP и пересылки модулей (`tp-smtp`), JMAP
/// `EmailSubmission/set` (`tp-jmap`). Реализации — `tp_smtp::queue::Queue` (подпись DKIM при постановке) и
/// `tp_smtp::Delivery` (локальные получатели — сразу в ящики); in-memory — `tp-testkit`.
///
/// Маршрут правила потока (`FlowOutcome::route`) не передаётся: очередь выбирает политику по домену назначения.
///
/// `owner` — учётная запись отправителя организации `tenant` (CTR-74, решение 46): реализация передаёт её в
/// `BlobIndex::acquire` и [`QueueStore::enqueue`] — тело и запись очереди ложатся на шард её ящика. `None` —
/// отправителя-учётной записи нет (DSN, пересылка, письмо модуля).
#[async_trait]
pub trait Outbound: Send + Sync + 'static {
    /// Ставит письмо `raw` (RFC 5322) от `mail_from` получателям `rcpts` организации `tenant`. Ошибка — письмо не
    /// принято (`Unavailable` — повтор безопасен).
    async fn enqueue(
        &self,
        tenant: TenantId,
        owner: Option<AccountId>,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<()>;

    /// Отложенная отправка (CTR-42; JMAP `sendAt`): ставит письмо в очередь со сроком выпуска `release_at`
    /// ([`OutMessage::next_attempt`]) для всех получателей — локальные доставляются при выпуске — и возвращает запись
    /// очереди (`EmailSubmission::queue`). Ошибки — как у [`Outbound::enqueue`].
    async fn schedule(
        &self,
        tenant: TenantId,
        owner: Option<AccountId>,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
        release_at: OffsetDateTime,
    ) -> Result<QueueId>;

    /// Отмена отложенной отправки до выпуска (CTR-42; JMAP `undoStatus: canceled`): [`QueueStore::cancel_queued`] и
    /// освобождение ссылки на тело. Уже выпущено — `Error::Conflict` (`cannotUnsend`); нет записи — `Error::NotFound`.
    /// По умолчанию — `Error::Internal`.
    async fn cancel_scheduled(&self, tenant: TenantId, queue: QueueId) -> Result<()> {
        let _ = (tenant, queue);
        Err(Error::internal(
            "outbound does not support cancel_scheduled",
        ))
    }

    /// Отправляет письмо и возвращает статус по каждому получателю в порядке `rcpts`. По умолчанию —
    /// [`Outbound::enqueue`], все получатели [`SubmitStatus::Queued`].
    async fn submit(
        &self,
        tenant: TenantId,
        owner: Option<AccountId>,
        mail_from: &str,
        rcpts: &[String],
        raw: Bytes,
    ) -> Result<Submitted> {
        self.enqueue(tenant, owner, mail_from, rcpts, raw).await?;
        Ok(Submitted::all(rcpts, SubmitStatus::Queued))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{BlobHash, Gen};
    use std::sync::{Arc, Mutex};

    #[test]
    fn object_safe() {
        fn _queue(_: Arc<dyn QueueStore>) {}
        fn _outbound(_: Arc<dyn Outbound>) {}
    }

    /// `submit` по умолчанию: письмо в `enqueue`, все получатели в очереди в порядке запроса.
    #[tokio::test]
    async fn submit_default_queued() {
        #[derive(Default)]
        struct Plain(Mutex<Vec<(Option<AccountId>, Vec<String>)>>);
        #[async_trait]
        impl Outbound for Plain {
            async fn enqueue(
                &self,
                _: TenantId,
                owner: Option<AccountId>,
                _: &str,
                rcpts: &[String],
                _: Bytes,
            ) -> Result<()> {
                self.0.lock().expect("lock").push((owner, rcpts.to_vec()));
                Ok(())
            }

            async fn schedule(
                &self,
                _: TenantId,
                _: Option<AccountId>,
                _: &str,
                _: &[String],
                _: Bytes,
                _: OffsetDateTime,
            ) -> Result<QueueId> {
                Err(Error::internal("not scheduled"))
            }
        }
        let ob = Plain::default();
        let owner = AccountId::new();
        let rcpts = vec!["b@example.com".to_owned(), "a@example.org".to_owned()];
        let sent = ob
            .submit(
                TenantId::DEFAULT,
                Some(owner),
                "x@example.org",
                &rcpts,
                Bytes::new(),
            )
            .await
            .expect("submit");
        assert_eq!(
            sent.rcpts,
            vec![
                ("b@example.com".to_owned(), SubmitStatus::Queued),
                ("a@example.org".to_owned(), SubmitStatus::Queued),
            ]
        );
        assert_eq!(
            *ob.0.lock().expect("lock"),
            vec![(Some(owner), rcpts.clone())]
        );
        let canceled = ob.cancel_scheduled(TenantId::DEFAULT, QueueId::new()).await;
        assert!(matches!(canceled, Err(Error::Internal(_))));
    }

    /// Фильтр списка очереди (CTR-85): состояние и подстрока адреса отправителя или получателя без учёта регистра.
    #[test]
    fn queue_filter_matches() {
        let item = QueueItem {
            id: QueueId::new(),
            state: QueueState::Retry,
            queued_at: OffsetDateTime::UNIX_EPOCH,
            next_attempt: OffsetDateTime::UNIX_EPOCH,
            from: "Alice@Example.org".into(),
            size: 10,
            rcpts: vec![QueueItemRcpt {
                rcpt: OutRcpt::new("bob@example.net"),
                state: QueueRcptState::Pending {
                    next_attempt: OffsetDateTime::UNIX_EPOCH,
                },
                attempts: 1,
                last_error: Some("451 later".into()),
            }],
        };
        let f = |state: Option<QueueState>, address: Option<&str>| QueueFilter {
            state,
            address: address.map(str::to_owned),
        };
        assert!(QueueFilter::default().matches(&item));
        assert!(f(Some(QueueState::Retry), None).matches(&item));
        assert!(!f(Some(QueueState::Ready), None).matches(&item));
        assert!(f(None, Some("alice@EXAMPLE")).matches(&item), "отправитель");
        assert!(f(None, Some("example.net")).matches(&item), "получатель");
        assert!(f(None, Some("")).matches(&item), "пустая строка");
        assert!(!f(None, Some("carol")).matches(&item));
        assert!(!f(Some(QueueState::Active), Some("bob")).matches(&item));
        let s = serde_json::to_string(&item).expect("serialize");
        assert_eq!(serde_json::from_str::<QueueItem>(&s).expect("parse"), item);
    }

    /// `list` и `retry` по умолчанию — `Internal` (реализация без просмотра очереди).
    #[tokio::test]
    async fn list_retry_default_internal() {
        struct Bare;
        #[async_trait]
        impl QueueStore for Bare {
            async fn enqueue(&self, _: Option<AccountId>, _: OutMessage) -> Result<QueueId> {
                Err(Error::internal("bare"))
            }
            async fn next_due(&self) -> Result<Option<OffsetDateTime>> {
                Ok(None)
            }
            async fn claim(
                &self,
                _: OffsetDateTime,
                _: usize,
                _: Duration,
            ) -> Result<Vec<Claimed>> {
                Ok(Vec::new())
            }
            async fn renew(&self, _: &QueueClaim, _: OffsetDateTime) -> Result<()> {
                Err(Error::Conflict)
            }
            async fn reschedule(
                &self,
                _: &QueueClaim,
                _: &str,
                _: OffsetDateTime,
                _: &str,
            ) -> Result<()> {
                Err(Error::Conflict)
            }
            async fn complete(&self, _: &QueueClaim, _: &str, _: RcptStatus) -> Result<bool> {
                Err(Error::Conflict)
            }
        }
        let now = OffsetDateTime::UNIX_EPOCH;
        let listed = Bare
            .list(TenantId::DEFAULT, &QueueFilter::default(), None, 10, now)
            .await;
        assert!(matches!(listed, Err(Error::Internal(_))));
        let retried = Bare.retry(TenantId::DEFAULT, QueueId::new(), now).await;
        assert!(matches!(retried, Err(Error::Internal(_))));
    }

    #[test]
    fn queue_ready_is_system() {
        let t = TenantId::new();
        let e = queue_ready(t);
        assert_eq!((e.tenant, e.account), (t, None));
        assert!(e.kind.is_system());
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
