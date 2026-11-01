//! Метаданные ящиков, журнал изменений и политики (CONTRACTS.md#Metadata, #ChangeLog, #Tenant; SRS 3.1, 3.6, 3.8).
//!
//! Все операции выполняются в контексте организации: `TenantId` — явный параметр или поле [`AccountScope`];
//! реализация (`tp-meta`) открывает транзакцию через `with_tenant` (RLS по `tenant_id`). Связей между
//! ящиками нет (SRS 3.6): операция затрагивает одну учётную запись.
//!
//! # Инварианты Uid и ModSeq
//!
//! - `Uid` выдаётся при вставке письма в папку: ≥ 1, строго больше всех ранее выданных в этой папке
//!   при том же `UidValidity`, никогда не переиспользуется (в том числе после удаления, копирования, переноса).
//!   Копирование и перенос создают в целевой папке новый `MessageId` и новый `Uid`.
//! - `UidValidity` папки задаётся при создании и меняется, только если прежние UID стали недействительны
//!   (пересоздание папки с тем же именем — новое значение).
//! - `ModSeq` выдаётся из одного счётчика учётной записи: каждое изменение (вставка, флаги, перенос,
//!   удаление, изменение папки) получает новый, строго больший номер. Поэтому `ModSeq` монотонен и в папке
//!   (IMAP CONDSTORE `HIGHESTMODSEQ` = наибольший номер изменений папки), и в учётной записи
//!   (состояние JMAP = наибольший `ModSeq` учётной записи).
//! - Журнал изменений ([`ChangeLog`]) и журнал событий ядра пишутся в той же транзакции, что изменение.

use std::collections::BTreeSet;
use std::ops::RangeInclusive;
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::Result;
use crate::ids::{
    AccountId, BlobKey, DomainId, MailboxId, MessageId, ModSeq, TenantId, Uid, UidValidity,
};
use crate::retention::{TrashPurge, TrashPurgeRule, TrashPurgeSetting};

/// Контекст операций ящика: организация и учётная запись.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AccountScope {
    /// Организация.
    pub tenant: TenantId,
    /// Учётная запись.
    pub account: AccountId,
}

// ---------- Учётные записи ----------

/// Учётная запись.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Идентификатор.
    pub id: AccountId,
    /// Организация.
    pub tenant: TenantId,
    /// Основной домен.
    pub domain: DomainId,
    /// Имя входа (обычно основной адрес), уникально в организации.
    pub login: String,
    /// Отображаемое имя.
    pub display_name: String,
    /// Идентификатор в каталоге (LDAP `objectGUID`/`entryUUID`), если запись из каталога.
    pub external_id: Option<String>,
    /// Вход разрешён.
    pub enabled: bool,
}

/// Данные для создания или обновления учётной записи (синхронизация с каталогом).
/// Сопоставление — по `external_id`, если задан, иначе по `login`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountUpsert {
    /// Основной домен.
    pub domain: DomainId,
    /// Имя входа.
    pub login: String,
    /// Отображаемое имя.
    pub display_name: String,
    /// Идентификатор в каталоге.
    pub external_id: Option<String>,
    /// Вход разрешён.
    pub enabled: bool,
}

/// Использование квоты учётной записи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct QuotaUsage {
    /// Занято байт (сумма размеров писем, включая корзину, без скрытых — `TrashStore::purge`).
    pub used_bytes: u64,
    /// Число писем.
    pub messages: u64,
    /// Лимит из [`PolicyResolver`]; `None` — без ограничения.
    pub limit_bytes: Option<u64>,
}

/// Учётные записи организации.
#[async_trait]
pub trait AccountStore: Send + Sync + 'static {
    /// Учётная запись по идентификатору.
    async fn get(&self, tenant: TenantId, id: AccountId) -> Result<Option<Account>>;

    /// Учётная запись по имени входа (без учёта регистра).
    async fn by_login(&self, tenant: TenantId, login: &str) -> Result<Option<Account>>;

    /// Создаёт или обновляет учётную запись (из LDAP); при создании — системные папки.
    async fn upsert(&self, tenant: TenantId, data: AccountUpsert) -> Result<Account>;

    /// Использование квоты.
    async fn quota(&self, tenant: TenantId, id: AccountId) -> Result<QuotaUsage>;
}

// ---------- Папки ----------

/// Назначение папки (RFC 6154 и `INBOX`; роль JMAP Mailbox).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SpecialUse {
    /// Входящие.
    Inbox,
    /// Архив.
    Archive,
    /// Черновики.
    Drafts,
    /// Нежелательная почта.
    Junk,
    /// Отправленные.
    Sent,
    /// Корзина.
    Trash,
}

/// Папка учётной записи. Иерархия — через `parent`; полный путь IMAP строит протокол.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mailbox {
    /// Идентификатор.
    pub id: MailboxId,
    /// Родительская папка; `None` — верхний уровень.
    pub parent: Option<MailboxId>,
    /// Имя (без разделителя иерархии).
    pub name: String,
    /// Назначение.
    pub role: Option<SpecialUse>,
    /// UIDVALIDITY.
    pub uid_validity: UidValidity,
    /// Подписка IMAP / `isSubscribed` JMAP.
    pub subscribed: bool,
}

/// Данные новой папки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewMailbox {
    /// Родительская папка.
    pub parent: Option<MailboxId>,
    /// Имя.
    pub name: String,
    /// Назначение.
    pub role: Option<SpecialUse>,
}

/// Состояние папки (IMAP STATUS / SELECT, JMAP Mailbox counts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailboxStatus {
    /// Писем в папке.
    pub messages: u32,
    /// Писем без флага `\Seen`.
    pub unseen: u32,
    /// Следующий UID.
    pub uid_next: Uid,
    /// UIDVALIDITY.
    pub uid_validity: UidValidity,
    /// Наибольший `ModSeq` изменений папки.
    pub highest_modseq: ModSeq,
    /// Суммарный размер писем, байт.
    pub size: u64,
}

/// Папки учётной записи. Изменения увеличивают `ModSeq` учётной записи и пишутся в [`ChangeLog`].
#[async_trait]
pub trait MailboxStore: Send + Sync + 'static {
    /// Все папки учётной записи.
    async fn list(&self, scope: AccountScope) -> Result<Vec<Mailbox>>;

    /// Папка по идентификатору.
    async fn get(&self, scope: AccountScope, id: MailboxId) -> Result<Option<Mailbox>>;

    /// Создаёт папку; имя занято у того же родителя — `Error::Conflict`.
    async fn create(&self, scope: AccountScope, data: NewMailbox) -> Result<Mailbox>;

    /// Переименовывает и/или переносит папку.
    async fn rename(
        &self,
        scope: AccountScope,
        id: MailboxId,
        parent: Option<MailboxId>,
        name: &str,
    ) -> Result<()>;

    /// Удаляет папку (письма — в корзину, SRS 4.7); системные папки удалять нельзя — `Error::Forbidden`.
    ///
    /// У папки есть вложенные и `cascade = false` — `Error::Conflict`: клиент (IMAP, JMAP `mailboxHasChild`,
    /// веб-интерфейс, Admin API) предупреждает пользователя и после подтверждения повторяет вызов
    /// с `cascade = true`. Тогда удаляется всё поддерево одной транзакцией, письма всех его папок — в корзину;
    /// системная папка в поддереве — `Error::Forbidden`, ничего не удаляется.
    async fn delete(&self, scope: AccountScope, id: MailboxId, cascade: bool) -> Result<()>;

    /// Подписка IMAP.
    async fn set_subscribed(&self, scope: AccountScope, id: MailboxId, on: bool) -> Result<()>;

    /// Счётчики, UIDNEXT, наибольший MODSEQ.
    async fn status(&self, scope: AccountScope, id: MailboxId) -> Result<MailboxStatus>;
}

// ---------- Письма ----------

/// Флаг письма (IMAP system flags и keywords; JMAP keywords).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Flag {
    /// `\Seen` / `$seen`.
    Seen,
    /// `\Answered` / `$answered`.
    Answered,
    /// `\Flagged` / `$flagged`.
    Flagged,
    /// `\Deleted` (только IMAP).
    Deleted,
    /// `\Draft` / `$draft`.
    Draft,
    /// Пользовательское ключевое слово (`$Forwarded`, `$Junk`, ...), регистр сохраняется.
    Keyword(String),
}

/// Набор флагов.
pub type Flags = BTreeSet<Flag>;

/// Изменение флагов.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FlagOp {
    /// Добавить.
    Add(Flags),
    /// Снять.
    Remove(Flags),
    /// Заменить набор целиком.
    Replace(Flags),
}

/// Адрес электронной почты с именем.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Address {
    /// Отображаемое имя (декодированное).
    pub name: Option<String>,
    /// Адрес `local@domain`; для групп RFC 5322 — имя группы без адреса не хранится.
    pub email: String,
}

/// Конверт IMAP (RFC 9051 ENVELOPE), вычисляется при приёме.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Envelope {
    /// Заголовок `Date`.
    pub date: Option<OffsetDateTime>,
    /// Тема (декодированная).
    pub subject: Option<String>,
    /// `From`.
    pub from: Vec<Address>,
    /// `Sender`.
    pub sender: Vec<Address>,
    /// `Reply-To`.
    pub reply_to: Vec<Address>,
    /// `To`.
    pub to: Vec<Address>,
    /// `Cc`.
    pub cc: Vec<Address>,
    /// `Bcc`.
    pub bcc: Vec<Address>,
    /// `In-Reply-To`.
    pub in_reply_to: Option<String>,
    /// `Message-ID`.
    pub message_id: Option<String>,
}

/// Узел структуры MIME (IMAP BODYSTRUCTURE, JMAP bodyStructure).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BodyPart {
    /// Путь части IMAP (`"1.2"`); корень — `""`.
    pub part: String,
    /// Тип, например `text`.
    pub media_type: String,
    /// Подтип, например `plain`.
    pub subtype: String,
    /// Параметры `Content-Type` (`charset`, `boundary`, `name`, ...).
    pub params: Vec<(String, String)>,
    /// `Content-ID`.
    pub content_id: Option<String>,
    /// `Content-Description`.
    pub description: Option<String>,
    /// `Content-Transfer-Encoding`.
    pub encoding: Option<String>,
    /// `Content-Disposition` (`inline`/`attachment`) с параметрами.
    pub disposition: Option<(String, Vec<(String, String)>)>,
    /// `Content-Language`.
    pub language: Vec<String>,
    /// `Content-Location`.
    pub location: Option<String>,
    /// Размер тела части в закодированном виде, байт.
    pub size: u64,
    /// Число строк (для `text/*` и `message/rfc822`).
    pub lines: Option<u32>,
    /// Вложенные части (`multipart/*`, `message/rfc822`).
    pub children: Vec<BodyPart>,
}

/// Вычисленные при приёме метаданные письма.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct MessageMeta {
    /// Конверт.
    pub envelope: Envelope,
    /// Структура MIME.
    pub body: BodyPart,
    /// Начало текста для списка писем (JMAP `preview`, ≤ 256 символов).
    pub preview: String,
    /// Ключ цепочки (по `References`/`In-Reply-To`/теме), одинаков у писем одной цепочки в учётной записи.
    pub thread_key: String,
    /// Есть вложения.
    pub has_attachment: bool,
}

/// Ссылка на объект части письма (модель «скелет + части»).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartRef {
    /// Путь части IMAP (`"1.2"`), чьё тело хранится отдельным объектом.
    pub part: String,
    /// Объект с телом части.
    pub blob: BlobKey,
    /// Размер объекта, байт.
    pub size: u64,
}

/// Новое письмо: объекты уже загружены и учтены в `BlobIndex` (SRS 3.4, шаг 4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewMessage {
    /// Объект-скелет (заголовки, границы MIME, ссылки на части).
    pub skeleton: BlobKey,
    /// Объекты частей.
    pub parts: Vec<PartRef>,
    /// Размер исходного письма, байт (RFC822.SIZE).
    pub size: u64,
    /// Момент приёма (INTERNALDATE, `receivedAt`).
    pub received_at: OffsetDateTime,
    /// Начальные флаги.
    pub flags: Flags,
    /// Вычисленные метаданные.
    pub meta: MessageMeta,
}

/// Письмо в папке.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Идентификатор.
    pub id: MessageId,
    /// Папка.
    pub mailbox: MailboxId,
    /// UID в папке.
    pub uid: Uid,
    /// `ModSeq` последнего изменения письма.
    pub modseq: ModSeq,
    /// Размер исходного письма.
    pub size: u64,
    /// Момент приёма.
    pub received_at: OffsetDateTime,
    /// Флаги.
    pub flags: Flags,
    /// Объект-скелет.
    pub skeleton: BlobKey,
    /// Объекты частей.
    pub parts: Vec<PartRef>,
    /// Метаданные.
    pub meta: MessageMeta,
}

/// Результат вставки, копирования, переноса.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inserted {
    /// Идентификатор письма в папке назначения: новый у `insert`, `copy`, `move_to`;
    /// у `restore` — прежний (корзина сохраняет `MessageId`).
    pub id: MessageId,
    /// Новый UID в папке назначения.
    pub uid: Uid,
    /// `ModSeq` изменения.
    pub modseq: ModSeq,
}

/// Выбор писем папки для [`MessageStore::list`]. Результат упорядочен по `Uid`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MessageQuery {
    /// Все письма.
    All,
    /// Диапазоны UID (sequence set IMAP), границы включительно.
    Uids(Vec<RangeInclusive<Uid>>),
    /// Конкретные письма.
    Ids(Vec<MessageId>),
    /// Изменённые после `ModSeq` (CONDSTORE `CHANGEDSINCE`).
    ChangedSince(ModSeq),
}

/// Результат [`MessageStore::set_flags`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FlagsUpdate {
    /// `ModSeq` изменения (если ничего не изменено — текущий).
    pub modseq: ModSeq,
    /// Изменённые письма.
    pub changed: Vec<MessageId>,
    /// Не изменены из-за `unchanged_since` (CONDSTORE `MODIFIED`).
    pub conflicts: Vec<MessageId>,
}

/// Письма учётной записи. Каждая изменяющая операция — одна транзакция: новый `ModSeq`,
/// запись в [`ChangeLog`] и событие ядра (`EventLog`). Объекты BLOB не трогаются, кроме учёта ссылок
/// в той же базе (`expunge_final` уменьшает счётчики).
#[async_trait]
pub trait MessageStore: Send + Sync + 'static {
    /// Вставляет письмо в папку.
    async fn insert(
        &self,
        scope: AccountScope,
        mailbox: MailboxId,
        msg: NewMessage,
    ) -> Result<Inserted>;

    /// Вставляет письма всех доставок одного приёма одной транзакцией (решение 2026-10-31-atomic-delivery):
    /// вставлены все или ни одного; события доставки — в той же транзакции. Результат — в порядке `items`.
    /// `scope.tenant` элемента не равен `tenant` — `Error::Invalid`; нет учётной записи или папки — `Error::NotFound`.
    async fn insert_batch(
        &self,
        tenant: TenantId,
        items: Vec<(AccountScope, MailboxId, NewMessage)>,
    ) -> Result<Vec<Inserted>>;

    /// Письмо по идентификатору.
    async fn get(&self, scope: AccountScope, id: MessageId) -> Result<Option<Message>>;

    /// Письма папки по выбору, не более `limit`, по возрастанию `Uid`.
    async fn list(
        &self,
        scope: AccountScope,
        mailbox: MailboxId,
        query: MessageQuery,
        limit: usize,
    ) -> Result<Vec<Message>>;

    /// Изменяет флаги; с `unchanged_since` письма с большим `ModSeq` не меняются (CONDSTORE).
    async fn set_flags(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        op: FlagOp,
        unchanged_since: Option<ModSeq>,
    ) -> Result<FlagsUpdate>;

    /// Копирует письма в папку (объекты BLOB общие: ссылки добавляются).
    async fn copy(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        to: MailboxId,
    ) -> Result<Vec<Inserted>>;

    /// Переносит письма в папку (новые `MessageId` и `Uid`, исходные удаляются).
    async fn move_to(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        to: MailboxId,
    ) -> Result<Vec<Inserted>>;

    /// Переносит письма в корзину (SRS 4.7): `MessageId` сохраняется, меняются папка и UID;
    /// ссылки на объекты сохраняются. Отсутствующие и уже лежащие в корзине письма пропускаются.
    async fn to_trash(&self, scope: AccountScope, ids: &[MessageId]) -> Result<ModSeq>;

    /// Восстанавливает письма из корзины в исходные папки (папки нет — во «Входящие») с тем же `MessageId`
    /// и новым UID. Письмо не в корзине — `Error::Invalid`.
    async fn restore(&self, scope: AccountScope, ids: &[MessageId]) -> Result<Vec<Inserted>>;

    /// Окончательно удаляет письма, которые в момент вызова лежат в корзине, и уменьшает счётчики ссылок
    /// объектов (SRS 3.4, удаление шаг 2). Отсутствующие и письма вне корзины (например, восстановленные
    /// между `TrashStore::expired` и этим вызовом) пропускаются без ошибки.
    async fn expunge_final(&self, scope: AccountScope, ids: &[MessageId]) -> Result<ModSeq>;
}

// ---------- Журнал изменений ----------

/// Изменённое письмо в журнале.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MessageChange {
    /// Письмо.
    pub id: MessageId,
    /// Папка письма.
    pub mailbox: MailboxId,
    /// UID (для IMAP QRESYNC `VANISHED`).
    pub uid: Uid,
    /// `ModSeq` изменения.
    pub modseq: ModSeq,
}

/// Изменения после состояния `since` (JMAP `/changes`, IMAP QRESYNC).
/// Объект, созданный и удалённый в интервале, попадает только в `destroyed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes<T> {
    /// Созданные.
    pub created: Vec<T>,
    /// Изменённые.
    pub updated: Vec<T>,
    /// Удалённые.
    pub destroyed: Vec<T>,
    /// Состояние после возвращённых изменений.
    pub new_state: ModSeq,
    /// Возвращены не все изменения (`limit`): повторить с `since = new_state`.
    pub has_more: bool,
}

/// Журнал изменений учётной записи. Пишется в той же транзакции, что изменение.
/// `since` старше хранимого журнала — `Error::Conflict` (JMAP `cannotCalculateChanges`, полная ресинхронизация).
#[async_trait]
pub trait ChangeLog: Send + Sync + 'static {
    /// Изменения писем учётной записи или одной папки после `since`, не более `limit`.
    async fn message_changes(
        &self,
        scope: AccountScope,
        mailbox: Option<MailboxId>,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<MessageChange>>;

    /// Изменения папок после `since`, не более `limit`.
    async fn mailbox_changes(
        &self,
        scope: AccountScope,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<MailboxId>>;
}

// ---------- Политики ----------

/// Политики, действующие для объекта (SRS 3.8). `None` — не задано ни на одном уровне цепочки:
/// действует значение по умолчанию из конфигурации крейта-потребителя.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Policy {
    /// Квота ящика, байт.
    pub quota_bytes: Option<u64>,
    /// Срок хранения в корзине.
    pub trash_retention: Option<Duration>,
    /// Срок хранения писем (политика хранения; разрешается до папки).
    pub retention: Option<Duration>,
    /// Лимит отправки, писем в час.
    pub send_per_hour: Option<u32>,
    /// Лимит получателей одного письма.
    pub max_recipients: Option<u32>,
    /// Селектор ключа DKIM домена.
    pub dkim_selector: Option<String>,
    /// Профиль Milter.
    pub milter_profile: Option<String>,
    /// Правило администратора `trash.purge` (решение 2026-11-01-trash-purge); вычисление с выбором пользователя —
    /// [`PolicyResolver::trash_purge`].
    pub trash_purge: Option<TrashPurgeRule>,
}

impl Policy {
    /// Накладывает более специфичный уровень: заданные в `over` поля заменяют поля `self`.
    pub fn overlay(self, over: Policy) -> Policy {
        Policy {
            quota_bytes: over.quota_bytes.or(self.quota_bytes),
            trash_retention: over.trash_retention.or(self.trash_retention),
            retention: over.retention.or(self.retention),
            send_per_hour: over.send_per_hour.or(self.send_per_hour),
            max_recipients: over.max_recipients.or(self.max_recipients),
            dkim_selector: over.dkim_selector.or(self.dkim_selector),
            milter_profile: over.milter_profile.or(self.milter_profile),
            trash_purge: over.trash_purge.or(self.trash_purge),
        }
    }
}

/// Разрешение политик по цепочке «организация → домен → ящик (→ папка)»: каждый следующий уровень
/// переопределяет заданные поля предыдущего ([`Policy::overlay`]). Настройка `trash.purge` — правило администратора
/// по цепочке и выбор пользователя ([`PolicyResolver::trash_purge`]; администратор приоритетнее).
#[async_trait]
pub trait PolicyResolver: Send + Sync + 'static {
    /// Действующая политика. `domain`/`account`/`mailbox` = `None` — разрешение до предыдущего уровня.
    async fn resolve(
        &self,
        tenant: TenantId,
        domain: Option<DomainId>,
        account: Option<AccountId>,
        mailbox: Option<MailboxId>,
    ) -> Result<Policy>;

    /// Настройка `trash.purge` учётной записи: правило администратора по цепочке «организация → домен → ящик»
    /// (`Policy::trash_purge`, не задано — `TrashPurgeRule::default()`) и выбор пользователя; действующее значение —
    /// [`TrashPurgeSetting::effective`]. Нет учётной записи — `Error::NotFound`.
    async fn trash_purge(&self, scope: AccountScope) -> Result<TrashPurgeSetting>;

    /// Задаёт выбор пользователя `trash.purge` (`None` — сбросить, действует значение администратора).
    /// Выбор при действующем правиле без `user_can_choose` — `Error::Forbidden` (сброс разрешён всегда);
    /// нет учётной записи — `Error::NotFound`.
    async fn set_user_trash_purge(
        &self,
        scope: AccountScope,
        mode: Option<TrashPurge>,
    ) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::{BlobHash, Gen};
    use std::sync::Arc;

    #[test]
    fn object_safe() {
        fn _a(_: Arc<dyn AccountStore>) {}
        fn _m(_: Arc<dyn MailboxStore>) {}
        fn _s(_: Arc<dyn MessageStore>) {}
        fn _c(_: Arc<dyn ChangeLog>) {}
        fn _p(_: Arc<dyn PolicyResolver>) {}
    }

    #[test]
    fn new_message_serde() {
        let key = |n| BlobKey {
            tenant: TenantId::DEFAULT,
            hash: BlobHash([n; 32]),
            generation: Gen::INITIAL,
        };
        let msg = NewMessage {
            skeleton: key(1),
            parts: vec![PartRef {
                part: "2".into(),
                blob: key(2),
                size: 10,
            }],
            size: 100,
            received_at: OffsetDateTime::UNIX_EPOCH,
            flags: [Flag::Seen, Flag::Keyword("$Forwarded".into())].into(),
            meta: MessageMeta {
                envelope: Envelope {
                    subject: Some("Тема".into()),
                    from: vec![Address {
                        name: None,
                        email: "a@example.org".into(),
                    }],
                    ..Envelope::default()
                },
                preview: "текст".into(),
                ..MessageMeta::default()
            },
        };
        let s = serde_json::to_string(&msg).expect("serialize");
        assert_eq!(serde_json::from_str::<NewMessage>(&s).expect("parse"), msg);
        let q = MessageQuery::Uids(vec![Uid(1)..=Uid(5)]);
        let s = serde_json::to_string(&q).expect("serialize");
        assert_eq!(serde_json::from_str::<MessageQuery>(&s).expect("parse"), q);
    }

    #[test]
    fn policy_chain() {
        let tenant = Policy {
            quota_bytes: Some(10),
            send_per_hour: Some(100),
            ..Policy::default()
        };
        let domain = Policy {
            quota_bytes: Some(20),
            dkim_selector: Some("s1".into()),
            ..Policy::default()
        };
        let mailbox = Policy {
            send_per_hour: Some(5),
            ..Policy::default()
        };
        let p = tenant.overlay(domain).overlay(mailbox);
        assert_eq!(p.quota_bytes, Some(20));
        assert_eq!(p.send_per_hour, Some(5));
        assert_eq!(p.dkim_selector.as_deref(), Some("s1"));
        assert_eq!(p.milter_profile, None);
        assert_eq!(p.trash_purge, None);
        let rule = TrashPurgeRule {
            mode: TrashPurge::Erase,
            user_can_choose: true,
        };
        let over = Policy {
            trash_purge: Some(rule),
            ..Policy::default()
        };
        assert_eq!(p.clone().overlay(over.clone()).trash_purge, Some(rule));
        assert_eq!(over.overlay(p).trash_purge, Some(rule));
    }
}
