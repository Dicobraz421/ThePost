//! Группы рассылки из каталога (AUTH-07; SRS 4.5, 5.1): инкрементальная синхронизация в [`GroupStore`].
//!
//! Порции изменений групп ([`LdapDirectory`], курсор — как у [`Directory`](tp_contracts::auth::Directory))
//! переводятся в [`DistributionGroup`]: DN участников, модераторов и разрешённых отправителей разрешаются
//! в учётные записи (по идентификатору каталога в [`AccountStore`], имя входа — только для учётных записей без
//! него) и группы (по `external_id` в [`GroupStore`]).
//! Почтовые контакты-участники (DN по сопоставлению [`ContactMap`](super::ContactMap)) — по `external_id` в
//! [`ContactStore`]: группа добавляется в `MailContact::groups` (раскрытие группы — и им), выбывшим — снимается.
//! Вложенная группа, ещё не выданная каталогом (порядок не гарантирован), создаётся заготовкой без
//! участников и заполняется своей записью. Учётная запись, которой ещё нет в метаданных, пропускается:
//! её подхватит следующее изменение группы или полная синхронизация.
//!
//! Поля, не сопоставленные в секции ([`GroupMap`]: `moderators`, `senders`, `internal_only`, `hidden`), каталог
//! не меняет: их задаёт Admin API (ADM-07). Удаление группы — по надгробию (режим USN) или полной
//! синхронизацией (`cursor = None`): группы каталога, которых нет в выгрузке, удаляются. Письма их очереди
//! модерации возвращаются вызывающему ([`GroupSyncReport::held`]) — он освобождает ссылки и уведомляет
//! отправителей.

use std::collections::HashMap;
use std::sync::Arc;

use tp_contracts::addressbook::ContactStore;
use tp_contracts::auth::{DirChange, DirCursor};
use tp_contracts::groups::{
    DistributionGroup, GroupMember, GroupStore, HeldMessage, SenderRestriction,
};
use tp_contracts::meta::AccountStore;
use tp_contracts::{AccountId, ContactId, Error, GroupId, Result};

use super::{LdapDirectory, Query, RawGroup, external_id, first};

/// Итог синхронизации групп.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupSyncReport {
    /// Позиция для следующего запуска (сохраняет вызывающий).
    pub cursor: DirCursor,
    /// Курсор устарел: выполнена полная синхронизация.
    pub full: bool,
    /// Созданные или обновлённые группы (без заготовок вложенных).
    pub upserted: usize,
    /// Удалённые группы.
    pub removed: usize,
    /// Пропущенные записи: некорректные поля или занятый другой группой адрес (в журнале — предупреждение).
    pub skipped: usize,
    /// Письма из очередей модерации удалённых групп.
    pub held: Vec<HeldMessage>,
}

/// Запись каталога, на которую ссылается группа.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Account(AccountId),
    Group(GroupId),
    Contact(ContactId),
}

/// Синхронизация групп рассылки организации каталога в [`GroupStore`].
#[derive(Clone)]
pub struct GroupSync {
    dir: LdapDirectory,
    accounts: Arc<dyn AccountStore>,
    groups: Arc<dyn GroupStore>,
    contacts: Arc<dyn ContactStore>,
}

impl std::fmt::Debug for GroupSync {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupSync")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

/// Состояние одного запуска: разрешённые DN.
#[derive(Default)]
struct Run {
    targets: HashMap<String, Option<Target>>,
}

impl GroupSync {
    /// Синхронизация каталога `dir` (организация — его) с учётными записями `accounts`, группами `groups` и
    /// почтовыми контактами `contacts`.
    pub fn new(
        dir: LdapDirectory,
        accounts: Arc<dyn AccountStore>,
        groups: Arc<dyn GroupStore>,
        contacts: Arc<dyn ContactStore>,
    ) -> Self {
        GroupSync {
            dir,
            accounts,
            groups,
            contacts,
        }
    }

    /// Изменения после `cursor` до конца порциями `page_size`; `None` — полная синхронизация с удалением
    /// групп каталога, которых нет в выгрузке. Устаревший курсор — полная синхронизация (`full`).
    /// Ошибка каталога или хранилища прерывает запуск: применённое сохранено, курсор прежний.
    pub async fn run(&self, cursor: Option<&DirCursor>) -> Result<GroupSyncReport> {
        let limit = usize::try_from(self.dir.cfg.page_size).unwrap_or(1).max(1);
        let (mut page, full) = match self.dir.raw_groups_since(cursor, limit).await {
            Err(Error::Conflict) if cursor.is_some() => {
                tracing::warn!("LDAP group sync: stale cursor, full resync");
                (self.dir.raw_groups_since(None, limit).await?, true)
            }
            r => (r?, false),
        };
        let full_listing = cursor.is_none() || full;
        let mut report = GroupSyncReport {
            cursor: page.next.clone(),
            full,
            upserted: 0,
            removed: 0,
            skipped: 0,
            held: Vec::new(),
        };
        let mut run = Run::default();
        let mut seen: Vec<String> = Vec::new();
        loop {
            for change in std::mem::take(&mut page.changes) {
                match change {
                    DirChange::Upsert(raw) => {
                        seen.push(raw.external_id.clone());
                        if self.apply(raw, &mut run).await? {
                            report.upserted += 1;
                        } else {
                            report.skipped += 1;
                        }
                    }
                    DirChange::Removed { external_id } => {
                        let tenant = self.dir.tenant;
                        if let Some(g) = self.groups.by_external_id(tenant, &external_id).await? {
                            self.remove(g.id, &mut report).await?;
                        }
                    }
                }
            }
            report.cursor = page.next.clone();
            if !page.has_more {
                break;
            }
            page = self.dir.raw_groups_since(Some(&page.next), limit).await?;
        }
        if full_listing {
            self.remove_unseen(seen, &mut report).await?;
        }
        Ok(report)
    }

    /// Полная выгрузка: удаляет группы каталога, которых в ней нет. Пустая выгрузка ничего не удаляет
    /// (ошибка базы поиска или фильтра не должна стирать все группы).
    async fn remove_unseen(
        &self,
        mut seen: Vec<String>,
        report: &mut GroupSyncReport,
    ) -> Result<()> {
        let tenant = self.dir.tenant;
        let stored = self.groups.list(tenant).await?;
        if seen.is_empty() {
            if stored.iter().any(|g| g.external_id.is_some()) {
                tracing::warn!("LDAP group sync: directory returned no groups, removal skipped");
            }
            return Ok(());
        }
        seen.sort_unstable();
        for g in stored {
            let Some(x) = &g.external_id else {
                continue;
            };
            if seen.binary_search(x).is_err() {
                self.remove(g.id, report).await?;
            }
        }
        Ok(())
    }

    /// Удаляет группу и снимает её с почтовых контактов.
    async fn remove(&self, id: GroupId, report: &mut GroupSyncReport) -> Result<()> {
        self.set_contacts(id, &[]).await?;
        report
            .held
            .extend(self.groups.remove(self.dir.tenant, id).await?);
        report.removed += 1;
        Ok(())
    }

    /// Состав группы `id` из почтовых контактов — ровно `contacts` (`MailContact::groups`).
    async fn set_contacts(&self, id: GroupId, contacts: &[ContactId]) -> Result<()> {
        let tenant = self.dir.tenant;
        for mut c in self.contacts.group_contacts(tenant, id).await? {
            if !contacts.contains(&c.id) {
                c.groups.retain(|g| *g != id);
                self.contacts.put_contact(c).await?;
            }
        }
        for cid in contacts {
            if let Some(mut c) = self.contacts.contact(tenant, *cid).await?
                && !c.groups.contains(&id)
            {
                c.groups.push(id);
                c.groups.sort_unstable();
                self.contacts.put_contact(c).await?;
            }
        }
        Ok(())
    }

    /// Применяет запись группы; `false` — запись пропущена (некорректна или конфликтует).
    async fn apply(&self, raw: RawGroup, run: &mut Run) -> Result<bool> {
        let tenant = self.dir.tenant;
        let m = &self.dir.cfg.groups;
        // Сначала ссылки: ссылка группы на себя создаёт заготовку, её идентификатор нужен ниже.
        let mut members = Vec::with_capacity(raw.members.len());
        let mut contacts = Vec::new();
        for dn in &raw.members {
            match self.target(dn, run).await? {
                Some(Target::Account(a)) => members.push(GroupMember::Account(a)),
                Some(Target::Group(g)) => members.push(GroupMember::Group(g)),
                Some(Target::Contact(c)) => contacts.push(c),
                None => {}
            }
        }
        members.sort_unstable();
        members.dedup();
        contacts.sort_unstable();
        contacts.dedup();
        let mut moderators = Vec::new();
        for dn in &raw.moderators {
            if let Some(Target::Account(a)) = self.target(dn, run).await? {
                moderators.push(a);
            }
        }
        moderators.sort_unstable();
        moderators.dedup();
        let (mut accounts, mut groups) = (Vec::new(), Vec::new());
        for dn in &raw.senders {
            match self.target(dn, run).await? {
                Some(Target::Account(a)) => accounts.push(a),
                Some(Target::Group(g)) => groups.push(g),
                Some(Target::Contact(_)) | None => {}
            }
        }
        accounts.sort_unstable();
        accounts.dedup();
        groups.sort_unstable();
        groups.dedup();

        let existing = self.groups.by_external_id(tenant, &raw.external_id).await?;
        let senders = if !raw.senders.is_empty() {
            // Ссылки есть, но не разрешились — никто: безопаснее, чем снять ограничение.
            Some(SenderRestriction::Only { accounts, groups })
        } else if raw.internal_only == Some(true) {
            Some(SenderRestriction::Internal)
        } else if !m.senders.is_empty() || m.internal_only.is_some() {
            Some(SenderRestriction::Anyone)
        } else {
            None
        };
        let group = DistributionGroup {
            id: existing.as_ref().map_or_else(GroupId::new, |g| g.id),
            tenant,
            name: raw.name,
            address: raw.email,
            external_id: Some(raw.external_id),
            members,
            senders: senders
                .or_else(|| existing.as_ref().map(|g| g.senders.clone()))
                .unwrap_or_default(),
            moderators: if m.moderators.is_some() {
                moderators
            } else {
                existing
                    .as_ref()
                    .map(|g| g.moderators.clone())
                    .unwrap_or_default()
            },
            hidden_from_gal: raw
                .hidden
                .or_else(|| existing.as_ref().map(|g| g.hidden_from_gal))
                .unwrap_or(false),
        };
        let id = group.id;
        if !self.put(group).await? {
            return Ok(false);
        }
        self.set_contacts(id, &contacts).await?;
        Ok(true)
    }

    /// Сохраняет группу; некорректная или с занятым адресом — пропуск с предупреждением.
    async fn put(&self, group: DistributionGroup) -> Result<bool> {
        let external_id = group.external_id.clone().unwrap_or_default();
        match self.groups.put(group).await {
            Ok(()) => Ok(true),
            Err(e @ (Error::Invalid(_) | Error::Conflict)) => {
                tracing::warn!(%external_id, error = %e, "LDAP group skipped");
                Ok(false)
            }
            Err(e) => Err(e),
        }
    }

    /// Учётная запись или группа по DN (с кэшем запуска); не найдена или вне фильтров — `None`.
    async fn target(&self, dn: &str, run: &mut Run) -> Result<Option<Target>> {
        let key = dn.to_ascii_lowercase();
        if let Some(t) = run.targets.get(&key) {
            return Ok(*t);
        }
        let t = match self.group_target(dn).await? {
            Some(t) => Some(t),
            None => match self.account_target(dn).await? {
                Some(t) => Some(t),
                None => self.contact_target(dn).await?,
            },
        };
        if t.is_none() {
            tracing::debug!(%dn, "LDAP group reference not resolved");
        }
        run.targets.insert(key, t);
        Ok(t)
    }

    /// Почтовый контакт по DN (по `external_id` в [`ContactStore`]); контакты не сопоставлены — `None`.
    async fn contact_target(&self, dn: &str) -> Result<Option<Target>> {
        let Some(m) = &self.dir.cfg.contacts else {
            return Ok(None);
        };
        let rows = self
            .dir
            .source
            .search(&Query {
                base: dn.to_owned(),
                base_only: true,
                filter: m.filter.clone(),
                attrs: vec![m.id.clone()],
                show_deleted: false,
            })
            .await?;
        let Some(x) = rows.first().and_then(|e| external_id(e, &m.id)) else {
            return Ok(None);
        };
        Ok(self
            .contacts
            .contact_by_external_id(self.dir.tenant, &x)
            .await?
            .map(|c| Target::Contact(c.id)))
    }

    async fn group_target(&self, dn: &str) -> Result<Option<Target>> {
        let tenant = self.dir.tenant;
        let m = &self.dir.cfg.groups;
        let rows = self
            .dir
            .source
            .search(&Query {
                base: dn.to_owned(),
                base_only: true,
                filter: m.filter.clone(),
                attrs: vec![m.id.clone(), m.name.clone()],
                show_deleted: false,
            })
            .await?;
        let Some(e) = rows.first() else {
            return Ok(None);
        };
        let Some(x) = external_id(e, &m.id) else {
            return Ok(None);
        };
        if let Some(g) = self.groups.by_external_id(tenant, &x).await? {
            return Ok(Some(Target::Group(g.id)));
        }
        // Заготовка: участники и признаки придут с записью самой группы.
        let stub = DistributionGroup {
            id: GroupId::new(),
            tenant,
            name: first(e, &m.name).unwrap_or_else(|| x.clone()),
            address: None,
            external_id: Some(x),
            members: Vec::new(),
            senders: SenderRestriction::default(),
            moderators: Vec::new(),
            hidden_from_gal: false,
        };
        let id = stub.id;
        Ok(self.put(stub).await?.then_some(Target::Group(id)))
    }

    /// Учётная запись по DN: по идентификатору каталога ([`AccountStore::by_external_id`]) — переименование
    /// входа не теряет членство; по имени входа — только учётная запись без `external_id` (заведена до
    /// синхронизации каталога), чтобы не сопоставить чужую запись каталога с тем же именем.
    async fn account_target(&self, dn: &str) -> Result<Option<Target>> {
        let tenant = self.dir.tenant;
        let m = &self.dir.cfg.accounts;
        let rows = self
            .dir
            .source
            .search(&Query {
                base: dn.to_owned(),
                base_only: true,
                filter: m.filter.clone(),
                attrs: vec![m.id.clone(), m.login.clone()],
                show_deleted: false,
            })
            .await?;
        let Some(e) = rows.first() else {
            return Ok(None);
        };
        if let Some(x) = external_id(e, &m.id)
            && let Some(a) = self.accounts.by_external_id(tenant, &x).await?
        {
            return Ok(Some(Target::Account(a.id)));
        }
        let Some(login) = first(e, &m.login) else {
            return Ok(None);
        };
        Ok(self
            .accounts
            .by_login(tenant, &login)
            .await?
            .filter(|a| a.external_id.is_none())
            .map(|a| Target::Account(a.id)))
    }
}
