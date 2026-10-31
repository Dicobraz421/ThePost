//! In-memory метаданные: учётные записи, папки, письма, журнал изменений (CONTRACTS.md#Metadata, #ChangeLog).
//!
//! Семантика — как у `tp-meta`: один счётчик `ModSeq` на учётную запись, UID из `uid_next` папки;
//! `to_trash` и `restore` сохраняют `MessageId` (меняются папка и UID), `copy` и `move_to` создают новые.
//! Изменяющая операция сначала проверяет все условия, затем меняет состояние под одной блокировкой:
//! журнал изменений, события ядра ([`MemEventLog`]) и ссылки на объекты ([`MemBlobIndex`], если подключён)
//! меняются вместе с данными.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use tp_contracts::events::{CoreEvent, CoreEventKind};
use tp_contracts::meta::{
    Account, AccountScope, AccountStore, AccountUpsert, ChangeLog, Changes, Flag, FlagOp, Flags,
    FlagsUpdate, Inserted, Mailbox, MailboxStatus, MailboxStore, Message, MessageChange,
    MessageQuery, MessageStore, NewMailbox, NewMessage, QuotaUsage, SpecialUse,
};
use tp_contracts::{
    AccountId, BlobKey, DomainId, Error, MailboxId, MessageId, ModSeq, Result, TenantId, Uid,
    UidValidity,
};

use crate::blob::MemBlobIndex;
use crate::infra::{Clock, MemEventLog, SystemClock, lock};

/// Срок хранения в корзине по умолчанию (как в конфигурации хранилища).
pub const DEFAULT_TRASH_RETENTION: Duration = Duration::from_secs(30 * 24 * 3600);

/// Системные папки новой учётной записи (как в `tp-meta`).
pub const SYSTEM_FOLDERS: [(&str, SpecialUse); 5] = [
    ("INBOX", SpecialUse::Inbox),
    ("Sent", SpecialUse::Sent),
    ("Drafts", SpecialUse::Drafts),
    ("Trash", SpecialUse::Trash),
    ("Junk", SpecialUse::Junk),
];

/// Вид изменения в журнале. `Moved` — письмо сменило папку с тем же идентификатором (корзина).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Created,
    Updated,
    Destroyed,
    Moved,
}

/// Запись журнала изменений письма.
#[derive(Debug, Clone, Copy)]
struct MsgLog {
    modseq: ModSeq,
    id: MessageId,
    op: Op,
    /// Папка и UID (для `Moved` — новые, для `Destroyed` — прежние).
    mailbox: MailboxId,
    uid: Uid,
    /// Прежние папка и UID (`Moved`).
    from: Option<(MailboxId, Uid)>,
}

#[derive(Debug, Clone)]
struct MailboxRow {
    mailbox: Mailbox,
    uid_next: Uid,
    highest_modseq: ModSeq,
}

#[derive(Debug, Clone)]
struct MessageRow {
    message: Message,
    /// Письмо в корзине: исходная папка (для `restore`) и сроки ([`crate::MemTrash`]).
    trashed: Option<Trashed>,
}

/// Отметка переноса в корзину.
#[derive(Debug, Clone, Copy)]
struct Trashed {
    from: MailboxId,
    deleted_at: OffsetDateTime,
    expires_at: OffsetDateTime,
}

/// Элемент корзины для [`crate::MemTrash`].
#[derive(Debug, Clone)]
pub(crate) struct TrashRow {
    pub(crate) scope: AccountScope,
    pub(crate) message: Message,
    pub(crate) origin: MailboxId,
    pub(crate) deleted_at: OffsetDateTime,
    pub(crate) expires_at: OffsetDateTime,
}

/// Изменения вне данных учётной записи, применяемые вместе с ней.
#[derive(Debug, Default)]
struct Effects {
    events: Vec<(Option<MailboxId>, CoreEventKind)>,
    refs: Vec<(BlobKey, i64)>,
}

#[derive(Debug)]
struct AccountData {
    account: Account,
    modseq: ModSeq,
    uid_validity_last: u32,
    mailboxes: BTreeMap<MailboxId, MailboxRow>,
    messages: HashMap<MessageId, MessageRow>,
    msg_log: Vec<MsgLog>,
    mbx_log: Vec<(ModSeq, MailboxId, Op)>,
    /// Журнал старше этого состояния удалён: `since` меньше — `Conflict`.
    floor: ModSeq,
}

fn blob_keys(m: &Message) -> impl Iterator<Item = BlobKey> + '_ {
    std::iter::once(m.skeleton).chain(m.parts.iter().map(|p| p.blob))
}

impl AccountData {
    fn bump(&mut self) -> ModSeq {
        self.modseq = self.modseq.next();
        self.modseq
    }

    fn mailbox(&self, id: MailboxId) -> Result<&MailboxRow> {
        self.mailboxes.get(&id).ok_or(Error::NotFound)
    }

    fn by_role(&self, role: SpecialUse) -> Result<MailboxId> {
        self.mailboxes
            .values()
            .find(|m| m.mailbox.role == Some(role))
            .map(|m| m.mailbox.id)
            .ok_or(Error::NotFound)
    }

    /// Имя свободно у родителя `parent` (кроме папки `except`).
    fn check_free(
        &self,
        parent: Option<MailboxId>,
        name: &str,
        except: Option<MailboxId>,
    ) -> Result<()> {
        let taken = self.mailboxes.values().any(|m| {
            m.mailbox.parent == parent && m.mailbox.name == name && Some(m.mailbox.id) != except
        });
        if taken {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    /// Новое значение UIDVALIDITY: строго растёт, не меньше unix-времени.
    fn next_uid_validity(&mut self, now: i64) -> UidValidity {
        let now = u32::try_from(now.max(0)).unwrap_or(u32::MAX);
        self.uid_validity_last = self.uid_validity_last.saturating_add(1).max(now);
        UidValidity(self.uid_validity_last)
    }

    fn create_mailbox(&mut self, data: &NewMailbox, now: i64, ms: ModSeq) -> Mailbox {
        let mailbox = Mailbox {
            id: MailboxId::new(),
            parent: data.parent,
            name: data.name.clone(),
            role: data.role,
            uid_validity: self.next_uid_validity(now),
            subscribed: true,
        };
        self.mailboxes.insert(
            mailbox.id,
            MailboxRow {
                mailbox: mailbox.clone(),
                uid_next: Uid::MIN,
                highest_modseq: ms,
            },
        );
        mailbox
    }

    /// Отмечает изменение папок номером `ms`.
    fn touch(&mut self, ms: ModSeq, boxes: impl IntoIterator<Item = MailboxId>) {
        let mut seen = HashSet::new();
        for b in boxes {
            if seen.insert(b)
                && let Some(row) = self.mailboxes.get_mut(&b)
            {
                row.highest_modseq = ms;
                self.mbx_log.push((ms, b, Op::Updated));
            }
        }
    }

    /// Выделяет UID в папке `to`; папка должна существовать.
    fn alloc_uid(&mut self, to: MailboxId) -> Result<Uid> {
        let row = self.mailboxes.get_mut(&to).ok_or(Error::NotFound)?;
        let uid = row.uid_next;
        row.uid_next = uid.next().ok_or(Error::Conflict)?;
        Ok(uid)
    }

    /// Свободных UID в папке `to` хватает на `n` писем.
    fn check_uids(&self, to: MailboxId, n: usize) -> Result<()> {
        let row = self.mailbox(to)?;
        let left = u64::from(u32::MAX) - u64::from(row.uid_next.0) + 1;
        if (n as u64) > left {
            return Err(Error::Conflict);
        }
        Ok(())
    }

    /// Существующие письма из `ids` без повторов, по порядку.
    fn existing(&self, ids: &[MessageId]) -> Vec<MessageId> {
        let mut seen = HashSet::new();
        ids.iter()
            .copied()
            .filter(|id| self.messages.contains_key(id) && seen.insert(*id))
            .collect()
    }

    /// Все `ids` существуют; без повторов, по порядку.
    fn require_all(&self, ids: &[MessageId]) -> Result<Vec<MessageId>> {
        if ids.iter().any(|id| !self.messages.contains_key(id)) {
            return Err(Error::NotFound);
        }
        Ok(self.existing(ids))
    }

    fn insert_message(
        &mut self,
        ms: ModSeq,
        to: MailboxId,
        mut message: Message,
    ) -> Result<Inserted> {
        let uid = self.alloc_uid(to)?;
        message.id = MessageId::new();
        message.mailbox = to;
        message.uid = uid;
        message.modseq = ms;
        let id = message.id;
        self.msg_log.push(MsgLog {
            modseq: ms,
            id,
            op: Op::Created,
            mailbox: to,
            uid,
            from: None,
        });
        self.messages.insert(
            id,
            MessageRow {
                message,
                trashed: None,
            },
        );
        Ok(Inserted {
            id,
            uid,
            modseq: ms,
        })
    }

    /// Копирует письма `src` (существующие) в папку `to` с новыми идентификаторами и UID.
    fn clone_into(
        &mut self,
        ms: ModSeq,
        src: &[MessageId],
        to: MailboxId,
    ) -> Result<Vec<Inserted>> {
        self.check_uids(to, src.len())?;
        let mut out = Vec::with_capacity(src.len());
        for id in src {
            let message = self
                .messages
                .get(id)
                .ok_or(Error::NotFound)?
                .message
                .clone();
            out.push(self.insert_message(ms, to, message)?);
        }
        self.touch(ms, [to]);
        Ok(out)
    }

    fn remove(&mut self, ms: ModSeq, id: MessageId) -> Option<Message> {
        let row = self.messages.remove(&id)?;
        let m = row.message;
        self.msg_log.push(MsgLog {
            modseq: ms,
            id,
            op: Op::Destroyed,
            mailbox: m.mailbox,
            uid: m.uid,
            from: None,
        });
        Some(m)
    }

    /// Переносит письма `ids` в папку `to` с теми же идентификаторами и новыми UID;
    /// `trash` — момент удаления и срок корзины: запомнить исходную папку. Возвращает (письмо, прежняя папка, новый UID).
    fn relocate(
        &mut self,
        ms: ModSeq,
        ids: &[MessageId],
        to: MailboxId,
        trash: Option<(OffsetDateTime, OffsetDateTime)>,
    ) -> Result<Vec<(MessageId, MailboxId, Uid)>> {
        self.check_uids(to, ids.len())?;
        let mut out = Vec::with_capacity(ids.len());
        let mut boxes = vec![to];
        for id in ids {
            if !self.messages.contains_key(id) {
                continue;
            }
            let uid = self.alloc_uid(to)?;
            let Some(row) = self.messages.get_mut(id) else {
                continue;
            };
            let from = (row.message.mailbox, row.message.uid);
            row.trashed = trash.map(|(deleted_at, expires_at)| Trashed {
                from: from.0,
                deleted_at,
                expires_at,
            });
            row.message.mailbox = to;
            row.message.uid = uid;
            row.message.modseq = ms;
            self.msg_log.push(MsgLog {
                modseq: ms,
                id: *id,
                op: Op::Moved,
                mailbox: to,
                uid,
                from: Some(from),
            });
            boxes.push(from.0);
            out.push((*id, from.0, uid));
        }
        self.touch(ms, boxes);
        Ok(out)
    }
}

#[derive(Debug, Default)]
struct MetaState {
    accounts: HashMap<(TenantId, AccountId), AccountData>,
}

/// Метаданные в памяти: [`AccountStore`], [`MailboxStore`], [`MessageStore`], [`ChangeLog`].
/// Клоны разделяют данные.
#[derive(Debug, Clone)]
pub struct MemMetadata {
    state: Arc<Mutex<MetaState>>,
    clock: Arc<dyn Clock>,
    events: MemEventLog,
    blobs: Option<MemBlobIndex>,
    trash_retention: Duration,
}

impl MemMetadata {
    /// Метаданные с часами `clock` и собственным журналом событий.
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            state: Arc::default(),
            clock,
            events: MemEventLog::default(),
            blobs: None,
            trash_retention: DEFAULT_TRASH_RETENTION,
        }
    }

    /// События ядра — в журнал `events`.
    pub fn with_event_log(mut self, events: MemEventLog) -> Self {
        self.events = events;
        self
    }

    /// Учёт ссылок на объекты (`copy` добавляет, `expunge_final` снимает) — в `blobs`.
    pub fn with_blob_index(mut self, blobs: MemBlobIndex) -> Self {
        self.blobs = Some(blobs);
        self
    }

    /// Срок хранения в корзине для писем, удаляемых после вызова (по умолчанию 30 суток).
    pub fn with_trash_retention(mut self, retention: Duration) -> Self {
        self.trash_retention = retention;
        self
    }

    /// Журнал событий ядра, в который пишут изменения.
    pub fn event_log(&self) -> &MemEventLog {
        &self.events
    }

    /// Создаёт учётную запись `login` в организации по умолчанию (хелпер тестов).
    pub async fn account(&self, login: &str) -> Result<AccountScope> {
        let acc = self
            .upsert(
                TenantId::DEFAULT,
                AccountUpsert {
                    domain: DomainId::new(),
                    login: login.to_owned(),
                    display_name: login.to_owned(),
                    external_id: None,
                    enabled: true,
                },
            )
            .await?;
        Ok(AccountScope {
            tenant: acc.tenant,
            account: acc.id,
        })
    }

    /// Папка с назначением `role` (хелпер тестов).
    pub fn mailbox_by_role(&self, scope: AccountScope, role: SpecialUse) -> Result<MailboxId> {
        let st = lock(&self.state);
        st.accounts
            .get(&(scope.tenant, scope.account))
            .ok_or(Error::NotFound)?
            .by_role(role)
    }

    /// Удаляет журнал изменений до `upto` включительно: `since` меньше — `Conflict`.
    pub fn truncate_changelog(&self, scope: AccountScope, upto: ModSeq) -> Result<()> {
        let mut st = lock(&self.state);
        let acc = st
            .accounts
            .get_mut(&(scope.tenant, scope.account))
            .ok_or(Error::NotFound)?;
        let upto = upto.min(acc.modseq);
        acc.floor = acc.floor.max(upto);
        acc.msg_log.retain(|c| c.modseq > upto);
        acc.mbx_log.retain(|c| c.0 > upto);
        Ok(())
    }

    /// Операция над учётной записью `scope` под одной блокировкой; эффекты применяются при успехе.
    fn with_account<R>(
        &self,
        scope: AccountScope,
        f: impl FnOnce(&mut AccountData, &mut Effects) -> Result<R>,
    ) -> Result<R> {
        let mut st = lock(&self.state);
        let acc = st
            .accounts
            .get_mut(&(scope.tenant, scope.account))
            .ok_or(Error::NotFound)?;
        let mut fx = Effects::default();
        let out = f(acc, &mut fx)?;
        let at = self.clock.now();
        for (mailbox, kind) in fx.events {
            self.events.append(CoreEvent {
                tenant: scope.tenant,
                account: scope.account,
                mailbox,
                kind,
                at,
            });
        }
        if let Some(blobs) = &self.blobs {
            for (key, delta) in fx.refs {
                blobs.adjust(&[key], delta);
            }
        }
        Ok(out)
    }

    fn read<R>(&self, scope: AccountScope, f: impl FnOnce(&AccountData) -> Result<R>) -> Result<R> {
        let st = lock(&self.state);
        f(st.accounts
            .get(&(scope.tenant, scope.account))
            .ok_or(Error::NotFound)?)
    }

    /// Момент удаления в корзину и срок окончательного удаления.
    fn trash_mark(&self) -> (OffsetDateTime, OffsetDateTime) {
        let now = self.clock.now();
        (
            now,
            now.saturating_add(
                self.trash_retention
                    .try_into()
                    .unwrap_or(time::Duration::MAX),
            ),
        )
    }

    /// Элементы корзины: учётная запись `scope` или все (`None`).
    pub(crate) fn trash_rows(&self, scope: Option<AccountScope>) -> Vec<TrashRow> {
        let st = lock(&self.state);
        st.accounts
            .iter()
            .filter(|(k, _)| scope.is_none_or(|s| (s.tenant, s.account) == **k))
            .flat_map(|(&(tenant, account), acc)| {
                let trash = acc.by_role(SpecialUse::Trash).ok();
                acc.messages.values().filter_map(move |row| {
                    let t = row.trashed?;
                    (Some(row.message.mailbox) == trash).then(|| TrashRow {
                        scope: AccountScope { tenant, account },
                        message: row.message.clone(),
                        origin: t.from,
                        deleted_at: t.deleted_at,
                        expires_at: t.expires_at,
                    })
                })
            })
            .collect()
    }

    /// Переносит срок окончательного удаления письма в корзине; письма в корзине нет — `NotFound`.
    pub(crate) fn defer_trash(
        &self,
        scope: AccountScope,
        id: MessageId,
        until: OffsetDateTime,
    ) -> Result<()> {
        let mut st = lock(&self.state);
        let acc = st
            .accounts
            .get_mut(&(scope.tenant, scope.account))
            .ok_or(Error::NotFound)?;
        let trash = acc.by_role(SpecialUse::Trash)?;
        let row = acc
            .messages
            .get_mut(&id)
            .filter(|r| r.message.mailbox == trash)
            .ok_or(Error::NotFound)?;
        let t = row.trashed.as_mut().ok_or(Error::NotFound)?;
        t.expires_at = until;
        Ok(())
    }

    fn now_unix(&self) -> i64 {
        self.clock.now().unix_timestamp()
    }
}

impl Default for MemMetadata {
    fn default() -> Self {
        Self::new(Arc::new(SystemClock))
    }
}

/// Имя папки: непустое, без управляющих символов.
fn check_name(name: &str) -> Result<()> {
    if name.is_empty() || name.chars().any(char::is_control) {
        return Err(Error::invalid(format!("недопустимое имя папки: {name:?}")));
    }
    Ok(())
}

/// Ключевое слово: непустое, без пробелов и управляющих символов, не начинается с `\`.
fn check_flags(flags: &Flags) -> Result<()> {
    for f in flags {
        if let Flag::Keyword(k) = f
            && (k.is_empty()
                || k.starts_with('\\')
                || k.chars().any(|c| c.is_whitespace() || c.is_control()))
        {
            return Err(Error::invalid(format!(
                "недопустимое ключевое слово: {k:?}"
            )));
        }
    }
    Ok(())
}

fn apply(op: &FlagOp, old: &Flags) -> Flags {
    match op {
        FlagOp::Add(add) => old.union(add).cloned().collect(),
        FlagOp::Remove(rm) => old.difference(rm).cloned().collect(),
        FlagOp::Replace(new) => new.clone(),
    }
}

#[async_trait]
impl AccountStore for MemMetadata {
    async fn get(&self, tenant: TenantId, id: AccountId) -> Result<Option<Account>> {
        Ok(lock(&self.state)
            .accounts
            .get(&(tenant, id))
            .map(|a| a.account.clone()))
    }

    async fn by_login(&self, tenant: TenantId, login: &str) -> Result<Option<Account>> {
        Ok(lock(&self.state)
            .accounts
            .values()
            .find(|a| a.account.tenant == tenant && a.account.login.eq_ignore_ascii_case(login))
            .map(|a| a.account.clone()))
    }

    async fn upsert(&self, tenant: TenantId, data: AccountUpsert) -> Result<Account> {
        if data.login.is_empty() {
            return Err(Error::invalid("пустое имя входа"));
        }
        let now = self.now_unix();
        let mut st = lock(&self.state);
        let in_tenant = |a: &&AccountData| a.account.tenant == tenant;
        let by_ext = data.external_id.as_ref().and_then(|ext| {
            st.accounts
                .values()
                .filter(in_tenant)
                .find(|a| a.account.external_id.as_ref() == Some(ext))
                .map(|a| a.account.id)
        });
        let found = match by_ext {
            Some(id) => Some(id),
            None => match st
                .accounts
                .values()
                .filter(in_tenant)
                .find(|a| a.account.login.eq_ignore_ascii_case(&data.login))
            {
                Some(a)
                    if a.account.external_id.is_some()
                        && data.external_id.is_some()
                        && a.account.external_id != data.external_id =>
                {
                    return Err(Error::Conflict);
                }
                Some(a) => Some(a.account.id),
                None => None,
            },
        };
        // Новое имя входа не должно совпасть с именем другой учётной записи.
        let clash = st.accounts.values().filter(in_tenant).any(|a| {
            Some(a.account.id) != found && a.account.login.eq_ignore_ascii_case(&data.login)
        });
        if clash {
            return Err(Error::Conflict);
        }
        if let Some(id) = found {
            let acc = st
                .accounts
                .get_mut(&(tenant, id))
                .ok_or_else(|| Error::internal("учётная запись пропала"))?;
            let a = &mut acc.account;
            a.domain = data.domain;
            a.login = data.login;
            a.display_name = data.display_name;
            a.external_id = data.external_id.or(a.external_id.take());
            a.enabled = data.enabled;
            return Ok(a.clone());
        }
        let account = Account {
            id: AccountId::new(),
            tenant,
            domain: data.domain,
            login: data.login,
            display_name: data.display_name,
            external_id: data.external_id,
            enabled: data.enabled,
        };
        let mut acc = AccountData {
            account: account.clone(),
            modseq: ModSeq::ZERO,
            uid_validity_last: 0,
            mailboxes: BTreeMap::new(),
            messages: HashMap::new(),
            msg_log: Vec::new(),
            mbx_log: Vec::new(),
            floor: ModSeq::ZERO,
        };
        for (name, role) in SYSTEM_FOLDERS {
            let data = NewMailbox {
                parent: None,
                name: name.to_owned(),
                role: Some(role),
            };
            acc.create_mailbox(&data, now, ModSeq::ZERO);
        }
        st.accounts.insert((tenant, account.id), acc);
        Ok(account)
    }

    async fn quota(&self, tenant: TenantId, id: AccountId) -> Result<QuotaUsage> {
        let st = lock(&self.state);
        let acc = st.accounts.get(&(tenant, id)).ok_or(Error::NotFound)?;
        Ok(QuotaUsage {
            used_bytes: acc.messages.values().map(|m| m.message.size).sum(),
            messages: acc.messages.len() as u64,
            limit_bytes: None,
        })
    }
}

#[async_trait]
impl MailboxStore for MemMetadata {
    async fn list(&self, scope: AccountScope) -> Result<Vec<Mailbox>> {
        self.read(scope, |acc| {
            Ok(acc.mailboxes.values().map(|m| m.mailbox.clone()).collect())
        })
    }

    async fn get(&self, scope: AccountScope, id: MailboxId) -> Result<Option<Mailbox>> {
        self.read(scope, |acc| {
            Ok(acc.mailboxes.get(&id).map(|m| m.mailbox.clone()))
        })
    }

    async fn create(&self, scope: AccountScope, data: NewMailbox) -> Result<Mailbox> {
        check_name(&data.name)?;
        let now = self.now_unix();
        self.with_account(scope, |acc, fx| {
            if let Some(p) = data.parent {
                acc.mailbox(p)?;
            }
            acc.check_free(data.parent, &data.name, None)?;
            let ms = acc.bump();
            let mb = acc.create_mailbox(&data, now, ms);
            acc.mbx_log.push((ms, mb.id, Op::Created));
            fx.events.push((Some(mb.id), CoreEventKind::MailboxCreated));
            Ok(mb)
        })
    }

    async fn rename(
        &self,
        scope: AccountScope,
        id: MailboxId,
        parent: Option<MailboxId>,
        name: &str,
    ) -> Result<()> {
        check_name(name)?;
        self.with_account(scope, |acc, _| {
            if acc.mailbox(id)?.mailbox.role == Some(SpecialUse::Inbox) {
                return Err(Error::Forbidden);
            }
            // Новый родитель не может быть самой папкой или её потомком.
            let mut cur = parent;
            while let Some(p) = cur {
                if p == id {
                    return Err(Error::invalid("папка не может быть вложена в себя"));
                }
                cur = acc.mailbox(p)?.mailbox.parent;
            }
            acc.check_free(parent, name, Some(id))?;
            let ms = acc.bump();
            let row = acc.mailboxes.get_mut(&id).ok_or(Error::NotFound)?;
            row.mailbox.parent = parent;
            row.mailbox.name = name.to_owned();
            row.highest_modseq = ms;
            acc.mbx_log.push((ms, id, Op::Updated));
            Ok(())
        })
    }

    async fn delete(&self, scope: AccountScope, id: MailboxId, cascade: bool) -> Result<()> {
        let mark = self.trash_mark();
        self.with_account(scope, |acc, fx| {
            acc.mailbox(id)?;
            // Папка и все вложенные, предки раньше потомков.
            let mut boxes = vec![id];
            let mut i = 0;
            while i < boxes.len() {
                let parent = boxes[i];
                boxes.extend(
                    acc.mailboxes
                        .values()
                        .filter(|m| m.mailbox.parent == Some(parent))
                        .map(|m| m.mailbox.id),
                );
                i += 1;
            }
            if boxes.iter().any(|b| {
                acc.mailboxes
                    .get(b)
                    .is_some_and(|m| m.mailbox.role.is_some())
            }) {
                return Err(Error::Forbidden);
            }
            // Вложенные без подтверждения: решение принимает пользователь в клиенте.
            if boxes.len() > 1 && !cascade {
                return Err(Error::Conflict);
            }
            let mut ids: Vec<_> = acc
                .messages
                .values()
                .filter_map(|m| {
                    let pos = boxes.iter().position(|b| *b == m.message.mailbox)?;
                    Some((pos, m.message.uid, m.message.id))
                })
                .collect();
            ids.sort_unstable();
            let ids: Vec<_> = ids.into_iter().map(|(_, _, id)| id).collect();
            let trash = if ids.is_empty() {
                None
            } else {
                let trash = acc.by_role(SpecialUse::Trash)?;
                acc.check_uids(trash, ids.len())?;
                Some(trash)
            };
            let ms = acc.bump();
            // Письма — в корзину (SRS 4.7); восстановление без исходной папки — во «Входящие».
            if let Some(trash) = trash {
                for (message, _, _) in acc.relocate(ms, &ids, trash, Some(mark))? {
                    fx.events
                        .push((Some(trash), CoreEventKind::Trashed { message }));
                }
            }
            // Потомки раньше предков.
            for b in boxes.iter().rev() {
                acc.mailboxes.remove(b);
                acc.mbx_log.retain(|c| !(c.0 == ms && c.1 == *b));
                acc.mbx_log.push((ms, *b, Op::Destroyed));
                fx.events.push((Some(*b), CoreEventKind::MailboxDeleted));
            }
            Ok(())
        })
    }

    async fn set_subscribed(&self, scope: AccountScope, id: MailboxId, on: bool) -> Result<()> {
        self.with_account(scope, |acc, _| {
            acc.mailbox(id)?;
            let ms = acc.bump();
            let row = acc.mailboxes.get_mut(&id).ok_or(Error::NotFound)?;
            row.mailbox.subscribed = on;
            row.highest_modseq = ms;
            acc.mbx_log.push((ms, id, Op::Updated));
            Ok(())
        })
    }

    async fn status(&self, scope: AccountScope, id: MailboxId) -> Result<MailboxStatus> {
        self.read(scope, |acc| {
            let row = acc.mailbox(id)?;
            let (mut messages, mut unseen, mut size) = (0u32, 0u32, 0u64);
            for m in acc.messages.values().filter(|m| m.message.mailbox == id) {
                messages += 1;
                unseen += u32::from(!m.message.flags.contains(&Flag::Seen));
                size += m.message.size;
            }
            Ok(MailboxStatus {
                messages,
                unseen,
                uid_next: row.uid_next,
                uid_validity: row.mailbox.uid_validity,
                highest_modseq: row.highest_modseq,
                size,
            })
        })
    }
}

#[async_trait]
impl MessageStore for MemMetadata {
    async fn insert(
        &self,
        scope: AccountScope,
        mailbox: MailboxId,
        msg: NewMessage,
    ) -> Result<Inserted> {
        check_flags(&msg.flags)?;
        if msg.skeleton.tenant != scope.tenant
            || msg.parts.iter().any(|p| p.blob.tenant != scope.tenant)
        {
            return Err(Error::invalid("объект письма другой организации"));
        }
        self.with_account(scope, |acc, fx| {
            acc.check_uids(mailbox, 1)?;
            let ms = acc.bump();
            let message = Message {
                id: MessageId::new(),
                mailbox,
                uid: Uid::MIN,
                modseq: ms,
                size: msg.size,
                received_at: msg.received_at,
                flags: msg.flags,
                skeleton: msg.skeleton,
                parts: msg.parts,
                meta: msg.meta,
            };
            let ins = acc.insert_message(ms, mailbox, message)?;
            acc.touch(ms, [mailbox]);
            fx.events
                .push((Some(mailbox), CoreEventKind::Delivered { message: ins.id }));
            Ok(ins)
        })
    }

    async fn get(&self, scope: AccountScope, id: MessageId) -> Result<Option<Message>> {
        self.read(scope, |acc| {
            Ok(acc.messages.get(&id).map(|m| m.message.clone()))
        })
    }

    async fn list(
        &self,
        scope: AccountScope,
        mailbox: MailboxId,
        query: MessageQuery,
        limit: usize,
    ) -> Result<Vec<Message>> {
        self.read(scope, |acc| {
            acc.mailbox(mailbox)?;
            let ids: HashSet<MessageId> = match &query {
                MessageQuery::Ids(ids) => ids.iter().copied().collect(),
                _ => HashSet::new(),
            };
            let mut out: Vec<_> = acc
                .messages
                .values()
                .map(|m| &m.message)
                .filter(|m| m.mailbox == mailbox)
                .filter(|m| match &query {
                    MessageQuery::All => true,
                    MessageQuery::Uids(ranges) => ranges.iter().any(|r| r.contains(&m.uid)),
                    MessageQuery::Ids(_) => ids.contains(&m.id),
                    MessageQuery::ChangedSince(ms) => m.modseq > *ms,
                })
                .cloned()
                .collect();
            out.sort_by_key(|m| m.uid);
            out.truncate(limit);
            Ok(out)
        })
    }

    async fn set_flags(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        op: FlagOp,
        unchanged_since: Option<ModSeq>,
    ) -> Result<FlagsUpdate> {
        match &op {
            FlagOp::Add(f) | FlagOp::Remove(f) | FlagOp::Replace(f) => check_flags(f)?,
        }
        self.with_account(scope, |acc, fx| {
            let mut update = FlagsUpdate {
                modseq: acc.modseq,
                ..FlagsUpdate::default()
            };
            let mut changed = Vec::new();
            for id in acc.existing(ids) {
                let m = &acc.messages[&id].message;
                if unchanged_since.is_some_and(|u| m.modseq > u) {
                    update.conflicts.push(id);
                    continue;
                }
                let new = apply(&op, &m.flags);
                if new != m.flags {
                    changed.push((id, new));
                }
            }
            if changed.is_empty() {
                return Ok(update);
            }
            let ms = acc.bump();
            let mut boxes = Vec::new();
            for (id, flags) in changed {
                let Some(row) = acc.messages.get_mut(&id) else {
                    continue;
                };
                let m = &mut row.message;
                m.flags = flags;
                m.modseq = ms;
                acc.msg_log.push(MsgLog {
                    modseq: ms,
                    id,
                    op: Op::Updated,
                    mailbox: m.mailbox,
                    uid: m.uid,
                    from: None,
                });
                boxes.push(m.mailbox);
                fx.events
                    .push((Some(m.mailbox), CoreEventKind::FlagsChanged { message: id }));
                update.changed.push(id);
            }
            acc.touch(ms, boxes);
            update.modseq = ms;
            Ok(update)
        })
    }

    async fn copy(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        to: MailboxId,
    ) -> Result<Vec<Inserted>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.with_account(scope, |acc, fx| {
            let src = acc.require_all(ids)?;
            acc.check_uids(to, src.len())?;
            let ms = acc.bump();
            let inserted = acc.clone_into(ms, &src, to)?;
            for id in &src {
                let m = &acc.messages[id].message;
                fx.refs.extend(blob_keys(m).map(|k| (k, 1)));
            }
            for i in &inserted {
                fx.events
                    .push((Some(to), CoreEventKind::Delivered { message: i.id }));
            }
            Ok(inserted)
        })
    }

    async fn move_to(
        &self,
        scope: AccountScope,
        ids: &[MessageId],
        to: MailboxId,
    ) -> Result<Vec<Inserted>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.with_account(scope, |acc, fx| {
            let src = acc.require_all(ids)?;
            acc.check_uids(to, src.len())?;
            let ms = acc.bump();
            let inserted = acc.clone_into(ms, &src, to)?;
            // Ссылки на объекты переходят к новым письмам; исходные удаляются.
            let mut boxes = Vec::new();
            for (old, new) in src.iter().zip(&inserted) {
                let removed = acc.remove(ms, *old).ok_or(Error::Conflict)?;
                boxes.push(removed.mailbox);
                fx.events.push((
                    Some(to),
                    CoreEventKind::Moved {
                        from_message: *old,
                        from_mailbox: removed.mailbox,
                        message: new.id,
                    },
                ));
            }
            acc.touch(ms, boxes);
            Ok(inserted)
        })
    }

    async fn to_trash(&self, scope: AccountScope, ids: &[MessageId]) -> Result<ModSeq> {
        let mark = self.trash_mark();
        self.with_account(scope, |acc, fx| {
            let trash = acc.by_role(SpecialUse::Trash)?;
            // Отсутствующие и уже лежащие в корзине письма пропускаются.
            let src: Vec<_> = acc
                .existing(ids)
                .into_iter()
                .filter(|id| acc.messages[id].message.mailbox != trash)
                .collect();
            if src.is_empty() {
                return Ok(acc.modseq);
            }
            acc.check_uids(trash, src.len())?;
            let ms = acc.bump();
            for (message, _, _) in acc.relocate(ms, &src, trash, Some(mark))? {
                fx.events
                    .push((Some(trash), CoreEventKind::Trashed { message }));
            }
            Ok(ms)
        })
    }

    async fn restore(&self, scope: AccountScope, ids: &[MessageId]) -> Result<Vec<Inserted>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.with_account(scope, |acc, fx| {
            let trash = acc.by_role(SpecialUse::Trash)?;
            let inbox = acc.by_role(SpecialUse::Inbox)?;
            let src = acc.require_all(ids)?;
            if src
                .iter()
                .any(|id| acc.messages[id].message.mailbox != trash)
            {
                return Err(Error::invalid("письмо не в корзине"));
            }
            // Группы по папке назначения в порядке первого появления; исходной папки нет — «Входящие».
            let mut groups: Vec<(MailboxId, Vec<MessageId>)> = Vec::new();
            for id in &src {
                let to = acc.messages[id]
                    .trashed
                    .map(|t| t.from)
                    .filter(|m| acc.mailboxes.contains_key(m))
                    .unwrap_or(inbox);
                match groups.iter_mut().find(|(g, _)| *g == to) {
                    Some((_, v)) => v.push(*id),
                    None => groups.push((to, vec![*id])),
                }
            }
            for (to, group) in &groups {
                acc.check_uids(*to, group.len())?;
            }
            let ms = acc.bump();
            let mut uids = HashMap::new();
            for (to, group) in groups {
                for (message, _, uid) in acc.relocate(ms, &group, to, None)? {
                    uids.insert(message, uid);
                    fx.events.push((
                        Some(to),
                        CoreEventKind::Moved {
                            from_message: message,
                            from_mailbox: trash,
                            message,
                        },
                    ));
                }
            }
            src.iter()
                .map(|id| {
                    Ok(Inserted {
                        id: *id,
                        uid: uids.get(id).copied().ok_or(Error::Conflict)?,
                        modseq: ms,
                    })
                })
                .collect()
        })
    }

    async fn expunge_final(&self, scope: AccountScope, ids: &[MessageId]) -> Result<ModSeq> {
        self.with_account(scope, |acc, fx| {
            let src = acc.existing(ids);
            if src.is_empty() {
                return Ok(acc.modseq);
            }
            let ms = acc.bump();
            let mut boxes = Vec::new();
            for id in src {
                if let Some(m) = acc.remove(ms, id) {
                    fx.refs.extend(blob_keys(&m).map(|k| (k, -1)));
                    boxes.push(m.mailbox);
                    fx.events
                        .push((Some(m.mailbox), CoreEventKind::Purged { message: id }));
                }
            }
            acc.touch(ms, boxes);
            Ok(ms)
        })
    }
}

// ---------- Журнал изменений ----------

/// Вид изменения с точки зрения запроса.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Seen {
    Created,
    Updated,
    Destroyed,
}

fn seen(op: Op) -> Seen {
    match op {
        Op::Created => Seen::Created,
        Op::Updated | Op::Moved => Seen::Updated,
        Op::Destroyed => Seen::Destroyed,
    }
}

/// Сводка изменений по ключу объекта: удалённый в интервале — только `destroyed`,
/// созданный — `created`, иначе `updated`; значение — по последнему изменению, порядок — по первому.
struct Fold<K, V> {
    order: Vec<K>,
    state: HashMap<K, (bool, bool, V)>,
}

impl<K: Hash + Eq + Copy, V> Fold<K, V> {
    fn new() -> Self {
        Self {
            order: Vec::new(),
            state: HashMap::new(),
        }
    }

    fn add(&mut self, key: K, kind: Seen, value: V) {
        let e = self.state.entry(key).or_insert_with(|| {
            self.order.push(key);
            (false, false, value)
        });
        e.0 |= kind == Seen::Created;
        e.1 |= kind == Seen::Destroyed;
    }

    fn set(&mut self, key: K, value: V) {
        if let Some(e) = self.state.get_mut(&key) {
            e.2 = value;
        }
    }

    fn finish(mut self, new_state: ModSeq, has_more: bool) -> Changes<V> {
        let mut out = Changes {
            created: Vec::new(),
            updated: Vec::new(),
            destroyed: Vec::new(),
            new_state,
            has_more,
        };
        for k in self.order {
            if let Some((created, destroyed, v)) = self.state.remove(&k) {
                if destroyed {
                    out.destroyed.push(v);
                } else if created {
                    out.created.push(v);
                } else {
                    out.updated.push(v);
                }
            }
        }
        out
    }
}

/// Строки журнала после `since`, не больше `limit` (один `ModSeq` не разрезается):
/// строки, состояние после них и признак продолжения.
fn window<T: Copy>(
    acc: &AccountData,
    rows: impl Iterator<Item = T>,
    modseq: impl Fn(&T) -> ModSeq,
    since: ModSeq,
    limit: usize,
) -> Result<(Vec<T>, ModSeq, bool)> {
    if since < acc.floor || since > acc.modseq {
        return Err(Error::Conflict);
    }
    let limit = limit.max(1);
    let mut rows: Vec<T> = rows.filter(|r| modseq(r) > since).collect();
    if rows.len() <= limit {
        return Ok((rows, acc.modseq, false));
    }
    let cut = modseq(&rows[limit]);
    let first = modseq(&rows[0]);
    // Одно изменение больше `limit` строк: отдаётся целиком.
    let keep = if first == cut { cut } else { ModSeq(cut.0 - 1) };
    rows.retain(|r| modseq(r) <= keep);
    let last = rows.last().map_or(since, &modseq);
    Ok((rows, last, true))
}

fn change(c: &MsgLog, mailbox: MailboxId, uid: Uid) -> MessageChange {
    MessageChange {
        id: c.id,
        mailbox,
        uid,
        modseq: c.modseq,
    }
}

#[async_trait]
impl ChangeLog for MemMetadata {
    async fn message_changes(
        &self,
        scope: AccountScope,
        mailbox: Option<MailboxId>,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<MessageChange>> {
        self.read(scope, |acc| {
            let rows = acc.msg_log.iter().copied().filter(|c| {
                mailbox.is_none_or(|m| c.mailbox == m || c.from.is_some_and(|f| f.0 == m))
            });
            let (rows, new_state, has_more) = window(acc, rows, |c| c.modseq, since, limit)?;
            match mailbox {
                // Учётная запись: ключ — письмо; перенос в корзину и обратно — изменение.
                None => {
                    let mut fold = Fold::new();
                    for c in &rows {
                        let v = change(c, c.mailbox, c.uid);
                        fold.add(c.id, seen(c.op), v);
                        fold.set(c.id, v);
                    }
                    Ok(fold.finish(new_state, has_more))
                }
                // Папка: ключ — письмо и его UID в папке; перенос — удаление из одной и создание в другой.
                Some(m) => {
                    let mut fold = Fold::new();
                    let mut add = |v: MessageChange, kind: Seen| {
                        fold.add((v.id, v.uid), kind, v);
                        fold.set((v.id, v.uid), v);
                    };
                    for c in &rows {
                        if c.op == Op::Moved {
                            if let Some((from, uid)) = c.from.filter(|f| f.0 == m) {
                                add(change(c, from, uid), Seen::Destroyed);
                            }
                            if c.mailbox == m {
                                add(change(c, m, c.uid), Seen::Created);
                            }
                        } else if c.mailbox == m {
                            add(change(c, m, c.uid), seen(c.op));
                        }
                    }
                    Ok(fold.finish(new_state, has_more))
                }
            }
        })
    }

    async fn mailbox_changes(
        &self,
        scope: AccountScope,
        since: ModSeq,
        limit: usize,
    ) -> Result<Changes<MailboxId>> {
        self.read(scope, |acc| {
            let (rows, new_state, has_more) =
                window(acc, acc.mbx_log.iter().copied(), |c| c.0, since, limit)?;
            let mut fold = Fold::new();
            for (_, id, op) in rows {
                fold.add(id, seen(op), id);
            }
            Ok(fold.finish(new_state, has_more))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::blob::{BlobState, MemBlobIndex};
    use crate::infra::ManualClock;
    use tp_contracts::blob::BlobIndex;
    use tp_contracts::meta::{MessageMeta, PartRef};
    use tp_contracts::{BlobHash, Gen};

    fn key(n: u8) -> BlobKey {
        BlobKey {
            tenant: TenantId::DEFAULT,
            hash: BlobHash([n; 32]),
            generation: Gen::INITIAL,
        }
    }

    fn new_message(size: u64) -> NewMessage {
        NewMessage {
            skeleton: key(1),
            parts: vec![PartRef {
                part: "2".into(),
                blob: key(2),
                size: 10,
            }],
            size,
            received_at: time::OffsetDateTime::UNIX_EPOCH,
            flags: Flags::new(),
            meta: MessageMeta::default(),
        }
    }

    async fn setup() -> (MemMetadata, AccountScope, MailboxId) {
        let meta = MemMetadata::new(ManualClock::default().shared());
        let scope = meta.account("user@example.com").await.expect("ok");
        let inbox = meta.mailbox_by_role(scope, SpecialUse::Inbox).expect("ok");
        (meta, scope, inbox)
    }

    #[tokio::test]
    async fn uid_and_modseq_monotonic() {
        let (meta, scope, inbox) = setup().await;
        let mut last = ModSeq::ZERO;
        let mut uids = Vec::new();
        for _ in 0..5 {
            let ins = meta
                .insert(scope, inbox, new_message(100))
                .await
                .expect("ok");
            assert!(ins.modseq > last);
            last = ins.modseq;
            uids.push(ins.uid);
        }
        assert_eq!(uids, (1..=5).map(Uid).collect::<Vec<_>>());

        // Удалённые UID не переиспользуются.
        let all = MessageStore::list(&meta, scope, inbox, MessageQuery::All, 100)
            .await
            .expect("ok");
        let last_id = all.last().expect("ok").id;
        let ms = meta.expunge_final(scope, &[last_id]).await.expect("ok");
        assert!(ms > last);
        let ins = meta.insert(scope, inbox, new_message(1)).await.expect("ok");
        assert_eq!(ins.uid, Uid(6));

        let st = meta.status(scope, inbox).await.expect("ok");
        assert_eq!(st.messages, 5);
        assert_eq!(st.unseen, 5);
        assert_eq!(st.uid_next, Uid(7));
        assert_eq!(st.highest_modseq, ins.modseq);

        // Пересозданная папка — новый UIDVALIDITY.
        let a = meta
            .create(
                scope,
                NewMailbox {
                    parent: None,
                    name: "A".into(),
                    role: None,
                },
            )
            .await
            .expect("ok");
        meta.delete(scope, a.id, false).await.expect("ok");
        let b = meta
            .create(
                scope,
                NewMailbox {
                    parent: None,
                    name: "A".into(),
                    role: None,
                },
            )
            .await
            .expect("ok");
        assert!(b.uid_validity > a.uid_validity);
    }

    #[tokio::test]
    async fn flags_condstore() {
        let (meta, scope, inbox) = setup().await;
        let a = meta.insert(scope, inbox, new_message(1)).await.expect("ok");
        let b = meta.insert(scope, inbox, new_message(1)).await.expect("ok");
        let seen = FlagOp::Add(Flags::from([Flag::Seen]));
        let up = meta
            .set_flags(scope, &[a.id, b.id], seen.clone(), Some(a.modseq))
            .await
            .expect("ok");
        assert_eq!(up.changed, [a.id]);
        assert_eq!(up.conflicts, [b.id]);
        assert!(up.modseq > b.modseq);

        // Ничего не изменилось — текущий ModSeq.
        let again = meta
            .set_flags(scope, &[a.id], seen, None)
            .await
            .expect("ok");
        assert!(again.changed.is_empty());
        assert_eq!(again.modseq, up.modseq);

        let bad = FlagOp::Add(Flags::from([Flag::Keyword("a b".into())]));
        assert!(meta.set_flags(scope, &[a.id], bad, None).await.is_err());

        let changed = MessageStore::list(
            &meta,
            scope,
            inbox,
            MessageQuery::ChangedSince(b.modseq),
            10,
        )
        .await
        .expect("ok");
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].id, a.id);
        assert_eq!(meta.status(scope, inbox).await.expect("ok").unseen, 1);
    }

    #[tokio::test]
    async fn changes_since() {
        let (meta, scope, inbox) = setup().await;
        let s0 = meta
            .message_changes(scope, None, ModSeq::ZERO, 100)
            .await
            .expect("ok")
            .new_state;
        let a = meta.insert(scope, inbox, new_message(1)).await.expect("ok");
        let b = meta.insert(scope, inbox, new_message(1)).await.expect("ok");
        let c1 = meta
            .message_changes(scope, None, s0, 100)
            .await
            .expect("ok");
        assert_eq!(
            c1.created.iter().map(|c| c.id).collect::<Vec<_>>(),
            [a.id, b.id]
        );
        assert_eq!(c1.new_state, b.modseq);
        assert!(!c1.has_more);

        // Создан и удалён в интервале — только destroyed.
        meta.set_flags(scope, &[a.id], FlagOp::Add(Flags::from([Flag::Seen])), None)
            .await
            .expect("ok");
        meta.expunge_final(scope, &[b.id]).await.expect("ok");
        let c2 = meta
            .message_changes(scope, None, c1.new_state, 100)
            .await
            .expect("ok");
        assert_eq!(c2.updated.iter().map(|c| c.id).collect::<Vec<_>>(), [a.id]);
        assert_eq!(
            c2.destroyed.iter().map(|c| c.id).collect::<Vec<_>>(),
            [b.id]
        );
        let c02 = meta
            .message_changes(scope, None, s0, 100)
            .await
            .expect("ok");
        assert_eq!(c02.created.len(), 1);
        assert_eq!(c02.destroyed.len(), 1);

        // Постраничная выдача.
        let p1 = meta.message_changes(scope, None, s0, 1).await.expect("ok");
        assert!(p1.has_more);
        assert_eq!(p1.new_state, a.modseq);
        let p2 = meta
            .message_changes(scope, None, p1.new_state, 100)
            .await
            .expect("ok");
        assert!(!p2.has_more);

        // Изменения папок: вставка меняет счётчики папки.
        let mc = meta.mailbox_changes(scope, s0, 100).await.expect("ok");
        assert_eq!(mc.updated, [inbox]);

        // Устаревший или будущий since — Conflict.
        meta.truncate_changelog(scope, a.modseq).expect("ok");
        assert_eq!(
            meta.message_changes(scope, None, s0, 10).await,
            Err(Error::Conflict)
        );
        assert_eq!(
            meta.message_changes(scope, None, ModSeq(1000), 10).await,
            Err(Error::Conflict)
        );
    }

    #[tokio::test]
    async fn move_copy_trash_restore_change_modseq() {
        let clock = ManualClock::default();
        let blobs = MemBlobIndex::new(clock.shared());
        let log = MemEventLog::new(2);
        let meta = MemMetadata::new(clock.shared())
            .with_blob_index(blobs.clone())
            .with_event_log(log.clone());
        let scope = meta.account("u@example.com").await.expect("ok");
        let inbox = meta.mailbox_by_role(scope, SpecialUse::Inbox).expect("ok");
        let trash = meta.mailbox_by_role(scope, SpecialUse::Trash).expect("ok");
        let work = meta
            .create(
                scope,
                NewMailbox {
                    parent: None,
                    name: "Work".into(),
                    role: None,
                },
            )
            .await
            .expect("ok")
            .id;
        for k in [key(1), key(2)] {
            blobs
                .commit_new(TenantId::DEFAULT, &k.hash, Gen::INITIAL, 10, 1)
                .await
                .expect("ok");
        }

        let a = meta.insert(scope, inbox, new_message(1)).await.expect("ok");
        let copied = meta.copy(scope, &[a.id], work).await.expect("ok");
        assert_eq!(copied.len(), 1);
        assert_ne!(copied[0].id, a.id);
        assert!(copied[0].modseq > a.modseq);
        assert_eq!(
            blobs
                .row(TenantId::DEFAULT, &key(1).hash)
                .expect("ok")
                .refcnt,
            2
        );
        let in_work = meta.status(scope, work).await.expect("ok");
        assert_eq!(in_work.highest_modseq, copied[0].modseq);

        let moved = meta.move_to(scope, &[a.id], work).await.expect("ok");
        assert!(moved[0].modseq > copied[0].modseq);
        assert!(
            MessageStore::get(&meta, scope, a.id)
                .await
                .expect("ok")
                .is_none()
        );
        assert_eq!(meta.status(scope, inbox).await.expect("ok").messages, 0);
        assert!(meta.status(scope, inbox).await.expect("ok").highest_modseq == moved[0].modseq);
        assert_eq!(
            meta.move_to(scope, &[a.id], work).await,
            Err(Error::NotFound)
        );

        // Корзина: тот же MessageId, новый UID и ModSeq.
        let m = moved[0].id;
        let ms = meta.to_trash(scope, &[m]).await.expect("ok");
        assert!(ms > moved[0].modseq);
        let t = MessageStore::get(&meta, scope, m)
            .await
            .expect("ok")
            .expect("ok");
        assert_eq!((t.mailbox, t.modseq), (trash, ms));
        let back = meta.restore(scope, &[m]).await.expect("ok");
        assert_eq!(back[0].id, m);
        assert!(back[0].modseq > ms);
        let r = MessageStore::get(&meta, scope, m)
            .await
            .expect("ok")
            .expect("ok");
        assert_eq!(r.mailbox, work);
        assert!(r.uid > moved[0].uid);
        assert!(meta.restore(scope, &[m]).await.is_err());

        // Журнал папки: перенос — удаление из одной и создание в другой.
        let work_changes = meta
            .message_changes(scope, Some(work), ms, 100)
            .await
            .expect("ok");
        assert_eq!(work_changes.created.len(), 1);
        let trash_changes = meta
            .message_changes(scope, Some(trash), moved[0].modseq, 100)
            .await
            .expect("ok");
        assert_eq!(trash_changes.destroyed.len(), 1);

        // Окончательное удаление снимает ссылки.
        meta.expunge_final(scope, &[m, copied[0].id])
            .await
            .expect("ok");
        let row = blobs.row(TenantId::DEFAULT, &key(1).hash).expect("ok");
        assert_eq!((row.refcnt, row.state), (0, BlobState::Live));
        assert!(row.zero_since.is_some());

        let kinds: Vec<_> = log.all().into_iter().map(|e| e.event.kind).collect();
        assert!(matches!(kinds[0], CoreEventKind::MailboxCreated));
        assert!(
            kinds
                .iter()
                .any(|k| matches!(k, CoreEventKind::Moved { .. }))
        );
        assert!(
            kinds
                .iter()
                .any(|k| matches!(k, CoreEventKind::Trashed { .. }))
        );
        assert_eq!(
            kinds
                .iter()
                .filter(|k| matches!(k, CoreEventKind::Purged { .. }))
                .count(),
            2
        );
    }

    #[tokio::test]
    async fn mailboxes_and_accounts() {
        let (meta, scope, inbox) = setup().await;
        assert_eq!(MailboxStore::list(&meta, scope).await.expect("ok").len(), 5);
        assert_eq!(meta.delete(scope, inbox, true).await, Err(Error::Forbidden));
        assert_eq!(
            meta.rename(scope, inbox, None, "X").await,
            Err(Error::Forbidden)
        );
        let new = |name: &str, parent| NewMailbox {
            parent,
            name: name.into(),
            role: None,
        };
        let p = meta.create(scope, new("P", None)).await.expect("ok");
        let c = meta.create(scope, new("C", Some(p.id))).await.expect("ok");
        assert_eq!(
            meta.create(scope, new("C", Some(p.id))).await,
            Err(Error::Conflict)
        );
        assert!(meta.rename(scope, p.id, Some(c.id), "P").await.is_err());
        assert_eq!(meta.delete(scope, p.id, false).await, Err(Error::Conflict));

        // Удаление поддерева с подтверждением: письма в корзину, восстановление — во «Входящие».
        let m = meta.insert(scope, c.id, new_message(5)).await.expect("ok");
        let trash = meta.mailbox_by_role(scope, SpecialUse::Trash).expect("ok");
        let sys_child = meta.create(scope, new("S", Some(trash))).await.expect("ok");
        assert_eq!(meta.delete(scope, trash, true).await, Err(Error::Forbidden));
        meta.delete(scope, sys_child.id, false).await.expect("ok");
        meta.delete(scope, p.id, true).await.expect("ok");
        assert!(
            MailboxStore::get(&meta, scope, c.id)
                .await
                .expect("ok")
                .is_none()
        );
        let mc = meta
            .mailbox_changes(scope, ModSeq::ZERO, 100)
            .await
            .expect("ok");
        assert!(mc.destroyed.contains(&p.id) && mc.destroyed.contains(&c.id));
        let back = meta.restore(scope, &[m.id]).await.expect("ok");
        let r = MessageStore::get(&meta, scope, back[0].id)
            .await
            .expect("ok")
            .expect("ok");
        assert_eq!(r.mailbox, inbox);

        let q = meta.quota(scope.tenant, scope.account).await.expect("ok");
        assert_eq!((q.used_bytes, q.messages), (5, 1));

        // Сопоставление учётных записей: по external_id, иначе по имени входа.
        let up = |login: &str, ext: Option<&str>| AccountUpsert {
            domain: DomainId::new(),
            login: login.into(),
            display_name: "N".into(),
            external_id: ext.map(Into::into),
            enabled: true,
        };
        let t = TenantId::DEFAULT;
        let x = meta
            .upsert(t, up("X@example.com", Some("g1")))
            .await
            .expect("ok");
        let renamed = meta
            .upsert(t, up("y@example.com", Some("g1")))
            .await
            .expect("ok");
        assert_eq!(renamed.id, x.id);
        assert_eq!(
            meta.by_login(t, "Y@EXAMPLE.COM")
                .await
                .expect("ok")
                .map(|a| a.id),
            Some(x.id)
        );
        assert_eq!(
            meta.upsert(t, up("y@example.com", Some("g2"))).await,
            Err(Error::Conflict)
        );
        assert!(
            meta.by_login(t, "x@example.com")
                .await
                .expect("ok")
                .is_none()
        );
        let other = AccountScope {
            tenant: TenantId::new(),
            account: scope.account,
        };
        assert_eq!(MailboxStore::list(&meta, other).await, Err(Error::NotFound));
    }
}
