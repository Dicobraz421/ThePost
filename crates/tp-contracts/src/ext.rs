//! Точки расширения ядра и модули (CONTRACTS.md#Extension points; SRS 2.5, 1.3).
//!
//! Модуль — набор крейтов под cargo feature, активируется `modules.<имя>.enabled` в config.yaml.
//! Ядро не меняется: модуль при запуске регистрирует в [`Registry`] свои роли, capability JMAP,
//! маршруты Admin API, подписчиков журнала событий ([`EventSink`]), точки принятия решений
//! ([`DecisionHook`]) и реализации интерфейсов ядра ([`BlobStore`], [`Directory`], [`SearchIndex`]).
//! Данные модуля — только в схеме PostgreSQL `mod_<имя>` с собственной последовательностью миграций.
//!
//! # Точки принятия решений
//!
//! Синхронные проверки перед операцией: модуль может запретить её или дополнить (заголовки, копии —
//! журналирование). Хуки вызываются в порядке регистрации через [`DecisionHooks`]: первый `Deny`
//! прерывает проверку, дополнения объединяются. Без модулей проверка ничего не делает:
//!
//! ```
//! use tp_contracts::ext::{Decision, DecisionHooks, DeliveryInfo};
//! use tp_contracts::TenantId;
//!
//! let hooks = DecisionHooks::default();
//! let info = DeliveryInfo {
//!     tenant: TenantId::DEFAULT,
//!     mail_from: "sender@example.org".into(),
//!     deliveries: vec![],
//!     size: 1024,
//! };
//! let decision = futures::executor::block_on(hooks.on_delivery(&info))?;
//! assert_eq!(decision, Decision::Allow);
//! # Ok::<(), tp_contracts::Error>(())
//! ```
//!
//! Ошибка хука — ошибка операции (проверка не пропускается): `Unavailable` повторяется вызывающим.

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::auth::{Directory, Principal};
use crate::blob::BlobStore;
use crate::error::{Error, Result};
use crate::events::EventSink;
use crate::ids::{MailboxId, MessageId, TenantId};
use crate::mailstore::Delivery;
use crate::meta::AccountScope;
use crate::role::{ConfigWatch, RoleService};
use crate::search::SearchIndex;

// ---------- Модуль ----------

/// Миграция схемы модуля. Применяются по возрастанию `version` в схеме [`Module::schema`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Migration {
    /// Номер: строго возрастает, начиная с 1; применённые миграции не меняются.
    pub version: u32,
    /// Краткое имя (журнал миграций).
    pub name: &'static str,
    /// SQL; объекты создаются без указания схемы (`search_path` = схема модуля).
    pub sql: &'static str,
}

/// Контекст регистрации модуля.
#[non_exhaustive]
#[derive(Clone)]
pub struct ModuleCtx {
    /// Секция `modules.<имя>` config.yaml с hot reload. Значение — секция в виде, который приложение
    /// разбирает из YAML (JSON-значение), см. CONTRACTS.md#Extension points.
    pub config: Arc<dyn ConfigWatch>,
}

impl ModuleCtx {
    /// Контекст с секцией конфигурации модуля.
    pub fn new(config: Arc<dyn ConfigWatch>) -> Self {
        ModuleCtx { config }
    }
}

impl fmt::Debug for ModuleCtx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModuleCtx").finish_non_exhaustive()
    }
}

/// Проверяет имя модуля: `[a-z][a-z0-9_]*`, не длиннее 59 символов (имя схемы `mod_<имя>` ≤ 63).
pub fn validate_module_name(name: &str) -> Result<()> {
    let mut chars = name.bytes();
    let ok = name.len() <= 59
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_');
    if ok {
        Ok(())
    } else {
        Err(Error::Invalid(format!("bad module name: {name}")))
    }
}

/// Дополнительный модуль (SRS 1.3).
pub trait Module: Send + Sync + 'static {
    /// Имя: ключ `modules.<имя>` и суффикс схемы БД; см. [`validate_module_name`].
    fn name(&self) -> &str;

    /// Схема PostgreSQL модуля. Не переопределяется: данные модуля — только в `mod_<имя>`.
    fn schema(&self) -> String {
        format!("mod_{}", self.name())
    }

    /// Миграции схемы модуля по возрастанию `version`.
    fn migrations(&self) -> &[Migration];

    /// Регистрирует расширения. Вызывается один раз при запуске, после миграций; тяжёлые ресурсы —
    /// лениво в ролях (SRS 2.7).
    fn register(&self, registry: &mut Registry, ctx: &ModuleCtx) -> Result<()>;
}

// ---------- JMAP, Admin API ----------

/// Capability JMAP модуля (RFC 8620): объект в сессии и методы. JSON передаётся байтами;
/// ссылки на результаты (`#`-аргументы) разрешает `tp-jmap` до вызова.
#[async_trait]
pub trait JmapCapability: Send + Sync + 'static {
    /// URI capability (`urn:...`), уникален.
    fn uri(&self) -> &str;

    /// JSON-объект capability для `Session.capabilities`.
    fn session_object(&self) -> Bytes;

    /// Имена методов (`Foo/get`, `Foo/set`).
    fn methods(&self) -> &[&str];

    /// Выполняет метод: аргументы и результат — JSON-объекты.
    async fn call(&self, principal: &Principal, method: &str, args: Bytes) -> Result<Bytes>;
}

/// Запрос к маршрутам Admin API модуля.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminRequest {
    /// Метод HTTP (`GET`, `POST`, ...).
    pub method: String,
    /// Путь после префикса маршрутов, начинается с `/`.
    pub path: String,
    /// Строка запроса без `?`.
    pub query: Option<String>,
    /// Тело.
    pub body: Bytes,
}

/// Ответ маршрута Admin API модуля.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminResponse {
    /// Код HTTP.
    pub status: u16,
    /// `Content-Type`.
    pub content_type: String,
    /// Тело.
    pub body: Bytes,
}

/// Маршруты Admin API модуля. Аутентификацию и проверку прав администратора выполняет `tp-admin`
/// до вызова; ошибки отображаются в коды HTTP по `Error`.
#[async_trait]
pub trait AdminRoutes: Send + Sync + 'static {
    /// Префикс пути (`/api/v1/modules/<имя>`), уникален.
    fn prefix(&self) -> &str;

    /// Обрабатывает запрос.
    async fn handle(&self, principal: &Principal, req: AdminRequest) -> Result<AdminResponse>;
}

// ---------- Точки принятия решений ----------

/// Куда направить дополнительную копию письма.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CopyTo {
    /// В папку учётной записи организации (журнальный ящик) — в той же загрузке, что основные доставки.
    Mailbox(Delivery),
    /// Внешнему получателю (через очередь smtp-out).
    Address(String),
}

/// Дополнение операции.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Augment {
    /// Заголовки, добавляемые в начало письма.
    pub headers: Vec<(String, String)>,
    /// Дополнительные копии.
    pub copies: Vec<CopyTo>,
}

impl Augment {
    /// Дополнений нет.
    pub fn is_empty(&self) -> bool {
        self.headers.is_empty() && self.copies.is_empty()
    }

    /// Добавляет дополнения `other` после своих.
    pub fn merge(&mut self, other: Augment) {
        self.headers.extend(other.headers);
        self.copies.extend(other.copies);
    }
}

/// Решение точки принятия решений.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Decision {
    /// Разрешить без изменений.
    #[default]
    Allow,
    /// Запретить; причина — для журнала и ответа (SMTP 550, JMAP `forbidden`).
    Deny(String),
    /// Разрешить с дополнениями.
    Augment(Augment),
}

/// Окончательное удаление письма (maintenance по сроку корзины, expunge).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinalDelete {
    /// Учётная запись.
    pub scope: AccountScope,
    /// Папка письма.
    pub mailbox: MailboxId,
    /// Письмо.
    pub message: MessageId,
    /// Момент приёма (политики хранения).
    pub received_at: OffsetDateTime,
}

/// Доставка письма получателям организации (`MailStore::ingest`, до загрузки).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryInfo {
    /// Организация.
    pub tenant: TenantId,
    /// Отправитель конверта (`MAIL FROM`); пустой — уведомление о доставке.
    pub mail_from: String,
    /// Доставки.
    pub deliveries: Vec<Delivery>,
    /// Размер письма, байт.
    pub size: u64,
}

/// Отправка письма пользователем (submission, JMAP EmailSubmission), до постановки в очередь.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmissionInfo {
    /// Отправитель.
    pub principal: Principal,
    /// Отправитель конверта.
    pub mail_from: String,
    /// Получатели конверта.
    pub rcpt_to: Vec<String>,
    /// Размер письма, байт.
    pub size: u64,
}

/// Точка принятия решений модуля. Методы по умолчанию разрешают операцию.
#[async_trait]
pub trait DecisionHook: Send + Sync + 'static {
    /// Имя (журнал, диагностика).
    fn name(&self) -> &str;

    /// Перед окончательным удалением (Legal Hold, политики хранения). `Augment` трактуется как `Allow`.
    async fn before_final_delete(&self, _op: &FinalDelete) -> Result<Decision> {
        Ok(Decision::Allow)
    }

    /// При доставке (журналирование входящих).
    async fn on_delivery(&self, _op: &DeliveryInfo) -> Result<Decision> {
        Ok(Decision::Allow)
    }

    /// При отправке (журналирование исходящих).
    async fn on_submission(&self, _op: &SubmissionInfo) -> Result<Decision> {
        Ok(Decision::Allow)
    }
}

/// Набор зарегистрированных хуков: вызывает их по порядку и объединяет решения.
#[derive(Clone, Default)]
pub struct DecisionHooks(Arc<[Arc<dyn DecisionHook>]>);

impl DecisionHooks {
    /// Из хуков в порядке регистрации.
    pub fn new(hooks: Vec<Arc<dyn DecisionHook>>) -> Self {
        DecisionHooks(hooks.into())
    }

    /// Хуков нет.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Проверка перед окончательным удалением: `Allow` или `Deny`.
    pub async fn before_final_delete<'a>(&'a self, op: &'a FinalDelete) -> Result<Decision> {
        Ok(match self.combine(|h| h.before_final_delete(op)).await? {
            Decision::Augment(_) => Decision::Allow,
            d => d,
        })
    }

    /// Проверка при доставке.
    pub async fn on_delivery<'a>(&'a self, op: &'a DeliveryInfo) -> Result<Decision> {
        self.combine(|h| h.on_delivery(op)).await
    }

    /// Проверка при отправке.
    pub async fn on_submission<'a>(&'a self, op: &'a SubmissionInfo) -> Result<Decision> {
        self.combine(|h| h.on_submission(op)).await
    }

    async fn combine<'a>(
        &'a self,
        call: impl Fn(&'a dyn DecisionHook) -> BoxFuture<'a, Result<Decision>>,
    ) -> Result<Decision> {
        let mut acc = Augment::default();
        for hook in self.0.iter() {
            match call(hook.as_ref()).await? {
                Decision::Allow => {}
                Decision::Deny(reason) => return Ok(Decision::Deny(reason)),
                Decision::Augment(a) => acc.merge(a),
            }
        }
        Ok(if acc.is_empty() {
            Decision::Allow
        } else {
            Decision::Augment(acc)
        })
    }
}

impl fmt::Debug for DecisionHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|h| h.name()))
            .finish()
    }
}

// ---------- Реестр ----------

/// Реестр расширений: заполняется модулями при запуске, читается приложением.
/// Повторная регистрация имени, URI, префикса или реализации интерфейса — `Error::Conflict`.
#[derive(Default)]
pub struct Registry {
    roles: Vec<Arc<dyn RoleService>>,
    jmap: Vec<Arc<dyn JmapCapability>>,
    admin: Vec<Arc<dyn AdminRoutes>>,
    sinks: Vec<Arc<dyn EventSink>>,
    hooks: Vec<Arc<dyn DecisionHook>>,
    blob_store: Option<Arc<dyn BlobStore>>,
    directory: Option<Arc<dyn Directory>>,
    search_index: Option<Arc<dyn SearchIndex>>,
}

fn unique<T: ?Sized>(items: &[Arc<T>], key: impl Fn(&T) -> &str, new: &T) -> Result<()> {
    if items.iter().any(|x| key(x) == key(new)) {
        Err(Error::Conflict)
    } else {
        Ok(())
    }
}

fn set_once<T: ?Sized>(slot: &mut Option<Arc<T>>, value: Arc<T>) -> Result<()> {
    if slot.is_some() {
        return Err(Error::Conflict);
    }
    *slot = Some(value);
    Ok(())
}

impl Registry {
    /// Роль модуля (запускается, если указана в `roles` узла).
    pub fn add_role(&mut self, role: Arc<dyn RoleService>) -> Result<()> {
        unique(&self.roles, |r| r.name(), role.as_ref())?;
        self.roles.push(role);
        Ok(())
    }

    /// Capability JMAP.
    pub fn add_jmap_capability(&mut self, cap: Arc<dyn JmapCapability>) -> Result<()> {
        unique(&self.jmap, |c| c.uri(), cap.as_ref())?;
        self.jmap.push(cap);
        Ok(())
    }

    /// Маршруты Admin API.
    pub fn add_admin_routes(&mut self, routes: Arc<dyn AdminRoutes>) -> Result<()> {
        unique(&self.admin, |r| r.prefix(), routes.as_ref())?;
        self.admin.push(routes);
        Ok(())
    }

    /// Подписчик журнала событий ядра.
    pub fn add_event_sink(&mut self, sink: Arc<dyn EventSink>) -> Result<()> {
        unique(&self.sinks, |s| s.name(), sink.as_ref())?;
        self.sinks.push(sink);
        Ok(())
    }

    /// Точка принятия решений.
    pub fn add_decision_hook(&mut self, hook: Arc<dyn DecisionHook>) -> Result<()> {
        unique(&self.hooks, |h| h.name(), hook.as_ref())?;
        self.hooks.push(hook);
        Ok(())
    }

    /// Реализация хранилища BLOB вместо встроенной.
    pub fn set_blob_store(&mut self, store: Arc<dyn BlobStore>) -> Result<()> {
        set_once(&mut self.blob_store, store)
    }

    /// Реализация каталога вместо встроенной.
    pub fn set_directory(&mut self, directory: Arc<dyn Directory>) -> Result<()> {
        set_once(&mut self.directory, directory)
    }

    /// Реализация поиска вместо встроенной.
    pub fn set_search_index(&mut self, index: Arc<dyn SearchIndex>) -> Result<()> {
        set_once(&mut self.search_index, index)
    }

    /// Роли модулей.
    pub fn roles(&self) -> &[Arc<dyn RoleService>] {
        &self.roles
    }

    /// Capability JMAP модулей.
    pub fn jmap_capabilities(&self) -> &[Arc<dyn JmapCapability>] {
        &self.jmap
    }

    /// Маршруты Admin API модулей.
    pub fn admin_routes(&self) -> &[Arc<dyn AdminRoutes>] {
        &self.admin
    }

    /// Подписчики журнала событий.
    pub fn event_sinks(&self) -> &[Arc<dyn EventSink>] {
        &self.sinks
    }

    /// Точки принятия решений в порядке регистрации.
    pub fn decision_hooks(&self) -> DecisionHooks {
        DecisionHooks::new(self.hooks.clone())
    }

    /// Хранилище BLOB модуля, если зарегистрировано.
    pub fn blob_store(&self) -> Option<Arc<dyn BlobStore>> {
        self.blob_store.clone()
    }

    /// Каталог модуля, если зарегистрирован.
    pub fn directory(&self) -> Option<Arc<dyn Directory>> {
        self.directory.clone()
    }

    /// Поиск модуля, если зарегистрирован.
    pub fn search_index(&self) -> Option<Arc<dyn SearchIndex>> {
        self.search_index.clone()
    }
}

impl fmt::Debug for Registry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Registry")
            .field(
                "roles",
                &self.roles.iter().map(|r| r.name()).collect::<Vec<_>>(),
            )
            .field(
                "jmap",
                &self.jmap.iter().map(|c| c.uri()).collect::<Vec<_>>(),
            )
            .field(
                "admin",
                &self.admin.iter().map(|r| r.prefix()).collect::<Vec<_>>(),
            )
            .field(
                "sinks",
                &self.sinks.iter().map(|s| s.name()).collect::<Vec<_>>(),
            )
            .field("hooks", &self.decision_hooks())
            .field("blob_store", &self.blob_store.is_some())
            .field("directory", &self.directory.is_some())
            .field("search_index", &self.search_index.is_some())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::AccountId;
    use futures::executor::block_on;

    #[test]
    fn object_safe() {
        fn _module(_: Arc<dyn Module>) {}
        fn _hook(_: Arc<dyn DecisionHook>) {}
        fn _jmap(_: Arc<dyn JmapCapability>) {}
        fn _admin(_: Arc<dyn AdminRoutes>) {}
    }

    struct Hook(&'static str, Decision);

    #[async_trait]
    impl DecisionHook for Hook {
        fn name(&self) -> &str {
            self.0
        }

        async fn on_delivery(&self, _op: &DeliveryInfo) -> Result<Decision> {
            Ok(self.1.clone())
        }

        async fn before_final_delete(&self, _op: &FinalDelete) -> Result<Decision> {
            Ok(self.1.clone())
        }
    }

    struct Passive;

    impl DecisionHook for Passive {
        fn name(&self) -> &str {
            "passive"
        }
    }

    fn header(v: &str) -> Decision {
        Decision::Augment(Augment {
            headers: vec![("X-Journal".into(), v.into())],
            copies: vec![],
        })
    }

    fn delivery() -> DeliveryInfo {
        DeliveryInfo {
            tenant: TenantId::DEFAULT,
            mail_from: String::new(),
            deliveries: vec![],
            size: 1,
        }
    }

    #[test]
    fn hooks_combine() {
        let hooks = |v: Vec<Arc<dyn DecisionHook>>| DecisionHooks::new(v);
        let info = delivery();
        let passive = hooks(vec![Arc::new(Passive)]);
        assert_eq!(block_on(passive.on_delivery(&info)), Ok(Decision::Allow));

        let merged = hooks(vec![
            Arc::new(Hook("a", header("1"))),
            Arc::new(Passive),
            Arc::new(Hook("b", header("2"))),
        ]);
        let Ok(Decision::Augment(a)) = block_on(merged.on_delivery(&info)) else {
            panic!("expected augment");
        };
        assert_eq!(a.headers.len(), 2);
        assert_eq!(a.headers[1].1, "2");

        let denied = hooks(vec![
            Arc::new(Hook("a", header("1"))),
            Arc::new(Hook("hold", Decision::Deny("legal hold".into()))),
        ]);
        assert_eq!(
            block_on(denied.on_delivery(&info)),
            Ok(Decision::Deny("legal hold".into()))
        );

        let del = FinalDelete {
            scope: AccountScope {
                tenant: TenantId::DEFAULT,
                account: AccountId::new(),
            },
            mailbox: MailboxId::new(),
            message: MessageId::new(),
            received_at: OffsetDateTime::UNIX_EPOCH,
        };
        assert_eq!(
            block_on(merged.before_final_delete(&del)),
            Ok(Decision::Allow)
        );
        assert_eq!(
            block_on(denied.before_final_delete(&del)),
            Ok(Decision::Deny("legal hold".into()))
        );
    }

    #[test]
    fn registry_unique() {
        let mut reg = Registry::default();
        reg.add_decision_hook(Arc::new(Passive)).expect("first");
        assert_eq!(
            reg.add_decision_hook(Arc::new(Passive)),
            Err(Error::Conflict)
        );
        assert!(!reg.decision_hooks().is_empty());
        assert!(reg.blob_store().is_none());
        assert!(format!("{reg:?}").contains("passive"));
    }

    #[test]
    fn module_name_and_schema() {
        struct M;
        impl Module for M {
            fn name(&self) -> &str {
                "compliance"
            }
            fn migrations(&self) -> &[Migration] {
                &[]
            }
            fn register(&self, _: &mut Registry, _: &ModuleCtx) -> Result<()> {
                Ok(())
            }
        }
        assert_eq!(M.schema(), "mod_compliance");
        assert!(validate_module_name("compliance").is_ok());
        assert!(validate_module_name("multi_tenant2").is_ok());
        for bad in ["", "2x", "Mod", "a-b", &"a".repeat(60)] {
            assert!(validate_module_name(bad).is_err(), "{bad}");
        }
    }
}
