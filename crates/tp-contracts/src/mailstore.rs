//! Высокоуровневый API почтового хранилища (CONTRACTS.md#MailStore; SRS 3.1–3.4).
//!
//! Реализация — `tp-mailstore` (полоса mailstore) поверх [`BlobStore`](crate::blob::BlobStore),
//! [`BlobIndex`](crate::blob::BlobIndex) и хранилищ метаданных; протоколы (smtp, imap, jmap, admin) работают через него.
//!
//! Модель «скелет + части»: письмо = объект-скелет (заголовки, границы MIME, ссылки на части) + объекты частей.
//! Исходное письмо восстанавливается из них побайтово. Части дедуплицируются в пределах организации:
//! [`MailStore::ingest`] загружает каждую часть один раз на всех получателей (`refs` = число доставок).
//! Письмо, которое кладёт клиент (IMAP APPEND, JMAP `Email/import`), — не доставка: [`MailStore::append`]
//! (решение 2026-11-01-trash-purge).
//!
//! Изменяющие операции публикуют уведомление в [`EventBus`](crate::bus::EventBus) после фиксации транзакции.

use std::fmt;
use std::ops::Range;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::blob::ByteStream;
use crate::error::Result;
use crate::ids::{AccountId, MailboxId, MessageId, ModSeq, TenantId, Uid};
use crate::meta::{AccountScope, FlagOp, Flags, FlagsUpdate, Inserted};

/// Доставка письма в одну папку одной учётной записи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivery {
    /// Учётная запись получателя.
    pub account: AccountId,
    /// Папка (выбрана Sieve или по умолчанию — «Входящие»).
    pub mailbox: MailboxId,
    /// Заголовки, добавляемые перед письмом только для этого получателя (`Delivered-To`, результат Sieve).
    /// Входят в скелет этой копии; части остаются общими.
    pub extra_headers: Vec<(String, String)>,
    /// Начальные флаги.
    pub flags: Flags,
}

/// Запрос приёма письма.
pub struct IngestRequest {
    /// Организация: дедупликация и все доставки — в её пределах.
    pub tenant: TenantId,
    /// Отправитель конверта (SMTP `MAIL FROM`); пустой — уведомление о недоставке.
    /// Хранилище не выводит его из заголовков письма (`Return-Path`).
    pub mail_from: String,
    /// Исходное письмо (RFC 5322), читается один раз потоково.
    pub raw: ByteStream,
    /// Доставки; пустой список — `Error::Invalid`.
    pub deliveries: Vec<Delivery>,
    /// Момент приёма (INTERNALDATE, `receivedAt`).
    pub received_at: OffsetDateTime,
}

impl fmt::Debug for IngestRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Содержимое письма в отладочный вывод не попадает.
        f.debug_struct("IngestRequest")
            .field("tenant", &self.tenant)
            .field("mail_from", &self.mail_from)
            .field("deliveries", &self.deliveries)
            .field("received_at", &self.received_at)
            .finish_non_exhaustive()
    }
}

/// Результат одной доставки (в порядке `IngestRequest::deliveries`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Delivered {
    /// Учётная запись.
    pub account: AccountId,
    /// Папка.
    pub mailbox: MailboxId,
    /// Письмо.
    pub message: MessageId,
    /// UID в папке.
    pub uid: Uid,
}

/// Результат приёма письма.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ingested {
    /// Доставки в порядке `IngestRequest::deliveries` (копии в папки от точек принятия решений не входят).
    pub delivered: Vec<Delivered>,
    /// Внешние адреса для пересылки копии (`CopyTo::Address` от точек принятия решений).
    /// В очередь исходящей почты их ставит вызывающий: хранилище почту не отправляет.
    pub forward: Vec<String>,
}

/// Почтовое хранилище: приём, чтение и изменение писем.
///
/// Операции, кроме `ingest`, — в контексте одной учётной записи ([`AccountScope`]).
/// Объекты BLOB загружаются вне транзакций БД (протокол SRS 3.4, см. [`crate::blob`]).
#[async_trait]
pub trait MailStore: Send + Sync + 'static {
    /// Принимает письмо: разбор, загрузка частей (одна на всех получателей), метаданные каждой доставки.
    /// Превышение квоты одного получателя — `Error::QuotaExceeded` для всего запроса
    /// (вызывающий исключает получателя и повторяет).
    async fn ingest(&self, req: IngestRequest) -> Result<Ingested>;

    /// Кладёт письмо клиента в папку учётной записи (IMAP APPEND, JMAP `Email/import`; решение 2026-11-01-trash-purge):
    /// разбор и загрузка объектов, как у `ingest`, квота (`Error::QuotaExceeded` до загрузки), `MessageStore::insert`,
    /// уведомление в шину. Это не доставка: точки принятия решений доставки (`DecisionHook::on_delivery`) не вызываются,
    /// копий (журнал, пересылка) нет, заголовки не добавляются — письмо хранится побайтово. Нет папки — `Error::NotFound`.
    async fn append(
        &self,
        scope: AccountScope,
        mailbox: MailboxId,
        raw: ByteStream,
        flags: Flags,
        received_at: OffsetDateTime,
    ) -> Result<Delivered>;

    /// Исходное письмо побайтово или диапазон его байтов (IMAP `BODY[]<partial>`, JMAP blob).
    async fn fetch_raw(
        &self,
        scope: AccountScope,
        message: MessageId,
        range: Option<Range<u64>>,
    ) -> Result<ByteStream>;

    /// Тело части MIME по пути IMAP (`"1.2"`) в кодировке передачи, как в письме.
    async fn fetch_part(
        &self,
        scope: AccountScope,
        message: MessageId,
        part_path: &str,
    ) -> Result<ByteStream>;

    /// Изменяет флаги; с `unchanged_since` — условно (CONDSTORE).
    async fn set_flags(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        op: FlagOp,
        unchanged_since: Option<ModSeq>,
    ) -> Result<FlagsUpdate>;

    /// Копирует письма в папку; объекты общие (ссылки добавляются).
    async fn copy(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        to: MailboxId,
    ) -> Result<Vec<Inserted>>;

    /// Переносит письма в папку.
    async fn move_to(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        to: MailboxId,
    ) -> Result<Vec<Inserted>>;

    /// Удаляет письма в корзину (SRS 4.7); окончательное удаление — maintenance по сроку корзины.
    async fn delete_to_trash(&self, scope: AccountScope, ids: &[MessageId]) -> Result<ModSeq>;

    /// Восстанавливает письма из корзины в исходные папки.
    async fn restore(&self, scope: AccountScope, ids: &[MessageId]) -> Result<Vec<Inserted>>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn object_safe() {
        fn _store(_: Arc<dyn MailStore>) {}
    }

    #[test]
    fn ingest_request_debug_hides_body() {
        let req = IngestRequest {
            tenant: TenantId::DEFAULT,
            mail_from: "sender@example.org".into(),
            raw: Box::pin(futures::stream::iter([Ok(bytes::Bytes::from_static(
                b"Subject: secret",
            ))])),
            deliveries: vec![],
            received_at: OffsetDateTime::UNIX_EPOCH,
        };
        let s = format!("{req:?}");
        assert!(s.contains("IngestRequest") && !s.contains("secret"));
    }
}
