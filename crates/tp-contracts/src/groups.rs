//! Группы рассылки (CONTRACTS.md#Groups; SRS 4.5, решение 2026-11-01-borrowings-security).
//!
//! Группы формируются из групп каталога (SRS 5.1, синхронизация — `tp-auth`, AUTH-07) или создаются через Admin API
//! (ADM-07): участники (учётные записи и вложенные группы), ограничение круга отправителей, модераторы, скрытие из GAL.
//! При приёме (`tp-smtp`, SMTP-16) группа-адресат проверяет отправителя ([`GroupStore::sender_allowed`]), письмо
//! модерируемой группы ставится в очередь модерации ([`GroupStore::hold`]), иначе группа раскрывается
//! ([`GroupStore::expand`]) с защитой от циклов. Ограничения и модерация действуют для группы-адресата; вложенные
//! группы при раскрытии их не проверяют.
//!
//! Реализация — `tp-meta` (META-21, рекурсивный CTE), in-memory — `tp-testkit`.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::{Error, Result};
use crate::ids::{AccountId, BlobKey, GroupId, HeldId, TenantId};

/// Участник группы.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GroupMember {
    /// Учётная запись организации.
    Account(AccountId),
    /// Вложенная группа (раскрывается рекурсивно).
    Group(GroupId),
}

/// Кто может писать в группу (SRS 4.5).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SenderRestriction {
    /// Без ограничений, в том числе внешние отправители.
    #[default]
    Anyone,
    /// Только учётные записи организации.
    Internal,
    /// Только перечисленные учётные записи и участники перечисленных групп (с раскрытием вложенных).
    Only {
        /// Учётные записи.
        accounts: Vec<AccountId>,
        /// Группы.
        groups: Vec<GroupId>,
    },
}

/// Группа рассылки.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DistributionGroup {
    /// Идентификатор.
    pub id: GroupId,
    /// Организация.
    pub tenant: TenantId,
    /// Отображаемое имя: 1–255 символов без управляющих.
    pub name: String,
    /// Адрес рассылки, уникален в организации без учёта регистра; `None` — группа без почты (только вложенная).
    pub address: Option<String>,
    /// Идентификатор в каталоге, уникален в организации; `None` — группа создана через Admin API.
    pub external_id: Option<String>,
    /// Участники. Неизвестные вложенные группы при раскрытии пропускаются (порядок синхронизации каталога
    /// не гарантирован); активность учётных записей проверяет вызывающий.
    pub members: Vec<GroupMember>,
    /// Ограничение круга отправителей.
    pub senders: SenderRestriction,
    /// Модераторы: непусто — группа модерируемая, письма не от модераторов ждут одобрения.
    pub moderators: Vec<AccountId>,
    /// Скрыта из адресной книги (GAL).
    pub hidden_from_gal: bool,
}

impl DistributionGroup {
    /// Проверка полей: имя 1–255 символов без управляющих; адрес — непустой, без пробельных и управляющих,
    /// с `@`, ≤ 320 байт; `external_id` — непустой, ≤ 255 байт. Иначе `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        let n = self.name.chars().count();
        if n == 0 || n > 255 || self.name.chars().any(char::is_control) {
            return Err(Error::invalid("invalid group name"));
        }
        if let Some(a) = &self.address
            && (a.len() > 320
                || !a.contains('@')
                || a.chars().any(|c| c.is_whitespace() || c.is_control()))
        {
            return Err(Error::invalid(format!("invalid group address {a:?}")));
        }
        if let Some(x) = &self.external_id
            && (x.is_empty() || x.len() > 255)
        {
            return Err(Error::invalid("invalid group external id"));
        }
        Ok(())
    }

    /// Группа модерируемая (есть модераторы).
    pub fn moderated(&self) -> bool {
        !self.moderators.is_empty()
    }

    /// Письмо отправителя `sender` (`None` — внешний) ждёт одобрения: группа модерируемая и отправитель не модератор.
    pub fn requires_moderation(&self, sender: Option<AccountId>) -> bool {
        self.moderated() && !sender.is_some_and(|s| self.moderators.contains(&s))
    }
}

/// Письмо в очереди модерации. Ссылка на объект учтена вызывающим и освобождается им после решения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldMessage {
    /// Идентификатор (назначает вызывающий, `HeldId::new()`).
    pub id: HeldId,
    /// Группа-адресат.
    pub group: GroupId,
    /// Отправитель конверта (`MAIL FROM`).
    pub sender: String,
    /// Учётная запись отправителя; `None` — внешний.
    pub sender_account: Option<AccountId>,
    /// Message-ID без угловых скобок.
    pub message_id: Option<String>,
    /// Объект с письмом (RFC 5322, как принято).
    pub blob: BlobKey,
    /// Размер письма, байт.
    pub size: u64,
    /// Момент постановки.
    pub held_at: OffsetDateTime,
}

/// Группы рассылки организации и очередь модерации. Всё — в контексте `tenant`.
#[async_trait]
pub trait GroupStore: Send + Sync + 'static {
    /// Группа по идентификатору.
    async fn get(&self, tenant: TenantId, id: GroupId) -> Result<Option<DistributionGroup>>;

    /// Группа по адресу рассылки (без учёта регистра).
    async fn by_address(
        &self,
        tenant: TenantId,
        address: &str,
    ) -> Result<Option<DistributionGroup>>;

    /// Группа по идентификатору в каталоге (синхронизация).
    async fn by_external_id(
        &self,
        tenant: TenantId,
        external_id: &str,
    ) -> Result<Option<DistributionGroup>>;

    /// Все группы организации по возрастанию `id` (Admin API, GAL — с фильтром `hidden_from_gal`).
    async fn list(&self, tenant: TenantId) -> Result<Vec<DistributionGroup>>;

    /// Создаёт или заменяет группу целиком (по `group.id`, организация — `group.tenant`).
    /// Некорректные поля — `Error::Invalid` ([`DistributionGroup::validate`]); адрес или `external_id`
    /// занят другой группой — `Error::Conflict`.
    async fn put(&self, group: DistributionGroup) -> Result<()>;

    /// Удаляет группу и возвращает её письма из очереди модерации (вызывающий освобождает ссылки и уведомляет
    /// отправителей). Ссылки на группу в других группах становятся неизвестными участниками.
    /// Отсутствующая группа — не ошибка (пустой список).
    async fn remove(&self, tenant: TenantId, id: GroupId) -> Result<Vec<HeldMessage>>;

    /// Учётные записи группы с раскрытием вложенных групп: каждая группа — один раз (циклы не зацикливают),
    /// по возрастанию без повторов. Нет группы `id` — `Error::NotFound`.
    async fn expand(&self, tenant: TenantId, id: GroupId) -> Result<Vec<AccountId>>;

    /// Может ли `sender` (`None` — внешний отправитель) писать в `group`. По умолчанию — по
    /// [`SenderRestriction`] через [`GroupStore::expand`] (неизвестные группы списка пропускаются).
    async fn sender_allowed(
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
        for g in groups {
            match self.expand(tenant, *g).await {
                Ok(members) if members.binary_search(&sender).is_ok() => return Ok(true),
                Ok(_) | Err(Error::NotFound) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(false)
    }

    /// Ставит письмо в очередь модерации. Нет группы — `Error::NotFound`; занятый `id` — `Error::Conflict`.
    async fn hold(&self, tenant: TenantId, message: HeldMessage) -> Result<()>;

    /// Очередь модерации организации или одной группы по (`held_at`, `id`), не более `limit`.
    async fn held(
        &self,
        tenant: TenantId,
        group: Option<GroupId>,
        limit: usize,
    ) -> Result<Vec<HeldMessage>>;

    /// Забирает письмо из очереди для решения модератора (доставить или отклонить — вызывающий).
    /// Нет письма (в том числе уже решено другим модератором) — `Error::NotFound`.
    async fn take_held(&self, tenant: TenantId, id: HeldId) -> Result<HeldMessage>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[test]
    fn object_safe() {
        fn _store(_: std::sync::Arc<dyn GroupStore>) {}
    }

    fn group(name: &str) -> DistributionGroup {
        DistributionGroup {
            id: GroupId::new(),
            tenant: TenantId::DEFAULT,
            name: name.into(),
            address: Some(format!("{name}@example.org")),
            external_id: None,
            members: Vec::new(),
            senders: SenderRestriction::Anyone,
            moderators: Vec::new(),
            hidden_from_gal: false,
        }
    }

    #[test]
    fn validate() {
        assert!(group("all").validate().is_ok());
        let bad = [
            DistributionGroup {
                name: String::new(),
                ..group("x")
            },
            DistributionGroup {
                name: "a\nb".into(),
                ..group("x")
            },
            DistributionGroup {
                address: Some("no-at".into()),
                ..group("x")
            },
            DistributionGroup {
                address: Some("a b@example.org".into()),
                ..group("x")
            },
            DistributionGroup {
                external_id: Some(String::new()),
                ..group("x")
            },
        ];
        for g in bad {
            assert!(matches!(g.validate(), Err(Error::Invalid(_))), "{g:?}");
        }
        let mailless = DistributionGroup {
            address: None,
            ..group("inner")
        };
        assert!(mailless.validate().is_ok());
    }

    #[test]
    fn moderation() {
        let (m, s) = (AccountId::new(), AccountId::new());
        let mut g = group("board");
        assert!(!g.moderated() && !g.requires_moderation(None));
        g.moderators = vec![m];
        assert!(g.moderated());
        assert!(g.requires_moderation(None));
        assert!(g.requires_moderation(Some(s)));
        assert!(
            !g.requires_moderation(Some(m)),
            "модератор пишет без одобрения"
        );
    }

    #[test]
    fn serde_shape() {
        let a = AccountId::new();
        let json = serde_json::to_value(GroupMember::Account(a)).expect("ser");
        assert_eq!(json["account"], a.to_string());
        assert_eq!(
            serde_json::to_value(SenderRestriction::Internal).expect("ser"),
            "internal"
        );
    }

    /// Хранилище только с `expand` — проверка `sender_allowed` по умолчанию.
    struct Expand(Mutex<BTreeMap<GroupId, Vec<AccountId>>>);

    #[async_trait]
    impl GroupStore for Expand {
        async fn get(&self, _: TenantId, _: GroupId) -> Result<Option<DistributionGroup>> {
            Ok(None)
        }
        async fn by_address(&self, _: TenantId, _: &str) -> Result<Option<DistributionGroup>> {
            Ok(None)
        }
        async fn by_external_id(&self, _: TenantId, _: &str) -> Result<Option<DistributionGroup>> {
            Ok(None)
        }
        async fn list(&self, _: TenantId) -> Result<Vec<DistributionGroup>> {
            Ok(Vec::new())
        }
        async fn put(&self, _: DistributionGroup) -> Result<()> {
            Ok(())
        }
        async fn remove(&self, _: TenantId, _: GroupId) -> Result<Vec<HeldMessage>> {
            Ok(Vec::new())
        }
        async fn expand(&self, _: TenantId, id: GroupId) -> Result<Vec<AccountId>> {
            self.0
                .lock()
                .map_err(|_| Error::internal("poisoned"))?
                .get(&id)
                .cloned()
                .ok_or(Error::NotFound)
        }
        async fn hold(&self, _: TenantId, _: HeldMessage) -> Result<()> {
            Ok(())
        }
        async fn held(
            &self,
            _: TenantId,
            _: Option<GroupId>,
            _: usize,
        ) -> Result<Vec<HeldMessage>> {
            Ok(Vec::new())
        }
        async fn take_held(&self, _: TenantId, _: HeldId) -> Result<HeldMessage> {
            Err(Error::NotFound)
        }
    }

    #[test]
    fn sender_allowed_default() {
        futures::executor::block_on(async {
            let (listed, member, other) = (AccountId::new(), AccountId::new(), AccountId::new());
            let (allowed, missing) = (GroupId::new(), GroupId::new());
            let mut members = vec![member, AccountId::new()];
            members.sort();
            let store = Expand(Mutex::new(BTreeMap::from([(allowed, members)])));
            let t = TenantId::DEFAULT;
            let mut g = group("team");
            for s in [None, Some(other)] {
                assert_eq!(store.sender_allowed(t, &g, s).await, Ok(true));
            }
            g.senders = SenderRestriction::Internal;
            assert_eq!(store.sender_allowed(t, &g, None).await, Ok(false));
            assert_eq!(store.sender_allowed(t, &g, Some(other)).await, Ok(true));
            g.senders = SenderRestriction::Only {
                accounts: vec![listed],
                groups: vec![missing, allowed],
            };
            assert_eq!(store.sender_allowed(t, &g, None).await, Ok(false));
            assert_eq!(store.sender_allowed(t, &g, Some(listed)).await, Ok(true));
            assert_eq!(store.sender_allowed(t, &g, Some(member)).await, Ok(true));
            assert_eq!(store.sender_allowed(t, &g, Some(other)).await, Ok(false));
        });
    }
}
