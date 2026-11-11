//! Адресная книга организации: почтовые контакты, оргструктура, динамические группы рассылки
//! (CONTRACTS.md#AddressBook; SRS 4.5, 5.1, решение 2026-11-04-exchange-parity; CTR-35).
//!
//! - Почтовые контакты — внешние адресаты в GAL и в составе групп: из каталога (`Directory::contacts_since`,
//!   синхронизация `tp-auth`) или созданные через Admin API ([`ContactStore`]).
//! - Оргструктура (иерархическая адресная книга): дерево подразделений и оргатрибуты учётной записи — подразделение,
//!   должность, руководитель, старшинство ([`OrgStore`]); путь подразделения — `ORG` в vCard ([`unit_path`]), дерево —
//!   JMAP-расширение `urn:thepost:orgchart` и Admin API.
//! - Динамическая группа — группа рассылки `groups::DistributionGroup` (адрес, ограничения отправителей, модерация,
//!   скрытие из GAL) без статических членов и с фильтром по атрибутам ([`DynamicGroupStore`]); состав вычисляется
//!   при раскрытии ([`DynamicGroupStore::expand_dynamic`]).
//!
//! Реализация — `tp-meta` (META-33), in-memory — `tp-testkit`. Состояние учётной записи (`meta::AccountState`) здесь не
//! учитывается: отключённые ящики исключает вызывающий.

use std::collections::{BTreeMap, HashSet};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};
use crate::identity::valid_address;
use crate::ids::{AccountId, ContactId, GroupId, OrgUnitId, TenantId};

/// Наибольшая длина имени (контакта, подразделения), должности и значения атрибута, символов.
pub const MAX_NAME_LEN: usize = 255;

/// Наибольшая длина имени атрибута, байт.
pub const MAX_ATTRIBUTE_LEN: usize = 64;

/// Наибольшее число узлов фильтра динамической группы.
pub const MAX_FILTER_NODES: usize = 64;

/// Наибольшая глубина вложенности фильтра динамической группы.
pub const MAX_FILTER_DEPTH: usize = 8;

/// Атрибут фильтра с должностью учётной записи ([`OrgAttributes::title`]).
pub const ATTR_TITLE: &str = "title";

fn plain(s: &str, max: usize) -> bool {
    s.chars().count() <= max && !s.chars().any(char::is_control)
}

fn valid_name(s: &str) -> bool {
    !s.trim().is_empty() && plain(s, MAX_NAME_LEN)
}

fn valid_attribute(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_ATTRIBUTE_LEN
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

fn valid_attributes(attrs: &BTreeMap<String, String>) -> bool {
    attrs
        .iter()
        .all(|(k, v)| valid_attribute(k) && plain(v, MAX_NAME_LEN))
}

// ---------- Контакты ----------

/// Почтовый контакт — внешний адресат адресной книги.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MailContact {
    /// Идентификатор.
    pub id: ContactId,
    /// Организация.
    pub tenant: TenantId,
    /// Отображаемое имя (1–255 символов без управляющих).
    pub display_name: String,
    /// Внешний адрес ([`valid_address`]); уникален в организации без учёта регистра.
    pub email: String,
    /// Идентификатор в каталоге (уникален); `None` — создан через Admin API.
    pub external_id: Option<String>,
    /// Атрибуты адресной книги и фильтров (имя ASCII → значение).
    pub attributes: BTreeMap<String, String>,
    /// Группы рассылки, в состав которых входит контакт (по возрастанию без повторов).
    pub groups: Vec<GroupId>,
    /// Скрыт из GAL.
    pub hidden_from_gal: bool,
}

impl MailContact {
    /// Проверка: имя 1–[`MAX_NAME_LEN`] символов без управляющих, адрес — [`valid_address`], имена атрибутов — ASCII
    /// (буквы, цифры, `-`, `_`, `.`) до [`MAX_ATTRIBUTE_LEN`] байт, значения — до [`MAX_NAME_LEN`] символов; иначе
    /// `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if !valid_name(&self.display_name) {
            return Err(Error::invalid(
                "contact: display name must be 1–255 plain characters",
            ));
        }
        if !valid_address(&self.email) {
            return Err(Error::invalid(format!(
                "contact: invalid address {:?}",
                self.email
            )));
        }
        if self.external_id.as_deref().is_some_and(str::is_empty) {
            return Err(Error::invalid("contact: empty external id"));
        }
        if !valid_attributes(&self.attributes) {
            return Err(Error::invalid("contact: invalid attribute"));
        }
        Ok(())
    }
}

/// Почтовые контакты организации.
#[async_trait]
pub trait ContactStore: Send + Sync + 'static {
    /// Контакт по идентификатору.
    async fn contact(&self, tenant: TenantId, id: ContactId) -> Result<Option<MailContact>>;

    /// Контакт по адресу (без учёта регистра).
    async fn contact_by_address(
        &self,
        tenant: TenantId,
        email: &str,
    ) -> Result<Option<MailContact>>;

    /// Контакт по идентификатору в каталоге (точное совпадение).
    async fn contact_by_external_id(
        &self,
        tenant: TenantId,
        external_id: &str,
    ) -> Result<Option<MailContact>>;

    /// Контакты организации по `id`.
    async fn contacts(&self, tenant: TenantId) -> Result<Vec<MailContact>>;

    /// Создаёт или заменяет контакт целиком (`groups` сохраняется по возрастанию без повторов).
    /// Некорректный — `Error::Invalid`; адрес или `external_id` занят другим контактом — `Error::Conflict`.
    async fn put_contact(&self, contact: MailContact) -> Result<()>;

    /// Удаляет контакт; нет — `Error::NotFound`.
    async fn remove_contact(&self, tenant: TenantId, id: ContactId) -> Result<()>;

    /// Контакты в составе группы (`MailContact::groups`) по `id` — внешние получатели при раскрытии группы.
    async fn group_contacts(&self, tenant: TenantId, group: GroupId) -> Result<Vec<MailContact>>;
}

// ---------- Оргструктура ----------

/// Подразделение.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgUnit {
    /// Идентификатор.
    pub id: OrgUnitId,
    /// Организация.
    pub tenant: TenantId,
    /// Название (1–255 символов без управляющих).
    pub name: String,
    /// Вышестоящее подразделение; `None` — корневое.
    pub parent: Option<OrgUnitId>,
    /// Идентификатор в каталоге (уникален); `None` — создано через Admin API.
    pub external_id: Option<String>,
}

/// Оргатрибуты учётной записи.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OrgAttributes {
    /// Подразделение.
    pub department: Option<OrgUnitId>,
    /// Должность.
    pub title: Option<String>,
    /// Руководитель.
    pub manager: Option<AccountId>,
    /// Старшинство в подразделении (больше — выше в списке; атрибут каталога — настройка синхронизации).
    pub seniority: Option<i32>,
    /// Прочие атрибуты для фильтров динамических групп (офис, произвольные; имя ASCII → значение).
    pub attributes: BTreeMap<String, String>,
}

impl OrgAttributes {
    /// Проверка: должность — до [`MAX_NAME_LEN`] символов без управляющих, атрибуты — как у контакта
    /// ([`MailContact::validate`]); иначе `Error::Invalid`. Ссылки (подразделение, руководитель) проверяет хранилище.
    pub fn validate(&self) -> Result<()> {
        if !self.title.as_deref().is_none_or(|t| plain(t, MAX_NAME_LEN))
            || !valid_attributes(&self.attributes)
        {
            return Err(Error::invalid("org attributes: invalid title or attribute"));
        }
        Ok(())
    }

    /// Атрибуты для фильтра: `attributes` и должность под именем [`ATTR_TITLE`].
    pub fn filter_attributes(&self) -> BTreeMap<String, String> {
        let mut out = self.attributes.clone();
        if let Some(t) = &self.title {
            out.insert(ATTR_TITLE.to_owned(), t.clone());
        }
        out
    }
}

/// Учётная запись подразделения с оргатрибутами.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgMember {
    /// Учётная запись.
    pub account: AccountId,
    /// Оргатрибуты.
    pub org: OrgAttributes,
}

/// Путь подразделения от корня до `id` включительно (`ORG` в vCard: организация; подразделения); неизвестное
/// подразделение — пусто, цикл обрывается.
pub fn unit_path(units: &[OrgUnit], id: OrgUnitId) -> Vec<&OrgUnit> {
    let mut path = Vec::new();
    let mut seen = HashSet::new();
    let mut next = Some(id);
    while let Some(cur) = next {
        let Some(u) = units.iter().find(|u| u.id == cur) else {
            break;
        };
        if !seen.insert(cur) {
            break;
        }
        path.push(u);
        next = u.parent;
    }
    path.reverse();
    path
}

/// Порядок членов подразделения: по убыванию старшинства (без старшинства — в конце), затем по учётной записи.
pub fn sort_members(members: &mut [OrgMember]) {
    members.sort_by(|a, b| {
        b.org
            .seniority
            .is_some()
            .cmp(&a.org.seniority.is_some())
            .then(b.org.seniority.cmp(&a.org.seniority))
            .then(a.account.cmp(&b.account))
    });
}

/// Проверка подразделения перед `put_org_unit` среди `units` организации (прежняя версия заменяется): название, вышестоящее
/// подразделение существует и не образует цикл; иначе `Error::Invalid`.
pub fn check_unit(units: &[OrgUnit], unit: &OrgUnit) -> Result<()> {
    if !valid_name(&unit.name) {
        return Err(Error::invalid(
            "org unit: name must be 1–255 plain characters",
        ));
    }
    if unit.external_id.as_deref().is_some_and(str::is_empty) {
        return Err(Error::invalid("org unit: empty external id"));
    }
    let mut seen = HashSet::from([unit.id]);
    let mut next = unit.parent;
    while let Some(cur) = next {
        if !seen.insert(cur) {
            return Err(Error::invalid("org unit: parent cycle"));
        }
        let parent = units
            .iter()
            .find(|u| u.id == cur && u.tenant == unit.tenant)
            .ok_or_else(|| Error::invalid("org unit: unknown parent"))?;
        next = parent.parent;
    }
    Ok(())
}

/// Оргструктура организации.
#[async_trait]
pub trait OrgStore: Send + Sync + 'static {
    /// Подразделения организации по `id`.
    async fn org_units(&self, tenant: TenantId) -> Result<Vec<OrgUnit>>;

    /// Создаёт или заменяет подразделение ([`check_unit`] — `Error::Invalid`); `external_id` занят другим — `Error::Conflict`.
    async fn put_org_unit(&self, unit: OrgUnit) -> Result<()>;

    /// Удаляет подразделение: есть вложенные — `Error::Conflict`, нет — `Error::NotFound`; у учётных записей
    /// подразделения `department` сбрасывается.
    async fn remove_org_unit(&self, tenant: TenantId, id: OrgUnitId) -> Result<()>;

    /// Оргатрибуты учётной записи; не заданы — `OrgAttributes::default()`.
    async fn org(&self, tenant: TenantId, account: AccountId) -> Result<OrgAttributes>;

    /// Задаёт оргатрибуты целиком: неизвестное подразделение, руководитель — сама учётная запись, некорректные атрибуты
    /// или должность ([`OrgAttributes::validate`]) — `Error::Invalid`. `OrgAttributes::default()` удаляет запись.
    async fn set_org(&self, tenant: TenantId, account: AccountId, org: OrgAttributes)
    -> Result<()>;

    /// Учётные записи подразделения `unit` (`subtree` — с вложенными) в порядке [`sort_members`]; `None` — все
    /// учётные записи с оргатрибутами. Неизвестное подразделение — `Error::NotFound`.
    async fn org_members(
        &self,
        tenant: TenantId,
        unit: Option<OrgUnitId>,
        subtree: bool,
    ) -> Result<Vec<OrgMember>>;
}

// ---------- Динамические группы ----------

/// Фильтр состава динамической группы. Имена атрибутов — без учёта регистра ASCII, значения — без учёта регистра.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemberFilter {
    /// Значение атрибута равно `value`.
    Equals {
        /// Имя атрибута.
        attribute: String,
        /// Значение.
        value: String,
    },
    /// Значение атрибута начинается с `value`.
    Prefix {
        /// Имя атрибута.
        attribute: String,
        /// Начало значения.
        value: String,
    },
    /// Атрибут задан.
    Present {
        /// Имя атрибута.
        attribute: String,
    },
    /// Подразделение (`subtree` — с вложенными); контакты не входят.
    Unit {
        /// Подразделение.
        unit: OrgUnitId,
        /// С вложенными подразделениями.
        subtree: bool,
    },
    /// Все условия (пусто — истина).
    All(Vec<MemberFilter>),
    /// Хотя бы одно условие (пусто — ложь).
    Any(Vec<MemberFilter>),
    /// Отрицание.
    Not(Box<MemberFilter>),
}

/// Что проверяет фильтр: атрибуты и цепочка подразделений (своё и вышестоящие; у контакта — пусто).
#[derive(Debug, Clone, Copy)]
pub struct FilterTarget<'a> {
    /// Атрибуты ([`OrgAttributes::filter_attributes`] или `MailContact::attributes`).
    pub attributes: &'a BTreeMap<String, String>,
    /// Подразделение и все вышестоящие, начиная со своего.
    pub units: &'a [OrgUnitId],
}

impl MemberFilter {
    /// Проверка: не более [`MAX_FILTER_NODES`] узлов и [`MAX_FILTER_DEPTH`] уровней, имена атрибутов — как у контакта,
    /// значения — до [`MAX_NAME_LEN`] символов; иначе `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        fn walk(f: &MemberFilter, depth: usize, nodes: &mut usize) -> Result<()> {
            *nodes += 1;
            if depth > MAX_FILTER_DEPTH || *nodes > MAX_FILTER_NODES {
                return Err(Error::invalid("member filter is too large"));
            }
            match f {
                MemberFilter::Equals { attribute, value }
                | MemberFilter::Prefix { attribute, value } => {
                    if !valid_attribute(attribute) || !plain(value, MAX_NAME_LEN) {
                        return Err(Error::invalid("member filter: invalid attribute or value"));
                    }
                }
                MemberFilter::Present { attribute } => {
                    if !valid_attribute(attribute) {
                        return Err(Error::invalid("member filter: invalid attribute"));
                    }
                }
                MemberFilter::Unit { .. } => {}
                MemberFilter::All(v) | MemberFilter::Any(v) => {
                    for x in v {
                        walk(x, depth + 1, nodes)?;
                    }
                }
                MemberFilter::Not(x) => walk(x, depth + 1, nodes)?,
            }
            Ok(())
        }
        walk(self, 1, &mut 0)
    }

    /// Подходит ли `target` под фильтр.
    pub fn matches(&self, target: FilterTarget<'_>) -> bool {
        let get = |name: &str| {
            target
                .attributes
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        };
        match self {
            MemberFilter::Equals { attribute, value } => {
                get(attribute).is_some_and(|v| v.to_lowercase() == value.to_lowercase())
            }
            MemberFilter::Prefix { attribute, value } => {
                get(attribute).is_some_and(|v| v.to_lowercase().starts_with(&value.to_lowercase()))
            }
            MemberFilter::Present { attribute } => get(attribute).is_some(),
            MemberFilter::Unit { unit, subtree } => {
                if *subtree {
                    target.units.contains(unit)
                } else {
                    target.units.first() == Some(unit)
                }
            }
            MemberFilter::All(v) => v.iter().all(|f| f.matches(target)),
            MemberFilter::Any(v) => v.iter().any(|f| f.matches(target)),
            MemberFilter::Not(f) => !f.matches(target),
        }
    }
}

/// Фильтр динамической группы. Группа рассылки с тем же `group` (адрес, отправители, модерация) — в `GroupStore`;
/// её статические члены при раскрытии не используются.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DynamicGroup {
    /// Группа рассылки.
    pub group: GroupId,
    /// Организация.
    pub tenant: TenantId,
    /// Фильтр состава.
    pub filter: MemberFilter,
    /// Включать почтовые контакты, подходящие под фильтр.
    pub contacts: bool,
}

/// Вычисленный состав группы.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Members {
    /// Учётные записи по возрастанию без повторов.
    pub accounts: Vec<AccountId>,
    /// Контакты по возрастанию без повторов.
    pub contacts: Vec<ContactId>,
}

/// Фильтры динамических групп организации.
#[async_trait]
pub trait DynamicGroupStore: Send + Sync + 'static {
    /// Фильтр группы; `None` — группа не динамическая.
    async fn dynamic(&self, tenant: TenantId, group: GroupId) -> Result<Option<DynamicGroup>>;

    /// Динамические группы организации по `group`.
    async fn dynamic_groups(&self, tenant: TenantId) -> Result<Vec<DynamicGroup>>;

    /// Задаёт фильтр группы (создать или заменить); некорректный фильтр — `Error::Invalid`. Группу рассылки создаёт
    /// вызывающий (`GroupStore::put`); реализация на PostgreSQL может требовать её существования (`Error::NotFound`).
    async fn put_dynamic(&self, group: DynamicGroup) -> Result<()>;

    /// Снимает фильтр; нет — `Error::NotFound`.
    async fn remove_dynamic(&self, tenant: TenantId, group: GroupId) -> Result<()>;

    /// Состав при раскрытии: учётные записи с оргатрибутами ([`OrgStore`]; [`OrgAttributes::filter_attributes`] и
    /// цепочка подразделений) и, если `contacts`, — контакты ([`ContactStore`], атрибуты контакта), подходящие под
    /// фильтр. Группа не динамическая — `Error::NotFound`.
    async fn expand_dynamic(&self, tenant: TenantId, group: GroupId) -> Result<Members>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn object_safe() {
        fn _c(_: std::sync::Arc<dyn ContactStore>) {}
        fn _o(_: std::sync::Arc<dyn OrgStore>) {}
        fn _d(_: std::sync::Arc<dyn DynamicGroupStore>) {}
    }

    fn unit(name: &str, parent: Option<OrgUnitId>) -> OrgUnit {
        OrgUnit {
            id: OrgUnitId::new(),
            tenant: TenantId::DEFAULT,
            name: name.into(),
            parent,
            external_id: None,
        }
    }

    #[test]
    fn tree() {
        let root = unit("Компания", None);
        let it = unit("ИТ", Some(root.id));
        let dev = unit("Разработка", Some(it.id));
        let units = vec![root.clone(), it.clone(), dev.clone()];
        let names: Vec<_> = unit_path(&units, dev.id)
            .iter()
            .map(|u| u.name.as_str())
            .collect();
        assert_eq!(names, ["Компания", "ИТ", "Разработка"]);
        assert!(unit_path(&units, OrgUnitId::new()).is_empty());
        assert!(check_unit(&units, &dev).is_ok());
        // Корень под своим потомком — цикл.
        let looped = OrgUnit {
            parent: Some(dev.id),
            ..root.clone()
        };
        assert!(matches!(
            check_unit(&units, &looped),
            Err(Error::Invalid(_))
        ));
        let orphan = unit("X", Some(OrgUnitId::new()));
        assert!(check_unit(&units, &orphan).is_err());
        assert!(check_unit(&units, &unit(" ", None)).is_err());
    }

    #[test]
    fn filter() {
        let root = OrgUnitId::new();
        let dev = OrgUnitId::new();
        let org = OrgAttributes {
            department: Some(dev),
            title: Some("Инженер".into()),
            attributes: BTreeMap::from([("office".to_owned(), "Москва, 5 этаж".to_owned())]),
            ..OrgAttributes::default()
        };
        let attrs = org.filter_attributes();
        let chain = [dev, root];
        let t = FilterTarget {
            attributes: &attrs,
            units: &chain,
        };
        let f = MemberFilter::All(vec![
            MemberFilter::Equals {
                attribute: "TITLE".into(),
                value: "инженер".into(),
            },
            MemberFilter::Prefix {
                attribute: "office".into(),
                value: "москва".into(),
            },
            MemberFilter::Unit {
                unit: root,
                subtree: true,
            },
            MemberFilter::Not(Box::new(MemberFilter::Present {
                attribute: "company".into(),
            })),
        ]);
        assert!(f.validate().is_ok());
        assert!(f.matches(t));
        assert!(
            !MemberFilter::Unit {
                unit: root,
                subtree: false
            }
            .matches(t)
        );
        assert!(!MemberFilter::Any(vec![]).matches(t));
        assert!(MemberFilter::All(vec![]).matches(t));
        let mut deep = MemberFilter::All(vec![]);
        for _ in 0..MAX_FILTER_DEPTH {
            deep = MemberFilter::Not(Box::new(deep));
        }
        assert!(matches!(deep.validate(), Err(Error::Invalid(_))));
        let bad = MemberFilter::Present {
            attribute: "a b".into(),
        };
        assert!(bad.validate().is_err());
        let s = serde_json::to_string(&f).expect("serialize");
        assert_eq!(serde_json::from_str::<MemberFilter>(&s).expect("parse"), f);
    }

    #[test]
    fn members_order_and_contact() {
        let a = |s: Option<i32>| OrgMember {
            account: AccountId::new(),
            org: OrgAttributes {
                seniority: s,
                ..OrgAttributes::default()
            },
        };
        let mut v = vec![a(None), a(Some(1)), a(Some(10))];
        sort_members(&mut v);
        let order: Vec<_> = v.iter().map(|m| m.org.seniority).collect();
        assert_eq!(order, [Some(10), Some(1), None]);

        let c = MailContact {
            id: ContactId::new(),
            tenant: TenantId::DEFAULT,
            display_name: "Партнёр".into(),
            email: "p@partner.example".into(),
            external_id: None,
            attributes: BTreeMap::new(),
            groups: vec![],
            hidden_from_gal: false,
        };
        assert!(c.validate().is_ok());
        assert!(
            MailContact {
                email: "bad".into(),
                ..c.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            MailContact {
                display_name: String::new(),
                ..c
            }
            .validate()
            .is_err()
        );
    }
}
