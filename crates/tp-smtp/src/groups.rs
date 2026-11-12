//! Группы рассылки при приёме (SRS 4.1, 4.5; CONTRACTS.md#Groups): ограничение круга отправителей, очередь
//! модерации, раскрытие вложенных групп.
//!
//! Адрес получателя, совпавший с адресом группы [`GroupStore`], проверяется до ответа `250` на `RCPT TO`:
//! отправитель вне круга группы — `550 5.7.1`. Внешний отправитель (smtp-in) — `None`, отправитель submission —
//! его учётная запись. Письмо в модерируемую группу не от модератора не раскрывается: после `DATA` оно
//! сохраняется одним объектом BLOB (одна ссылка, протокол записи SRS 3.4) и ставится в очередь модерации
//! ([`GroupStore::hold`]); ссылку снимает тот, кто решает судьбу письма. Иначе группа раскрывается
//! ([`Groups::members`]) в учётные записи организации и внешние адреса почтовых контактов.
//!
//! Раскрытие (SMTP-26) — обход группы и вложенных групп, каждая — один раз (циклы не зацикливают): у обычной
//! группы — её учётные записи и контакты в её составе (`ContactStore::group_contacts`); у динамической
//! (`DynamicGroupStore`) — состав по фильтру на момент раскрытия (`expand_dynamic`), статические члены не
//! используются. Адресная книга подключается [`Groups::with_address_book`]. Ограничения отправителей и модерация
//! динамической группы — её записи в [`GroupStore`], как у остальных; группа из списка разрешённых отправителей
//! раскрывается так же.
//!
//! Группы каталога без записи в [`GroupStore`] раскрываются по снимку каталога, как до SMTP-16.
//!
//! Решение модератора (`tp_contracts::groups::Moderation`, FIX-01) — `impl Moderation for Delivery`
//! ([`crate::deliver`]): объект письма читается отсюда, ссылку на него снимает успешное решение.

use std::collections::HashSet;
use std::fmt;
use std::sync::Arc;

use bytes::Bytes;
use time::OffsetDateTime;
use tp_contracts::addressbook::{ContactStore, DynamicGroupStore};
use tp_contracts::blob::{BlobIndex, BlobStore};
use tp_contracts::groups::{
    DistributionGroup, GroupMember, GroupStore, HeldMessage, SenderRestriction,
};
use tp_contracts::{AccountId, BlobKey, GroupId, HeldId, Result, TenantId};

use crate::queue::{release_blob, store_blob};

/// Группы рассылки и хранилище писем очереди модерации.
#[derive(Clone)]
pub struct Groups {
    store: Arc<dyn GroupStore>,
    blobs: Arc<dyn BlobStore>,
    index: Arc<dyn BlobIndex>,
    book: Option<AddressBook>,
}

/// Адресная книга: фильтры динамических групп и почтовые контакты.
#[derive(Clone)]
struct AddressBook {
    dynamic: Arc<dyn DynamicGroupStore>,
    contacts: Arc<dyn ContactStore>,
}

/// Состав группы при раскрытии.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Expanded {
    /// Учётные записи по возрастанию без повторов (активность проверяет вызывающий).
    pub accounts: Vec<AccountId>,
    /// Адреса почтовых контактов без повторов (регистр не учитывается) — внешняя доставка.
    pub contacts: Vec<String>,
}

impl Expanded {
    fn contact(&mut self, email: String) {
        if !self.contacts.iter().any(|c| c.eq_ignore_ascii_case(&email)) {
            self.contacts.push(email);
        }
    }
}

impl fmt::Debug for Groups {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Groups").finish_non_exhaustive()
    }
}

/// Письмо для очереди модерации.
pub(crate) struct Hold<'a> {
    pub tenant: TenantId,
    pub group: &'a DistributionGroup,
    pub sender: &'a str,
    pub sender_account: Option<AccountId>,
    pub message_id: Option<String>,
    pub raw: Bytes,
    pub at: OffsetDateTime,
}

impl Groups {
    /// Группы `store`; письма очереди модерации — объектами `blobs` с учётом ссылок `index`.
    pub fn new(
        store: Arc<dyn GroupStore>,
        blobs: Arc<dyn BlobStore>,
        index: Arc<dyn BlobIndex>,
    ) -> Self {
        Self {
            store,
            blobs,
            index,
            book: None,
        }
    }

    /// Адресная книга (SMTP-26): динамические группы раскрываются по фильтру `dynamic`, контакты `contacts` в
    /// составе групп получают письмо по внешнему адресу.
    pub fn with_address_book(
        mut self,
        dynamic: Arc<dyn DynamicGroupStore>,
        contacts: Arc<dyn ContactStore>,
    ) -> Self {
        self.book = Some(AddressBook { dynamic, contacts });
        self
    }

    /// Группа с адресом рассылки `address`.
    pub(crate) async fn by_address(
        &self,
        tenant: TenantId,
        address: &str,
    ) -> Result<Option<DistributionGroup>> {
        self.store.by_address(tenant, address).await
    }

    /// Группа по идентификатору.
    pub(crate) async fn get(
        &self,
        tenant: TenantId,
        id: GroupId,
    ) -> Result<Option<DistributionGroup>> {
        self.store.get(tenant, id).await
    }

    /// Письмо очереди модерации целиком (как принято).
    pub(crate) async fn read(&self, key: &BlobKey) -> Result<Bytes> {
        crate::deliver::read_blob(self.blobs.as_ref(), key).await
    }

    /// Снимает ссылку письма очереди модерации на объект (после решения модератора).
    pub(crate) async fn release(&self, key: &BlobKey) {
        release_blob(self.index.as_ref(), key).await;
    }

    /// Состав группы с раскрытием вложенных: учётные записи и адреса контактов (см. описание модуля).
    pub(crate) async fn members(
        &self,
        tenant: TenantId,
        group: &DistributionGroup,
    ) -> Result<Expanded> {
        let mut out = Expanded::default();
        let mut seen = HashSet::new();
        let mut pending = vec![group.clone()];
        while let Some(g) = pending.pop() {
            if !seen.insert(g.id) {
                continue;
            }
            if let Some(book) = &self.book {
                if book.dynamic.dynamic(tenant, g.id).await?.is_some() {
                    let m = book.dynamic.expand_dynamic(tenant, g.id).await?;
                    out.accounts.extend(m.accounts);
                    for id in m.contacts {
                        if let Some(c) = book.contacts.contact(tenant, id).await? {
                            out.contact(c.email);
                        }
                    }
                    continue;
                }
                for c in book.contacts.group_contacts(tenant, g.id).await? {
                    out.contact(c.email);
                }
            }
            for m in &g.members {
                match m {
                    GroupMember::Account(a) => out.accounts.push(*a),
                    // Неизвестная вложенная группа пропускается (порядок синхронизации каталога не гарантирован).
                    GroupMember::Group(id) => pending.extend(self.store.get(tenant, *id).await?),
                }
            }
        }
        out.accounts.sort_unstable();
        out.accounts.dedup();
        Ok(out)
    }

    /// Может ли отправитель (`None` — внешний) писать в группу: [`SenderRestriction`]; группы списка
    /// раскрываются [`Groups::members`] (неизвестные пропускаются).
    pub(crate) async fn allowed(
        &self,
        tenant: TenantId,
        group: &DistributionGroup,
        sender: Option<AccountId>,
    ) -> Result<bool> {
        let (sender, groups) = match (&group.senders, sender) {
            (SenderRestriction::Anyone, _) => return Ok(true),
            (_, None) => return Ok(false),
            (SenderRestriction::Internal, Some(_)) => return Ok(true),
            (SenderRestriction::Only { accounts, groups }, Some(s)) => {
                if accounts.contains(&s) {
                    return Ok(true);
                }
                (s, groups)
            }
        };
        for id in groups {
            if let Some(g) = self.store.get(tenant, *id).await?
                && self
                    .members(tenant, &g)
                    .await?
                    .accounts
                    .binary_search(&sender)
                    .is_ok()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Ставит письмо в очередь модерации группы; ошибка постановки снимает ссылку на объект.
    pub(crate) async fn hold(&self, h: Hold<'_>) -> Result<HeldId> {
        let size = h.raw.len() as u64;
        let blob = store_blob(self.blobs.as_ref(), self.index.as_ref(), h.tenant, h.raw).await?;
        let id = HeldId::new();
        let message = HeldMessage {
            id,
            group: Some(h.group.id),
            source: Default::default(),
            moderators: Vec::new(),
            recipients: Vec::new(),
            sender: h.sender.to_owned(),
            sender_account: h.sender_account,
            message_id: h.message_id,
            blob,
            size,
            held_at: h.at,
        };
        if let Err(e) = self.store.hold(h.tenant, message).await {
            release_blob(self.index.as_ref(), &blob).await;
            return Err(e);
        }
        Ok(id)
    }
}
