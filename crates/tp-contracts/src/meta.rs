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

use crate::bus::{Event, EventKind};
use crate::error::{Error, Result};
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

/// Состояние ящика учётной записи (CTR-35, решение 2026-11-04-exchange-parity «Удалённые ящики»).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum AccountState {
    /// Действует.
    Active,
    /// Отключён: учётная запись исчезла из каталога (или отключена администратором). Вход запрещён и почта не
    /// принимается независимо от `Account::enabled`, ящика нет в GAL; ящик хранится до `until`, затем удаляется
    /// окончательно (через `DecisionHooks::before_final_delete`), до срока — восстанавливается, подключается к другой
    /// учётной записи или выгружается.
    Disabled {
        /// Срок хранения отключённого ящика (по умолчанию — 30 дней от отключения, политика синхронизации).
        until: OffsetDateTime,
    },
}

/// Что сделать с содержимым ящика учётной записи при подключении к ней отключённого ящика
/// ([`AccountStore::attach_account`]; CTR-50, решение 44). Выбирает администратор.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExistingContent {
    /// Оставить: письма ящика учётной записи переносятся в подключаемый ящик. Письмо системной папки — в папку
    /// подключаемого ящика с той же ролью, остальные папки переносятся с иерархией; имя, занятое у того же
    /// родителя (без учёта регистра ASCII), получает наименьший свободный суффикс: «Проекты 1», «Проекты 2»…
    /// Письма корзины остаются в корзине со своими сроками, скрытые — скрытыми.
    Keep,
    /// Удалить: письма ящика учётной записи переносятся в корзину подключаемого ящика со сроком корзины по
    /// политике (как `MessageStore::to_trash`; уже лежавшие в корзине — со своими сроками), папки не переносятся.
    /// До срока письма восстанавливаются (`MessageStore::restore`): в папку с той же ролью, иначе во «Входящие».
    Delete,
}

/// Отключённый ящик со сроком хранения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisabledAccount {
    /// Учётная запись.
    pub account: Account,
    /// Срок хранения.
    pub until: OffsetDateTime,
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

/// Сигнал «вход учётной записи запрещён» (системный, `account = None`, [`EventKind::AccountDisabled`]; CTR-64).
/// Публикует реализация [`AccountStore`] после фиксации `enabled = false` (`upsert` разрешённой ранее записи) и
/// [`AccountStore::disable_account`]; протоколы с долгими сеансами (IMAP, JMAP) закрывают сеансы учётной записи.
pub fn account_disabled(scope: AccountScope) -> Event {
    Event {
        tenant: scope.tenant,
        account: None,
        kind: EventKind::AccountDisabled {
            account: scope.account,
        },
    }
}

/// Учётные записи организации.
#[async_trait]
pub trait AccountStore: Send + Sync + 'static {
    /// Учётная запись по идентификатору.
    async fn get(&self, tenant: TenantId, id: AccountId) -> Result<Option<Account>>;

    /// Учётные записи по списку идентификаторов (CTR-63; имена участников группы, Admin API): в порядке `ids`,
    /// не найденные (и чужой организации) пропускаются, повтор идентификатора — одна запись (первое вхождение).
    /// По умолчанию — [`AccountStore::get`] по одному; реализация одним запросом — `tp-meta` (META-66).
    async fn by_ids(&self, tenant: TenantId, ids: &[AccountId]) -> Result<Vec<Account>> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::with_capacity(ids.len());
        for &id in ids {
            if seen.insert(id)
                && let Some(a) = self.get(tenant, id).await?
            {
                out.push(a);
            }
        }
        Ok(out)
    }

    /// Учётная запись по имени входа (без учёта регистра).
    async fn by_login(&self, tenant: TenantId, login: &str) -> Result<Option<Account>>;

    /// Учётная запись по идентификатору в каталоге (`Account::external_id`, точное совпадение; CTR-32). Синхронизация
    /// каталога сопоставляет по нему участников групп и учётные записи: переименование входа не меняет `AccountId`.
    /// По умолчанию — `Error::Internal` (реализации — `tp-meta`, `tp-testkit`).
    async fn by_external_id(&self, tenant: TenantId, external_id: &str) -> Result<Option<Account>> {
        let _ = (tenant, external_id);
        Err(Error::Internal(
            "AccountStore::by_external_id не реализован".to_owned(),
        ))
    }

    /// Учётные записи организации постранично (CTR-44, список Admin API): по возрастанию имени входа в нижнем
    /// регистре (порядок сравнения строк хранилища), имя входа в нижнем регистре начинается с `prefix` (нижний
    /// регистр; пусто — все) и строго больше `after` (нижний регистр; `None` — с начала); не более `limit`.
    /// Курсор следующей страницы — имя входа последней записи в нижнем регистре.
    /// По умолчанию — `Error::Internal` (реализации — `tp-meta`, `tp-testkit`).
    async fn list(
        &self,
        tenant: TenantId,
        prefix: &str,
        after: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Account>> {
        let _ = (tenant, prefix, after, limit);
        Err(Error::Internal(
            "AccountStore::list не реализован".to_owned(),
        ))
    }

    /// Создаёт или обновляет учётную запись (из LDAP); при создании — системные папки.
    async fn upsert(&self, tenant: TenantId, data: AccountUpsert) -> Result<Account>;

    /// Использование квоты.
    async fn quota(&self, tenant: TenantId, id: AccountId) -> Result<QuotaUsage>;

    /// Состояние ящика (CTR-35); нет учётной записи — `Error::NotFound`. По умолчанию — через [`AccountStore::get`],
    /// всегда [`AccountState::Active`] (хранилище без отключения ящиков).
    async fn account_state(&self, tenant: TenantId, id: AccountId) -> Result<AccountState> {
        match self.get(tenant, id).await? {
            Some(_) => Ok(AccountState::Active),
            None => Err(Error::NotFound),
        }
    }

    /// Отключает ящик до `until` (повторно — срок заменяется); `upsert` состояние не меняет. Нет учётной записи —
    /// `Error::NotFound`. По умолчанию — `Error::Internal`.
    async fn disable_account(
        &self,
        tenant: TenantId,
        id: AccountId,
        until: OffsetDateTime,
    ) -> Result<()> {
        let _ = (tenant, id, until);
        Err(Error::internal(
            "AccountStore::disable_account не реализован",
        ))
    }

    /// Восстанавливает отключённый ящик ([`AccountState::Active`]; действующий — без изменений). Нет учётной записи —
    /// `Error::NotFound`. По умолчанию — `Error::Internal`.
    async fn restore_account(&self, tenant: TenantId, id: AccountId) -> Result<()> {
        let _ = (tenant, id);
        Err(Error::internal(
            "AccountStore::restore_account не реализован",
        ))
    }

    /// Отключённые ящики организации по (`until`, `id`) — список Admin API. По умолчанию — `Error::Internal`.
    async fn disabled_accounts(&self, tenant: TenantId) -> Result<Vec<DisabledAccount>> {
        let _ = tenant;
        Err(Error::internal(
            "AccountStore::disabled_accounts не реализован",
        ))
    }

    /// Отключённые ящики всех организаций со сроком хранения не позже `now`, по (`until`, `id`), не более `limit` —
    /// окончательное удаление (роль maintenance). По умолчанию — `Error::Internal`.
    async fn expired_accounts(
        &self,
        now: OffsetDateTime,
        limit: usize,
    ) -> Result<Vec<AccountScope>> {
        let _ = (now, limit);
        Err(Error::internal(
            "AccountStore::expired_accounts не реализован",
        ))
    }

    /// Подключает отключённый ящик `disabled` к учётной записи `to` (Connect-Mailbox Exchange): `disabled` получает
    /// домен, вход, имя, `external_id` и `enabled` учётной записи `to` и становится действующим (папки, письма, права —
    /// прежние), запись `to` удаляется. Письма ящика `to` (в том числе в корзине и скрытые) — по режиму `existing`
    /// ([`ExistingContent`]; CTR-50, решение 44): переносятся в подключаемый ящик или в его корзину; перенос — часть
    /// операции (ссылки на объекты BLOB организации не меняются), долгую операцию над большим ящиком Admin API ведёт
    /// фоновой задачей (ADM-10). Без режима (`None`) ящик `to` должен быть пуст, иначе — `Error::Invalid` с подсказкой
    /// выбрать режим. `disabled` действует — `Error::Invalid`; `to` отключена — `Error::Conflict`; нет любой из
    /// записей — `Error::NotFound`. По умолчанию — `Error::Internal`.
    async fn attach_account(
        &self,
        tenant: TenantId,
        disabled: AccountId,
        to: AccountId,
        existing: Option<ExistingContent>,
    ) -> Result<Account> {
        let _ = (tenant, disabled, to, existing);
        Err(Error::internal(
            "AccountStore::attach_account не реализован",
        ))
    }

    /// Окончательно удаляет отключённый ящик (учётную запись и папки) после удаления писем вызывающим (`MailStore`,
    /// освобождение ссылок BLOB). Действующий — `Error::Invalid`; остались письма — `Error::Conflict`; нет —
    /// `Error::NotFound`. По умолчанию — `Error::Internal`.
    async fn remove_disabled(&self, tenant: TenantId, id: AccountId) -> Result<()> {
        let _ = (tenant, id);
        Err(Error::internal(
            "AccountStore::remove_disabled не реализован",
        ))
    }
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
    /// Идентификатор письма в папке назначения: новый у `insert`, `copy`;
    /// у `move_to` и `restore` — прежний (перенос и корзина сохраняют `MessageId`, CTR-29).
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

    /// Переносит письма в папку: `MessageId` сохраняется (JMAP: id письма неизменен, RFC 8621), меняются папка,
    /// UID и `ModSeq`; ссылки на объекты не меняются (CTR-29). Результат — в порядке первого появления в `ids`;
    /// нет письма — `Error::NotFound`. Событие ядра — `Moved { from_message: id, message: id, .. }`.
    async fn move_to(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        to: MailboxId,
    ) -> Result<Vec<Inserted>>;

    /// Переносит письма в корзину (SRS 4.7): `MessageId` сохраняется, меняются папка и UID;
    /// ссылки на объекты сохраняются. Флаг `Flag::Deleted` снимается, прочие флаги — прежние (CTR-33: письмо,
    /// удалённое EXPUNGE из другой папки, не стирается первым же EXPUNGE в корзине). Отсутствующие и уже лежащие
    /// в корзине письма пропускаются (их флаги не меняются).
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

/// Политики, действующие для объекта (SRS 3.8). `None` — не задано ни на одном уровне цепочки: действует
/// умолчание, указанное у поля. У полей SMTP умолчание уровня организации — константы `DEFAULT_*` и методы
/// `*_or_default`, настроек этих полей в config.yaml нет (решение 97). У каждого поля указан потребитель.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Policy {
    /// Квота ящика, байт. Потребитель — `tp-meta` (`QuotaUsage::limit_bytes`); не задано — значение секции
    /// `meta.retention` (без него — без ограничения).
    pub quota_bytes: Option<u64>,
    /// Срок хранения в корзине. Потребитель — `tp-meta` (срок письма в корзине); не задано — значение секции
    /// `meta.retention`.
    pub trash_retention: Option<Duration>,
    /// Срок хранения писем (политика хранения; разрешается до папки). Исполнитель — модуль «Соответствие
    /// требованиям» (удаление по сроку через `DecisionHook::before_final_delete`, CONTRACTS.md#Trash); ядро поле
    /// не применяет. Не задано — письма хранятся без срока.
    pub retention: Option<Duration>,
    /// Лимит отправки (submission), писем в час на учётную запись. Потребитель — SMTP (SMTP-40); не задано —
    /// [`Policy::DEFAULT_SEND_PER_HOUR`].
    pub send_per_hour: Option<u32>,
    /// Лимит отправки, адресатов в час на учётную запись (скользящий час, сумма получателей всех писем; как
    /// RecipientRateLimit в Exchange — решение 113). Потребитель — SMTP (SMTP-43); не задано —
    /// [`Policy::DEFAULT_RCPTS_PER_HOUR`].
    pub rcpts_per_hour: Option<u32>,
    /// Лимит получателей одного письма (`RCPT TO` сеанса submission). Потребитель — SMTP (SMTP-40); не задано —
    /// [`Policy::DEFAULT_MAX_RECIPIENTS`].
    pub max_recipients: Option<u32>,
    /// Селектор ключа DKIM домена отправителя (ключи — ссылки на секреты в `smtp.dkim`). Потребитель — SMTP
    /// (SMTP-40); не задано — письмо подписывается всеми ключами домена.
    pub dkim_selector: Option<String>,
    /// Профиль Milter — набор фильтров из `smtp.milter` (адреса фильтров — в config.yaml). Потребитель — SMTP
    /// (SMTP-40); не задано — все фильтры конфигурации, как без профилей.
    pub milter_profile: Option<String>,
    /// Правило администратора `trash.purge` (решение 2026-11-01-trash-purge); вычисление с выбором пользователя —
    /// [`PolicyResolver::trash_purge`].
    pub trash_purge: Option<TrashPurgeRule>,
}

impl Policy {
    /// Умолчание уровня организации для [`Policy::send_per_hour`], писем в час.
    pub const DEFAULT_SEND_PER_HOUR: u32 = 1000;
    /// Умолчание уровня организации для [`Policy::rcpts_per_hour`], адресатов в час (решение 113).
    pub const DEFAULT_RCPTS_PER_HOUR: u32 = 2000;
    /// Умолчание уровня организации для [`Policy::max_recipients`].
    pub const DEFAULT_MAX_RECIPIENTS: u32 = 100;

    /// Действующий лимит отправки, писем в час: заданный в цепочке или [`Policy::DEFAULT_SEND_PER_HOUR`].
    pub fn send_per_hour_or_default(&self) -> u32 {
        self.send_per_hour.unwrap_or(Self::DEFAULT_SEND_PER_HOUR)
    }

    /// Действующий лимит отправки, адресатов в час: заданный в цепочке или [`Policy::DEFAULT_RCPTS_PER_HOUR`].
    pub fn rcpts_per_hour_or_default(&self) -> u32 {
        self.rcpts_per_hour.unwrap_or(Self::DEFAULT_RCPTS_PER_HOUR)
    }

    /// Действующий лимит получателей письма: заданный в цепочке или [`Policy::DEFAULT_MAX_RECIPIENTS`].
    pub fn max_recipients_or_default(&self) -> u32 {
        self.max_recipients.unwrap_or(Self::DEFAULT_MAX_RECIPIENTS)
    }

    /// Накладывает более специфичный уровень: заданные в `over` поля заменяют поля `self`.
    pub fn overlay(self, over: Policy) -> Policy {
        Policy {
            quota_bytes: over.quota_bytes.or(self.quota_bytes),
            trash_retention: over.trash_retention.or(self.trash_retention),
            retention: over.retention.or(self.retention),
            send_per_hour: over.send_per_hour.or(self.send_per_hour),
            rcpts_per_hour: over.rcpts_per_hour.or(self.rcpts_per_hour),
            max_recipients: over.max_recipients.or(self.max_recipients),
            dkim_selector: over.dkim_selector.or(self.dkim_selector),
            milter_profile: over.milter_profile.or(self.milter_profile),
            trash_purge: over.trash_purge.or(self.trash_purge),
        }
    }
}

/// Уровень цепочки политик «организация → домен → ящик → папка» (CTR-25) — один тип на всю систему: хранилище
/// политик ([`PolicyStore`]), Admin API, in-memory `tp-testkit`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PolicyTarget {
    /// Организация.
    Tenant,
    /// Домен.
    Domain(DomainId),
    /// Учётная запись (ящик).
    Account(AccountId),
    /// Папка.
    Mailbox(MailboxId),
}

/// Политики уровней цепочки (CTR-25): чтение и запись одного уровня без наложения — для Admin API. Наложение
/// цепочки — [`PolicyResolver`], запись видна ему сразу. Существование объекта уровня (домена, учётной записи,
/// папки) не проверяется: это делает вызывающий.
#[async_trait]
pub trait PolicyStore: Send + Sync + 'static {
    /// Политика одного уровня без наложения; не задана — `Policy::default()` (все поля `None`).
    async fn policy(&self, tenant: TenantId, target: PolicyTarget) -> Result<Policy>;

    /// Задаёт политику уровня целиком; `Policy::default()` удаляет уровень.
    async fn set_policy(
        &self,
        tenant: TenantId,
        target: PolicyTarget,
        policy: &Policy,
    ) -> Result<()>;
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
        fn _ps(_: Arc<dyn PolicyStore>) {}
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

    /// Режим подключения ящика в Admin API — строки `keep`, `delete` (CTR-50).
    #[test]
    fn existing_content_serde() {
        for (mode, text) in [
            (ExistingContent::Keep, "\"keep\""),
            (ExistingContent::Delete, "\"delete\""),
        ] {
            assert_eq!(serde_json::to_string(&mode).expect("serialize"), text);
            assert_eq!(
                serde_json::from_str::<ExistingContent>(text).expect("parse"),
                mode
            );
        }
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
            rcpts_per_hour: Some(300),
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
        // Умолчания уровня организации полей SMTP (CTR-67).
        assert_eq!(p.send_per_hour_or_default(), 5);
        assert_eq!(p.rcpts_per_hour_or_default(), 300);
        assert_eq!(
            p.max_recipients_or_default(),
            Policy::DEFAULT_MAX_RECIPIENTS
        );
        assert_eq!(
            Policy::default().send_per_hour_or_default(),
            Policy::DEFAULT_SEND_PER_HOUR
        );
        assert_eq!(Policy::default().rcpts_per_hour_or_default(), 2000);
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
