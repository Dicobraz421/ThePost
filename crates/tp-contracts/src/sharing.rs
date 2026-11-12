//! Общий доступ к календарям и адресным книгам, представители, ресурсы (CTR-48; CONTRACTS.md#Sharing; SRS 4.4, 4.5).
//!
//! - [`CollectionAcl`] — права на коллекции ([`crate::dav::Collection`]) по субъектам, как у
//!   [`AclStore`](crate::acl::AclStore): имя входа, имя группы, [`ANYONE`]. Владелец имеет все права на свои коллекции
//!   без записей — это правило слоя авторизации, хранилище его не хранит.
//! - [`DelegateStore`] — представители учётной записи (CalDAV `calendar-proxy-read`, `calendar-proxy-write`; Outlook
//!   «делегаты»): доступ ко всем календарям владельца и планирование от его имени.
//! - [`ResourceStore`] — правила бронирования [`BookingPolicy`] ресурсов (переговорных) — учётных записей вида
//!   [`AccountKind::Resource`] ([`Account::kind`](crate::meta::Account::kind)); решение по приглашению —
//!   [`BookingPolicy::decide`].
//! - [`SharingPolicy`] — политика организации на общий доступ (APP-I «Политики общего доступа к календарям»).
//!
//! Права проверяет единый слой авторизации (`tp-auth`), хранилища только хранят записи; реализации — `tp-meta`,
//! in-memory — `tp-testkit`.

use std::collections::BTreeSet;
use std::ops::{BitOr, BitOrAssign};
use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, PrimitiveDateTime, Time, Weekday};

pub use crate::acl::{ANYONE, validate_identifier};
use crate::config::duration;
use crate::dav::CollectionId;
use crate::error::{Error, Result};
use crate::ids::{AccountId, GroupId, TenantId};
#[cfg(doc)]
use crate::meta::AccountKind;
use crate::meta::AccountScope;
use crate::schedule::{BusyPeriod, Period};

// ---------- Права на коллекцию ----------

/// Права на коллекцию. Старшее право включает младшие ([`CollectionRights::effective`]): `admin` — все, `write` —
/// `read`, `read` — `free_busy`; `schedule` — отдельное.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CollectionRights {
    /// Видеть занятость без подробностей (CalDAV `read-free-busy`; JMAP `mayReadFreeBusy`).
    pub free_busy: bool,
    /// Читать объекты (WebDAV `read`; JMAP `mayReadItems`).
    pub read: bool,
    /// Создавать, изменять и удалять объекты (WebDAV `write-content`, `bind`, `unbind`; JMAP `mayWriteAll`).
    pub write: bool,
    /// Планировать от имени владельца: приглашения и ответы (RFC 6638 `schedule-send`; JMAP `mayRSVP`).
    pub schedule: bool,
    /// Менять свойства коллекции, удалять её и выдавать права (WebDAV `write-properties`, `write-acl`; JMAP
    /// `mayAdmin`, `mayDelete`).
    pub admin: bool,
}

impl CollectionRights {
    /// Нет прав.
    pub const NONE: Self = Self {
        free_busy: false,
        read: false,
        write: false,
        schedule: false,
        admin: false,
    };
    /// Только занятость.
    pub const FREE_BUSY: Self = Self {
        free_busy: true,
        ..Self::NONE
    };
    /// Чтение (и занятость).
    pub const READ: Self = Self {
        read: true,
        ..Self::FREE_BUSY
    };
    /// Запись (и чтение).
    pub const WRITE: Self = Self {
        write: true,
        ..Self::READ
    };
    /// Планирование от имени владельца.
    pub const SCHEDULE: Self = Self {
        schedule: true,
        ..Self::NONE
    };
    /// Все права.
    pub const ALL: Self = Self {
        free_busy: true,
        read: true,
        write: true,
        schedule: true,
        admin: true,
    };

    /// Нет ни одного права.
    pub const fn is_empty(self) -> bool {
        !(self.free_busy || self.read || self.write || self.schedule || self.admin)
    }

    /// Права с включёнными младшими: `admin` — все, `write` — `read`, `read` — `free_busy`.
    pub const fn effective(self) -> Self {
        if self.admin {
            return Self::ALL;
        }
        let read = self.read || self.write;
        Self {
            free_busy: self.free_busy || read,
            read,
            ..self
        }
    }

    /// Действующие права включают все действующие права `required`.
    pub const fn contains(self, required: Self) -> bool {
        let (have, need) = (self.effective(), required.effective());
        (have.free_busy || !need.free_busy)
            && (have.read || !need.read)
            && (have.write || !need.write)
            && (have.schedule || !need.schedule)
            && (have.admin || !need.admin)
    }

    /// Общая часть действующих прав (ограничение политикой).
    pub const fn intersect(self, other: Self) -> Self {
        let (a, b) = (self.effective(), other.effective());
        Self {
            free_busy: a.free_busy && b.free_busy,
            read: a.read && b.read,
            write: a.write && b.write,
            schedule: a.schedule && b.schedule,
            admin: a.admin && b.admin,
        }
    }
}

impl BitOr for CollectionRights {
    type Output = Self;

    /// Объединение прав.
    fn bitor(self, other: Self) -> Self {
        Self {
            free_busy: self.free_busy || other.free_busy,
            read: self.read || other.read,
            write: self.write || other.write,
            schedule: self.schedule || other.schedule,
            admin: self.admin || other.admin,
        }
    }
}

impl BitOrAssign for CollectionRights {
    fn bitor_assign(&mut self, other: Self) {
        *self = *self | other;
    }
}

/// Запись прав на коллекцию.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionAclEntry {
    /// Субъект: имя входа, имя группы или [`ANYONE`] ([`validate_identifier`] без отрицательных прав).
    pub identifier: String,
    /// Права (непустые), как выданы — без включённых младших.
    pub rights: CollectionRights,
}

/// Чужая коллекция, доступная субъекту.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SharedCollection {
    /// Владелец.
    pub owner: AccountId,
    /// Коллекция.
    pub collection: CollectionId,
    /// Действующие права субъекта ([`CollectionRights::effective`]).
    pub rights: CollectionRights,
}

/// Права субъекта по записям: объединение прав его идентификаторов (имя входа, группы, [`ANYONE`]) с включёнными
/// младшими.
pub fn collection_rights<'a>(
    entries: impl IntoIterator<Item = &'a CollectionAclEntry>,
    identifiers: &[String],
) -> CollectionRights {
    entries
        .into_iter()
        .filter(|e| identifiers.contains(&e.identifier))
        .fold(CollectionRights::NONE, |acc, e| acc | e.rights)
        .effective()
}

/// Права на коллекции учётных записей. Субъекты — как у [`AclStore`](crate::acl::AclStore), без отрицательных прав.
/// Запись выдаёт владелец или субъект с правом `admin` в пределах [`SharingPolicy`] — проверяет вызывающий
/// ([`SharingPolicy::check`]). Удаление коллекции или учётной записи удаляет записи.
#[async_trait]
pub trait CollectionAcl: Send + Sync + 'static {
    /// Записи прав на коллекцию по идентификатору. Нет коллекции — `Error::NotFound`.
    async fn collection_acl(
        &self,
        owner: AccountScope,
        collection: CollectionId,
    ) -> Result<Vec<CollectionAclEntry>>;

    /// Заменяет права субъекта на коллекцию целиком; пустые права удаляют запись. Недопустимый идентификатор —
    /// `Error::Invalid`; нет коллекции — `Error::NotFound`.
    async fn set_collection_acl(
        &self,
        owner: AccountScope,
        collection: CollectionId,
        identifier: &str,
        rights: CollectionRights,
    ) -> Result<()>;

    /// Действующие права субъекта с идентификаторами `identifiers` ([`collection_rights`]); нет коллекции —
    /// [`CollectionRights::NONE`].
    async fn collection_rights(
        &self,
        owner: AccountScope,
        collection: CollectionId,
        identifiers: &[String],
    ) -> Result<CollectionRights>;

    /// Чужие коллекции организации, на которые у субъекта есть права, — для дома коллекций получателя (CalDAV,
    /// CardDAV) и списка JMAP; по (`owner`, `collection`). Свои коллекции субъекта не входят.
    async fn shared_with(
        &self,
        scope: AccountScope,
        identifiers: &[String],
    ) -> Result<Vec<SharedCollection>>;
}

// ---------- Представители ----------

/// Уровень доступа представителя ко всем календарям владельца.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DelegateLevel {
    /// Чтение (CalDAV `calendar-proxy-read`).
    Read,
    /// Чтение и запись (CalDAV `calendar-proxy-write`).
    Write,
}

/// Представитель учётной записи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Delegate {
    /// Учётная запись представителя (той же организации).
    pub delegate: AccountId,
    /// Уровень доступа к календарям.
    pub level: DelegateLevel,
    /// Представитель отправляет приглашения и ответы от имени владельца (iTIP `SENT-BY`).
    #[serde(default)]
    pub schedule_on_behalf: bool,
}

impl Delegate {
    /// Права представителя на каждый календарь владельца.
    pub const fn rights(&self) -> CollectionRights {
        let base = match self.level {
            DelegateLevel::Read => CollectionRights::READ,
            DelegateLevel::Write => CollectionRights::WRITE,
        };
        CollectionRights {
            schedule: self.schedule_on_behalf,
            ..base
        }
    }
}

/// Учётная запись, представителем которой является субъект.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Delegation {
    /// Владелец календарей.
    pub owner: AccountId,
    /// Уровень доступа.
    pub level: DelegateLevel,
    /// Планирование от имени владельца.
    pub schedule_on_behalf: bool,
}

/// Представители учётных записей. Удаление любой из учётных записей удаляет запись.
#[async_trait]
pub trait DelegateStore: Send + Sync + 'static {
    /// Представители владельца по `delegate`. Нет учётной записи — `Error::NotFound`.
    async fn delegates(&self, owner: AccountScope) -> Result<Vec<Delegate>>;

    /// Назначает или заменяет представителя `delegate.delegate`. Владелец сам себе — `Error::Invalid`; нет любой из
    /// учётных записей — `Error::NotFound`.
    async fn set_delegate(&self, owner: AccountScope, delegate: Delegate) -> Result<()>;

    /// Отзывает представителя; не назначен — без изменений. Нет владельца — `Error::NotFound`.
    async fn remove_delegate(&self, owner: AccountScope, delegate: AccountId) -> Result<()>;

    /// Учётные записи, представителем которых является `delegate` (CalDAV `calendar-proxy-read-for`,
    /// `calendar-proxy-write-for`), по `owner`.
    async fn delegations(&self, delegate: AccountScope) -> Result<Vec<Delegation>>;
}

// ---------- Ресурсы ----------

/// Кому разрешено бронировать ресурс.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum Bookers {
    /// Любая учётная запись организации.
    #[default]
    Internal,
    /// Только перечисленные учётные записи и участники перечисленных групп (раскрывает вызывающий).
    Only {
        /// Учётные записи.
        accounts: Vec<AccountId>,
        /// Группы.
        groups: Vec<GroupId>,
    },
}

/// Рабочее время ресурса в его часовом поясе.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkingHours {
    /// Часовой пояс IANA; местное время бронирования ([`BookingPeriod::local_start`]) вычисляет вызывающий.
    pub timezone: String,
    /// Рабочие дни недели (непусто).
    pub days: BTreeSet<DayOfWeek>,
    /// Начало рабочего дня.
    pub from: Time,
    /// Конец рабочего дня, позже начала.
    pub to: Time,
}

/// День недели (ISO 8601: понедельник — первый).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DayOfWeek {
    /// Понедельник.
    Monday,
    /// Вторник.
    Tuesday,
    /// Среда.
    Wednesday,
    /// Четверг.
    Thursday,
    /// Пятница.
    Friday,
    /// Суббота.
    Saturday,
    /// Воскресенье.
    Sunday,
}

impl From<Weekday> for DayOfWeek {
    fn from(day: Weekday) -> Self {
        match day {
            Weekday::Monday => Self::Monday,
            Weekday::Tuesday => Self::Tuesday,
            Weekday::Wednesday => Self::Wednesday,
            Weekday::Thursday => Self::Thursday,
            Weekday::Friday => Self::Friday,
            Weekday::Saturday => Self::Saturday,
            Weekday::Sunday => Self::Sunday,
        }
    }
}

impl WorkingHours {
    /// Встреча с местным началом `local_start` и длительностью `length` целиком в рабочем времени одного рабочего дня.
    pub fn covers(&self, local_start: PrimitiveDateTime, length: time::Duration) -> bool {
        let local_end = local_start + length;
        self.days.contains(&local_start.weekday().into())
            && local_start.date() == local_end.date()
            && local_start.time() >= self.from
            && local_end.time() <= self.to
    }
}

/// Правила бронирования ресурса.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BookingPolicy {
    /// Подходящее приглашение принимается автоматически; иначе остаётся предварительным до решения ответственного
    /// (представителя ресурса).
    pub auto_accept: bool,
    /// Разрешены пересекающиеся бронирования.
    pub allow_conflicts: bool,
    /// Наибольшая длительность одной встречи; `None` — без ограничения.
    #[serde(with = "duration::option")]
    pub max_duration: Option<Duration>,
    /// Насколько вперёд от текущего момента можно бронировать (конец встречи и последнего повтора); `None` — без
    /// ограничения.
    #[serde(with = "duration::option")]
    pub horizon: Option<Duration>,
    /// Кому разрешено бронировать.
    pub bookers: Bookers,
    /// Рабочее время; `None` — круглосуточно.
    pub working_hours: Option<WorkingHours>,
}

impl Default for BookingPolicy {
    /// Автоматическое принятие без пересечений для всех учётных записей организации: до 24 часов, на 180 дней вперёд,
    /// круглосуточно.
    fn default() -> Self {
        Self {
            auto_accept: true,
            allow_conflicts: false,
            max_duration: Some(Duration::from_secs(24 * 3600)),
            horizon: Some(Duration::from_secs(180 * 24 * 3600)),
            bookers: Bookers::Internal,
            working_hours: None,
        }
    }
}

/// Одно вхождение запрошенной встречи.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookingPeriod {
    /// Интервал `[start, end)` (UTC).
    pub period: Period,
    /// Начало в часовом поясе [`WorkingHours::timezone`]; `None` — не вычислено (нужно только при рабочем времени).
    pub local_start: Option<PrimitiveDateTime>,
}

/// Запрос бронирования — приглашение ресурсу (iTIP `REQUEST`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BookingRequest {
    /// Учётная запись организатора; `None` — внешний организатор.
    pub organizer: Option<AccountId>,
    /// Группы организатора с вложенными (раскрывает вызывающий).
    pub organizer_groups: BTreeSet<GroupId>,
    /// Вхождения встречи с развёрнутыми повторами — до горизонта индекса занятости
    /// ([`DavLimits::busy_horizon`](crate::dav::DavLimits)); бесконечные повторы вызывающий обрывает на нём и ставит
    /// [`BookingRequest::unbounded`].
    pub periods: Vec<BookingPeriod>,
    /// Повторы без конца: при заданном [`BookingPolicy::horizon`] — отказ.
    pub unbounded: bool,
}

/// Причина отказа в бронировании.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclineReason {
    /// Организатору не разрешено бронировать ресурс.
    NotAllowed,
    /// Запрос без вхождений или с вхождением нулевой либо отрицательной длительности.
    Invalid,
    /// Встреча длиннее [`BookingPolicy::max_duration`].
    TooLong,
    /// Встреча (или её повторы) дальше [`BookingPolicy::horizon`].
    TooFar,
    /// Вне рабочего времени ресурса.
    OutsideWorkingHours,
    /// Ресурс занят.
    Conflict,
}

/// Решение ресурса по приглашению (iTIP `PARTSTAT`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BookingDecision {
    /// Принять (`ACCEPTED`).
    Accept,
    /// Отклонить (`DECLINED`) с причиной для ответа организатору.
    Decline(DeclineReason),
    /// Оставить предварительным (`TENTATIVE`) до решения ответственного.
    Tentative,
}

impl BookingPolicy {
    /// Проверяет правила: длительности больше нуля; рабочее время — непустые дни, начало раньше конца, часовой пояс
    /// 1–255 байт без управляющих символов. Иначе — `Error::Invalid`.
    pub fn validate(&self) -> Result<()> {
        if self.max_duration.is_some_and(|d| d.is_zero())
            || self.horizon.is_some_and(|d| d.is_zero())
        {
            return Err(Error::invalid(
                "длительность и горизонт бронирования — больше нуля",
            ));
        }
        if let Some(hours) = &self.working_hours {
            let zone = &hours.timezone;
            if hours.days.is_empty()
                || hours.from >= hours.to
                || zone.is_empty()
                || zone.len() > 255
                || zone.chars().any(char::is_control)
            {
                return Err(Error::invalid("недопустимое рабочее время ресурса"));
            }
        }
        Ok(())
    }

    /// Организатору разрешено бронировать: внешнему — никогда.
    pub fn allows(&self, organizer: Option<AccountId>, groups: &BTreeSet<GroupId>) -> bool {
        let Some(organizer) = organizer else {
            return false;
        };
        match &self.bookers {
            Bookers::Internal => true,
            Bookers::Only {
                accounts,
                groups: allowed,
            } => accounts.contains(&organizer) || allowed.iter().any(|g| groups.contains(g)),
        }
    }

    /// Решение по приглашению. `busy` — занятость ресурса в интервале запроса без самой этой встречи (при изменении
    /// уже принятой), `now` — текущий момент. Проверки по порядку, первая несработавшая — отказ: организатор
    /// ([`DeclineReason::NotAllowed`]), вхождения ([`DeclineReason::Invalid`]), длительность
    /// ([`DeclineReason::TooLong`]), горизонт ([`DeclineReason::TooFar`]), рабочее время
    /// ([`DeclineReason::OutsideWorkingHours`]; вхождение без `local_start` считается вне его), пересечения, если не
    /// разрешены ([`DeclineReason::Conflict`]; занятость любого вида). Прошло всё — [`BookingDecision::Accept`] при
    /// `auto_accept`, иначе [`BookingDecision::Tentative`].
    pub fn decide(
        &self,
        request: &BookingRequest,
        busy: &[BusyPeriod],
        now: OffsetDateTime,
    ) -> BookingDecision {
        use BookingDecision::Decline;
        if !self.allows(request.organizer, &request.organizer_groups) {
            return Decline(DeclineReason::NotAllowed);
        }
        let periods = || request.periods.iter().map(|p| p.period);
        if request.periods.is_empty() || periods().any(|p| p.end <= p.start) {
            return Decline(DeclineReason::Invalid);
        }
        if let Some(max) = self.max_duration
            && periods().any(|p| p.end - p.start > max)
        {
            return Decline(DeclineReason::TooLong);
        }
        if let Some(horizon) = self.horizon
            && (request.unbounded || periods().any(|p| p.end > now + horizon))
        {
            return Decline(DeclineReason::TooFar);
        }
        if let Some(hours) = &self.working_hours {
            let inside = request.periods.iter().all(|p| {
                p.local_start
                    .is_some_and(|local| hours.covers(local, p.period.end - p.period.start))
            });
            if !inside {
                return Decline(DeclineReason::OutsideWorkingHours);
            }
        }
        if !self.allow_conflicts && periods().any(|p| busy.iter().any(|b| b.overlaps(&p))) {
            return Decline(DeclineReason::Conflict);
        }
        if self.auto_accept {
            BookingDecision::Accept
        } else {
            BookingDecision::Tentative
        }
    }
}

/// Правила бронирования ресурсов организации. Ресурс — учётная запись вида [`AccountKind::Resource`]: вид —
/// свойство самой учётной записи ([`Account::kind`](crate::meta::Account::kind), задаётся
/// [`AccountUpsert::kind`](crate::meta::AccountUpsert::kind)), здесь не хранится и не меняется.
#[async_trait]
pub trait ResourceStore: Send + Sync + 'static {
    /// Правила бронирования ресурса; `None` — не заданы (приглашения ресурсу не обрабатываются автоматически) или
    /// учётная запись — не ресурс. Нет учётной записи — `Error::NotFound`.
    async fn booking_policy(&self, scope: AccountScope) -> Result<Option<BookingPolicy>>;

    /// Задаёт правила бронирования ресурса или снимает их (`None`; принятые бронирования остаются).
    /// [`BookingPolicy::validate`] и учётная запись не вида [`AccountKind::Resource`] — `Error::Invalid`; нет
    /// учётной записи — `Error::NotFound`. Смена вида учётной записи на пользователя снимает правила.
    async fn set_booking_policy(
        &self,
        scope: AccountScope,
        policy: Option<BookingPolicy>,
    ) -> Result<()>;

    /// Учётные записи вида [`AccountKind::Resource`] организации по идентификатору — список выбора переговорной,
    /// GAL.
    async fn resources(&self, tenant: TenantId) -> Result<Vec<AccountId>>;
}

// ---------- Политика организации ----------

/// Кому в организации можно выдавать права на свои календари и адресные книги.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum SharingScope {
    /// Общий доступ запрещён: записи прав не выдаются (представители — тоже).
    Nobody,
    /// Полные права — участникам перечисленных групп (с вложенными); остальным субъектам организации — не больше
    /// [`SharingPolicy::max_outside`].
    Groups {
        /// Группы.
        groups: Vec<GroupId>,
    },
    /// Любому субъекту организации — полные права.
    #[default]
    Organization,
}

/// Политика организации на общий доступ к календарям и адресным книгам (SRS 4.4, 4.5; APP-I). Применяется при выдаче
/// прав ([`SharingPolicy::check`]) и при вычислении действующих ([`SharingPolicy::limit`]) — ужесточение политики
/// действует на уже выданные записи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SharingPolicy {
    /// Кому можно выдавать права.
    pub scope: SharingScope,
    /// Наибольшие права субъекту вне групп [`SharingScope::Groups`] и субъекту [`ANYONE`].
    pub max_outside: CollectionRights,
}

impl Default for SharingPolicy {
    /// Любому субъекту организации; «всем» ([`ANYONE`]) — не больше чтения.
    fn default() -> Self {
        Self {
            scope: SharingScope::Organization,
            max_outside: CollectionRights::READ,
        }
    }
}

impl SharingPolicy {
    /// Наибольшие права, которые политика допускает субъекту. `anyone` — субъект [`ANYONE`]; `in_group` — субъект
    /// входит в группу [`SharingScope::Groups`] (или сам — такая группа; раскрывает вызывающий).
    pub fn ceiling(&self, anyone: bool, in_group: bool) -> CollectionRights {
        match &self.scope {
            SharingScope::Nobody => CollectionRights::NONE,
            SharingScope::Organization if !anyone => CollectionRights::ALL,
            SharingScope::Groups { .. } if in_group && !anyone => CollectionRights::ALL,
            _ => self.max_outside.effective(),
        }
    }

    /// Права `rights`, ограниченные политикой, — действующие права субъекта.
    pub fn limit(
        &self,
        rights: CollectionRights,
        anyone: bool,
        in_group: bool,
    ) -> CollectionRights {
        rights.intersect(self.ceiling(anyone, in_group))
    }

    /// Выдача прав `rights` субъекту допустима; иначе — `Error::Forbidden`. Пустые права (отзыв) допустимы всегда.
    pub fn check(&self, rights: CollectionRights, anyone: bool, in_group: bool) -> Result<()> {
        if rights.is_empty() || self.ceiling(anyone, in_group).contains(rights) {
            Ok(())
        } else {
            Err(Error::Forbidden)
        }
    }
}

/// Политика общего доступа организации. Реализация — `tp-meta`; меняет Admin API.
#[async_trait]
pub trait SharingPolicyStore: Send + Sync + 'static {
    /// Политика организации; не задана — [`SharingPolicy::default`].
    async fn sharing_policy(&self, tenant: TenantId) -> Result<SharingPolicy>;

    /// Задаёт политику. Уже выданные записи не удаляются: их действие ограничивает [`SharingPolicy::limit`].
    async fn set_sharing_policy(&self, tenant: TenantId, policy: SharingPolicy) -> Result<()>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::BusyKind;
    use std::sync::Arc;
    use time::Date;
    use time::Month::November;

    #[test]
    fn object_safe() {
        fn _acl(_: Arc<dyn CollectionAcl>) {}
        fn _delegates(_: Arc<dyn DelegateStore>) {}
        fn _resources(_: Arc<dyn ResourceStore>) {}
        fn _policy(_: Arc<dyn SharingPolicyStore>) {}
    }

    #[test]
    fn rights() {
        use CollectionRights as R;
        assert!(R::NONE.is_empty() && !R::FREE_BUSY.is_empty());
        // Старшее право включает младшие; планирование — отдельно.
        let write = R {
            write: true,
            ..R::NONE
        };
        assert_eq!(write.effective(), R::WRITE);
        let admin = R {
            admin: true,
            ..R::NONE
        };
        assert_eq!(admin.effective(), R::ALL);
        assert!(R::WRITE.contains(R::READ) && R::READ.contains(R::FREE_BUSY));
        assert!(!R::WRITE.contains(R::SCHEDULE) && !R::READ.contains(R::WRITE));
        assert!(admin.contains(R::SCHEDULE | R::WRITE));
        assert_eq!(R::ALL.intersect(R::READ), R::READ);
        assert_eq!((R::WRITE | R::SCHEDULE).intersect(R::SCHEDULE), R::SCHEDULE);
        // Отсутствующие поля — нет права.
        assert_eq!(
            serde_json::from_str::<R>(r#"{"read":true}"#).ok(),
            Some(R {
                read: true,
                ..R::NONE
            })
        );

        // Права субъекта — объединение записей его идентификаторов.
        let entry = |id: &str, rights| CollectionAclEntry {
            identifier: id.into(),
            rights,
        };
        let acl = [
            entry("ann@example.com", R::FREE_BUSY),
            entry("sales", write),
            entry(ANYONE, R::SCHEDULE),
            entry("bob@example.com", admin),
        ];
        let ids = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            collection_rights(&acl, &ids(&["ann@example.com", "sales", ANYONE])),
            R::WRITE | R::SCHEDULE
        );
        assert_eq!(
            collection_rights(&acl, &ids(&["ann@example.com"])),
            R::FREE_BUSY
        );
        assert_eq!(collection_rights(&acl, &ids(&["eve@example.com"])), R::NONE);
    }

    #[test]
    fn delegate_rights() {
        let delegate = |level, schedule_on_behalf| Delegate {
            delegate: AccountId::new(),
            level,
            schedule_on_behalf,
        };
        assert_eq!(
            delegate(DelegateLevel::Read, false).rights(),
            CollectionRights::READ
        );
        assert_eq!(
            delegate(DelegateLevel::Write, true).rights(),
            CollectionRights::WRITE | CollectionRights::SCHEDULE
        );
        assert!(!delegate(DelegateLevel::Write, true).rights().admin);
    }

    /// 2026-11-16 — понедельник; время — UTC, местное совпадает.
    fn at(day: u8, hour: u8) -> PrimitiveDateTime {
        let date = Date::from_calendar_date(2026, November, day).unwrap_or(Date::MIN);
        PrimitiveDateTime::new(date, Time::from_hms(hour, 0, 0).unwrap_or(Time::MIDNIGHT))
    }

    fn slot(day: u8, from: u8, to: u8) -> BookingPeriod {
        BookingPeriod {
            period: Period {
                start: at(day, from).assume_utc(),
                end: at(day, to).assume_utc(),
            },
            local_start: Some(at(day, from)),
        }
    }

    fn request(organizer: Option<AccountId>, periods: Vec<BookingPeriod>) -> BookingRequest {
        BookingRequest {
            organizer,
            organizer_groups: BTreeSet::new(),
            periods,
            unbounded: false,
        }
    }

    #[test]
    fn booking() {
        use BookingDecision::{Accept, Decline, Tentative};
        let now = at(10, 12).assume_utc();
        let ann = Some(AccountId::new());
        let policy = BookingPolicy::default();
        assert_eq!(policy.validate(), Ok(()));
        let meeting = request(ann, vec![slot(16, 10, 11)]);
        assert_eq!(policy.decide(&meeting, &[], now), Accept);

        // Внешний организатор и организатор вне списка.
        let external = request(None, vec![slot(16, 10, 11)]);
        assert_eq!(
            policy.decide(&external, &[], now),
            Decline(DeclineReason::NotAllowed)
        );
        let group = GroupId::new();
        let listed = BookingPolicy {
            bookers: Bookers::Only {
                accounts: Vec::new(),
                groups: vec![group],
            },
            ..policy.clone()
        };
        assert_eq!(
            listed.decide(&meeting, &[], now),
            Decline(DeclineReason::NotAllowed)
        );
        let member = BookingRequest {
            organizer_groups: BTreeSet::from([group]),
            ..meeting.clone()
        };
        assert_eq!(listed.decide(&member, &[], now), Accept);

        // Вхождения, длительность, горизонт.
        let empty = request(ann, Vec::new());
        assert_eq!(
            policy.decide(&empty, &[], now),
            Decline(DeclineReason::Invalid)
        );
        let reversed = request(ann, vec![slot(16, 11, 10)]);
        assert_eq!(
            policy.decide(&reversed, &[], now),
            Decline(DeclineReason::Invalid)
        );
        let short = BookingPolicy {
            max_duration: Some(Duration::from_secs(1800)),
            ..policy.clone()
        };
        assert_eq!(
            short.decide(&meeting, &[], now),
            Decline(DeclineReason::TooLong)
        );
        let near = BookingPolicy {
            horizon: Some(Duration::from_secs(24 * 3600)),
            ..policy.clone()
        };
        assert_eq!(
            near.decide(&meeting, &[], now),
            Decline(DeclineReason::TooFar)
        );
        let forever = BookingRequest {
            unbounded: true,
            ..meeting.clone()
        };
        assert_eq!(
            policy.decide(&forever, &[], now),
            Decline(DeclineReason::TooFar)
        );
        let no_horizon = BookingPolicy {
            horizon: None,
            ..policy.clone()
        };
        assert_eq!(no_horizon.decide(&forever, &[], now), Accept);

        // Рабочее время: будни 9–18.
        let office = BookingPolicy {
            working_hours: Some(WorkingHours {
                timezone: "Europe/Moscow".into(),
                days: BTreeSet::from([DayOfWeek::Monday, DayOfWeek::Tuesday]),
                from: at(16, 9).time(),
                to: at(16, 18).time(),
            }),
            ..policy.clone()
        };
        assert_eq!(office.validate(), Ok(()));
        assert_eq!(office.decide(&meeting, &[], now), Accept);
        for outside in [slot(16, 8, 10), slot(16, 17, 19), slot(18, 10, 11)] {
            let late = request(ann, vec![slot(16, 10, 11), outside]);
            assert_eq!(
                office.decide(&late, &[], now),
                Decline(DeclineReason::OutsideWorkingHours)
            );
        }
        let no_local = request(
            ann,
            vec![BookingPeriod {
                local_start: None,
                ..slot(16, 10, 11)
            }],
        );
        assert_eq!(
            office.decide(&no_local, &[], now),
            Decline(DeclineReason::OutsideWorkingHours)
        );
        assert_eq!(policy.decide(&no_local, &[], now), Accept);

        // Пересечения: смежная встреча не мешает.
        let busy = |from, to| BusyPeriod {
            start: at(16, from).assume_utc(),
            end: at(16, to).assume_utc(),
            kind: BusyKind::Tentative,
        };
        assert_eq!(
            policy.decide(&meeting, &[busy(9, 10), busy(11, 12)], now),
            Accept
        );
        assert_eq!(
            policy.decide(&meeting, &[busy(10, 12)], now),
            Decline(DeclineReason::Conflict)
        );
        let shared = BookingPolicy {
            allow_conflicts: true,
            ..policy.clone()
        };
        assert_eq!(shared.decide(&meeting, &[busy(10, 12)], now), Accept);

        // Без автоматического принятия — предварительно, отказы — те же.
        let manual = BookingPolicy {
            auto_accept: false,
            ..policy.clone()
        };
        assert_eq!(manual.decide(&meeting, &[], now), Tentative);
        assert_eq!(
            manual.decide(&meeting, &[busy(10, 12)], now),
            Decline(DeclineReason::Conflict)
        );
    }

    #[test]
    fn booking_policy_validate_and_serde() {
        let policy = BookingPolicy::default();
        let text = serde_json::to_string(&policy).unwrap_or_default();
        assert!(text.contains(r#""max_duration":"1d""#), "{text}");
        assert_eq!(
            serde_json::from_str::<BookingPolicy>(&text).ok(),
            Some(policy.clone())
        );
        assert_eq!(
            serde_json::from_str::<BookingPolicy>("{}").ok(),
            Some(policy.clone())
        );

        let hours = WorkingHours {
            timezone: "UTC".into(),
            days: BTreeSet::from([DayOfWeek::Friday]),
            from: at(16, 9).time(),
            to: at(16, 18).time(),
        };
        let bad = [
            BookingPolicy {
                max_duration: Some(Duration::ZERO),
                ..policy.clone()
            },
            BookingPolicy {
                horizon: Some(Duration::ZERO),
                ..policy.clone()
            },
            BookingPolicy {
                working_hours: Some(WorkingHours {
                    days: BTreeSet::new(),
                    ..hours.clone()
                }),
                ..policy.clone()
            },
            BookingPolicy {
                working_hours: Some(WorkingHours {
                    from: hours.to,
                    ..hours.clone()
                }),
                ..policy.clone()
            },
            BookingPolicy {
                working_hours: Some(WorkingHours {
                    timezone: String::new(),
                    ..hours
                }),
                ..policy
            },
        ];
        for policy in bad {
            assert!(policy.validate().is_err(), "{policy:?}");
        }
    }

    #[test]
    fn sharing_policy() {
        use CollectionRights as R;
        // По умолчанию: субъекту организации — всё, «всем» — не больше чтения.
        let policy = SharingPolicy::default();
        assert_eq!(
            serde_json::from_str::<SharingPolicy>("{}").ok(),
            Some(policy.clone())
        );
        assert_eq!(policy.check(R::ALL, false, false), Ok(()));
        assert_eq!(policy.check(R::READ, true, false), Ok(()));
        assert_eq!(policy.check(R::WRITE, true, false), Err(Error::Forbidden));
        assert_eq!(policy.limit(R::ALL, true, true), R::READ);

        // Полные права — только группе, остальным — занятость.
        let groups = SharingPolicy {
            scope: SharingScope::Groups {
                groups: vec![GroupId::new()],
            },
            max_outside: R::FREE_BUSY,
        };
        assert_eq!(groups.check(R::WRITE | R::SCHEDULE, false, true), Ok(()));
        assert_eq!(groups.check(R::READ, false, false), Err(Error::Forbidden));
        assert_eq!(groups.check(R::FREE_BUSY, false, false), Ok(()));
        assert_eq!(groups.limit(R::WRITE, false, false), R::FREE_BUSY);
        assert_eq!(groups.limit(R::WRITE, false, true), R::WRITE);

        // Запрет: только отзыв.
        let nobody = SharingPolicy {
            scope: SharingScope::Nobody,
            ..SharingPolicy::default()
        };
        assert_eq!(
            nobody.check(R::FREE_BUSY, false, true),
            Err(Error::Forbidden)
        );
        assert_eq!(nobody.check(R::NONE, false, false), Ok(()));
        assert_eq!(nobody.limit(R::ALL, false, true), R::NONE);
    }
}
