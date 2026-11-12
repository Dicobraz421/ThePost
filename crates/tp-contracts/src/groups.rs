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
//!
//! Решение модератора (CTR-31): письмо забирается из очереди ([`GroupStore::take_held`]) и передаётся [`Moderation`]
//! (реализация — `tp-smtp`): одобрено — доставка участникам группы, отклонено — уведомление отправителю.
//! Ошибка решения — вызывающий возвращает письмо в очередь ([`GroupStore::hold`]).
//!
//! Одна очередь модерации для групп и правил потока почты (CTR-43, решение 2026-11-11-stage2-followups п. 9): у письма
//! источник задержки ([`HoldSource`]) и модераторы ([`Moderator`]); письмо правила — без группы, одобрение доставляет
//! его исходным получателям ([`HeldMessage::recipients`]).

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

/// Наибольшая длина имени правила потока в очереди модерации, символов.
pub const MAX_RULE_NAME_LEN: usize = 255;

/// Модератор задержанного письма (CTR-43).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Moderator {
    /// Учётная запись.
    Account(AccountId),
    /// Участники группы (с раскрытием вложенных).
    Group(GroupId),
}

/// Источник задержки письма (CTR-43).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldSource {
    /// Модерируемая группа рассылки [`HeldMessage::group`].
    #[default]
    Group,
    /// Правило потока почты (`FlowVerdict::Moderate`); письмо без группы.
    FlowRule {
        /// Имя правила (1–[`MAX_RULE_NAME_LEN`] символов без управляющих).
        rule: String,
    },
}

/// Письмо в очереди модерации. Ссылка на объект учтена вызывающим и освобождается им после решения.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeldMessage {
    /// Идентификатор (назначает вызывающий, `HeldId::new()`).
    pub id: HeldId,
    /// Группа-адресат; `None` — письмо задержано правилом потока (CTR-43).
    pub group: Option<GroupId>,
    /// Источник задержки (CTR-43).
    #[serde(default)]
    pub source: HoldSource,
    /// Модераторы (CTR-43): у письма правила — не пусто; у письма группы — снимок модераторов группы (пусто — модераторы
    /// группы на момент решения).
    #[serde(default)]
    pub moderators: Vec<Moderator>,
    /// Исходные получатели конверта (CTR-43): у письма правила — не пусто, им доставляется одобренное письмо; у письма
    /// группы — адрес группы или пусто.
    #[serde(default)]
    pub recipients: Vec<String>,
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

impl HeldMessage {
    /// Проверка источника (CTR-43): [`HoldSource::Group`] — группа задана; [`HoldSource::FlowRule`] — без группы, имя
    /// правила 1–[`MAX_RULE_NAME_LEN`] символов без управляющих, модераторы и получатели не пусты. Иначе `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        match &self.source {
            HoldSource::Group if self.group.is_some() => Ok(()),
            HoldSource::Group => Err(Error::invalid("письмо группы без группы")),
            HoldSource::FlowRule { rule } => {
                if self.group.is_some() {
                    return Err(Error::invalid(
                        "письмо правила потока не относится к группе",
                    ));
                }
                if rule.trim().is_empty()
                    || rule.chars().count() > MAX_RULE_NAME_LEN
                    || rule.chars().any(char::is_control)
                {
                    return Err(Error::invalid("имя правила потока — 1..=255 символов"));
                }
                if self.moderators.is_empty() || self.recipients.is_empty() {
                    return Err(Error::invalid(
                        "у письма правила потока нет модераторов или получателей",
                    ));
                }
                Ok(())
            }
        }
    }
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

    /// Ставит письмо в очередь модерации: письмо группы — нет группы — `Error::NotFound`; письмо правила потока
    /// (CTR-43, без группы); [`HeldMessage::validate`] — `Error::Invalid`; занятый `id` — `Error::Conflict`.
    async fn hold(&self, tenant: TenantId, message: HeldMessage) -> Result<()>;

    /// Страница очереди модерации организации (письма групп и правил) или одной группы по (`held_at`, `id`) строго
    /// после курсора `after` (CTR-55; `None` — с начала), не более `limit`. Курсор следующей страницы —
    /// [`HeldCursor::of`] последнего письма страницы; страница короче `limit` — последняя. Письма, поставленные или
    /// забранные между страницами, не дают ни пропусков, ни повторов остальных. Правило — [`held_page`].
    async fn held(
        &self,
        tenant: TenantId,
        group: Option<GroupId>,
        after: Option<HeldCursor>,
        limit: usize,
    ) -> Result<Vec<HeldMessage>>;

    /// Забирает письмо из очереди для решения модератора (доставить или отклонить — вызывающий).
    /// Нет письма (в том числе уже решено другим модератором) — `Error::NotFound`.
    async fn take_held(&self, tenant: TenantId, id: HeldId) -> Result<HeldMessage>;
}

/// Курсор очереди модерации (CTR-55): место последнего письма предыдущей страницы в порядке (`held_at`, `id`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct HeldCursor {
    /// Время постановки последнего письма страницы.
    pub held_at: OffsetDateTime,
    /// Его идентификатор — различает письма с одинаковым `held_at`.
    pub id: HeldId,
}

impl HeldCursor {
    /// Курсор письма `message`.
    pub fn of(message: &HeldMessage) -> Self {
        Self {
            held_at: message.held_at,
            id: message.id,
        }
    }

    /// Письмо стоит в очереди строго после курсора.
    pub fn precedes(&self, message: &HeldMessage) -> bool {
        *self < Self::of(message)
    }
}

/// Страница очереди для [`GroupStore::held`]: письма `queue` (в любом порядке) строго после `after` по
/// (`held_at`, `id`), не более `limit`. Для реализаций в памяти и проверки реализаций хранилища.
pub fn held_page(
    queue: impl IntoIterator<Item = HeldMessage>,
    after: Option<HeldCursor>,
    limit: usize,
) -> Vec<HeldMessage> {
    let mut page: Vec<HeldMessage> = queue
        .into_iter()
        .filter(|m| after.is_none_or(|c| c.precedes(m)))
        .collect();
    page.sort_by_key(HeldCursor::of);
    page.truncate(limit);
    page
}

/// Наибольшая длина причины отказа модератора, символов.
pub const MAX_REJECT_REASON_LEN: usize = 500;

/// Причина отказа: одна строка из 1–[`MAX_REJECT_REASON_LEN`] символов без управляющих; иначе — `Error::Invalid`.
pub fn validate_reject_reason(reason: &str) -> Result<()> {
    if reason.is_empty()
        || reason.chars().count() > MAX_REJECT_REASON_LEN
        || reason.chars().any(char::is_control)
    {
        return Err(Error::invalid(format!(
            "причина отказа — одна строка из 1..={MAX_REJECT_REASON_LEN} символов"
        )));
    }
    Ok(())
}

/// Решения модератора группы рассылки (CTR-31; вместо временного трейта `tp-admin`, ADM-07). Реализация — `tp-smtp`;
/// in-memory (запись решений) — `tp-testkit`. Письмо вызывающий уже забрал из очереди ([`GroupStore::take_held`]);
/// успешное решение освобождает ссылку на объект письма, ошибка — письмо возвращают в очередь, ссылка остаётся.
#[async_trait]
pub trait Moderation: Send + Sync + 'static {
    /// Одобрено: доставить письмо без повторной модерации — участникам `group` (раскрытие [`GroupStore::expand`] с
    /// вложенными, приём в ящики, журнал доставки) или, для письма правила потока (`group = None`, CTR-43), исходным
    /// получателям [`HeldMessage::recipients`] (локальные — в ящики, внешние — в очередь). Проверка — [`check_delivery`].
    async fn deliver(
        &self,
        tenant: TenantId,
        group: Option<&DistributionGroup>,
        message: &HeldMessage,
    ) -> Result<()>;

    /// Отклонено: уведомить отправителя (DSN, RFC 3464) с причиной `reason` ([`validate_reject_reason`], иначе
    /// `Error::Invalid`); пустой отправитель конверта — без уведомления.
    async fn reject(&self, tenant: TenantId, message: &HeldMessage, reason: &str) -> Result<()>;
}

/// Проверка аргументов [`Moderation::deliver`] для реализаций: группа письма и организация; без группы — письмо
/// правила потока ([`HeldMessage::validate`]). Иначе `Error::Invalid`.
pub fn check_delivery(
    tenant: TenantId,
    group: Option<&DistributionGroup>,
    message: &HeldMessage,
) -> Result<()> {
    match group {
        Some(g) if g.tenant != tenant || Some(g.id) != message.group => {
            Err(Error::invalid("письмо очереди модерации — другой группы"))
        }
        Some(_) => Ok(()),
        None if message.group.is_some() => {
            Err(Error::invalid("письмо группы: для доставки нужна группа"))
        }
        None => message.validate(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[test]
    fn object_safe() {
        fn _store(_: std::sync::Arc<dyn GroupStore>) {}
        fn _moderation(_: std::sync::Arc<dyn Moderation>) {}
    }

    #[test]
    fn held_pages() {
        // Пять писем, у трёх одинаковое время: страницы по два — без пропусков и повторов.
        let at = |s: i64| OffsetDateTime::UNIX_EPOCH + time::Duration::seconds(s);
        let queue: Vec<HeldMessage> = [3, 1, 1, 2, 1]
            .into_iter()
            .map(|s| HeldMessage {
                held_at: at(s),
                ..held(None, HoldSource::FlowRule { rule: "r".into() })
            })
            .collect();
        let mut expected = queue.clone();
        expected.sort_by_key(HeldCursor::of);
        let (mut read, mut after, mut pages) = (Vec::new(), None, 0);
        loop {
            let page = held_page(queue.clone(), after, 2);
            pages += 1;
            after = page.last().map(HeldCursor::of).or(after);
            let last = page.len() < 2;
            read.extend(page);
            if last {
                break;
            }
        }
        assert_eq!(read, expected);
        assert_eq!(pages, 3);
        assert_eq!(held_page(queue.clone(), None, 2), expected[..2]);
        assert_eq!(held_page(queue, after, 2), []);
    }

    fn held(group: Option<GroupId>, source: HoldSource) -> HeldMessage {
        HeldMessage {
            id: HeldId::new(),
            group,
            source,
            moderators: vec![Moderator::Account(AccountId::new())],
            recipients: vec!["a@example.org".into()],
            sender: "x@example.com".into(),
            sender_account: None,
            message_id: None,
            blob: BlobKey {
                tenant: TenantId::DEFAULT,
                hash: crate::ids::BlobHash([1; 32]),
                generation: crate::ids::Gen::INITIAL,
            },
            size: 1,
            held_at: OffsetDateTime::UNIX_EPOCH,
        }
    }

    /// CTR-43: источник задержки и проверка доставки письма без группы.
    #[test]
    fn held_source() {
        let g = group("Все");
        let rule = || HoldSource::FlowRule {
            rule: "Внешние вложения".into(),
        };
        let by_group = held(Some(g.id), HoldSource::Group);
        let by_rule = held(None, rule());
        assert_eq!(by_group.validate(), Ok(()));
        assert_eq!(by_rule.validate(), Ok(()));
        assert!(held(None, HoldSource::Group).validate().is_err());
        assert!(held(Some(g.id), rule()).validate().is_err());
        let unnamed = held(None, HoldSource::FlowRule { rule: " ".into() });
        assert!(unnamed.validate().is_err());
        let nobody = HeldMessage {
            moderators: Vec::new(),
            ..by_rule.clone()
        };
        assert!(nobody.validate().is_err());
        let nowhere = HeldMessage {
            recipients: Vec::new(),
            ..by_rule.clone()
        };
        assert!(nowhere.validate().is_err());

        let t = TenantId::DEFAULT;
        assert_eq!(check_delivery(t, Some(&g), &by_group), Ok(()));
        assert_eq!(check_delivery(t, None, &by_rule), Ok(()));
        assert!(check_delivery(t, None, &by_group).is_err());
        assert!(check_delivery(t, Some(&g), &by_rule).is_err());
        assert!(check_delivery(TenantId::new(), Some(&g), &by_group).is_err());

        let json = serde_json::to_string(&by_rule).expect("json");
        assert_eq!(
            serde_json::from_str::<HeldMessage>(&json).expect("parse"),
            by_rule
        );
        // Запись до CTR-43: группа задана, новых полей нет.
        let mut old: serde_json::Value = serde_json::to_value(&by_group).expect("value");
        for f in ["source", "moderators", "recipients"] {
            old.as_object_mut().expect("object").remove(f);
        }
        let parsed: HeldMessage = serde_json::from_value(old).expect("old");
        assert_eq!(
            (parsed.source, parsed.moderators.len()),
            (HoldSource::Group, 0)
        );
    }

    #[test]
    fn reject_reason() {
        assert_eq!(validate_reject_reason("off-topic"), Ok(()));
        assert_eq!(
            validate_reject_reason(&"я".repeat(MAX_REJECT_REASON_LEN)),
            Ok(())
        );
        for bad in [
            String::new(),
            "a\nb".to_owned(),
            "я".repeat(MAX_REJECT_REASON_LEN + 1),
        ] {
            assert!(validate_reject_reason(&bad).is_err(), "{bad:?}");
        }
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
            _: Option<HeldCursor>,
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
