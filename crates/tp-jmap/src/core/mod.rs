//! Ядро JMAP (RFC 8620): объект сессии, запрос API, ссылки на результаты, ошибки, лимиты,
//! аутентификация Bearer и Basic, реестр capability (встроенные и модулей, CONTRACTS.md#Extension points).
//!
//! Остановка узла (SRS 4.2): [`Jmap::shutdown`] закрывает долгоживущие соединения push (EventSource, WebSocket).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use tp_contracts::auth::{Authenticator, Authorizer, Principal};
use tp_contracts::blob::{BlobIndex, BlobStore};
use tp_contracts::bus::EventBus;
use tp_contracts::dav::DavStore;
use tp_contracts::ext::{DecisionHooks, JmapCapability};
use tp_contracts::mailstore::MailStore;
use tp_contracts::meta::{
    AccountScope, AccountStore, ChangeLog, MailboxStore, Message, MessageQuery, MessageStore,
    PolicyResolver,
};
use tp_contracts::queue::Outbound;
use tp_contracts::retention::TrashStore;
use tp_contracts::search::SearchIndex;
use tp_contracts::sieve::SieveStore;
use tp_contracts::transport::MailFlow;
use tp_contracts::{AccountId, Error, MailboxId, ModSeq, Result, TenantId};

use crate::autoconfig::sign::ProfileSigner;

pub mod config;
pub mod error;
pub mod http;
pub mod request;
#[cfg(test)]
mod tests;

pub use config::JmapConfig;
pub use error::{MethodError, MethodResult, Problem};
pub use request::{Invocation, Request, Response};

/// Capability ядра.
pub const CORE: &str = "urn:ietf:params:jmap:core";
/// Capability почты (RFC 8621).
pub const MAIL: &str = "urn:ietf:params:jmap:mail";
/// Capability отправки (RFC 8621 §7).
pub const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
/// Capability WebSocket (RFC 8887).
pub const WEBSOCKET: &str = "urn:ietf:params:jmap:websocket";
/// Capability настроек пользователя The Post (`UserSettings`: `trash.purge`).
pub const SETTINGS: &str = "urn:thepost:params:jmap:settings";

/// Capability автоответа (RFC 8621 §8); доступна при подключённом [`Jmap::with_sieve`].
pub const VACATION: &str = "urn:ietf:params:jmap:vacationresponse";
/// Capability скриптов Sieve (RFC 9661); доступна при подключённом [`Jmap::with_sieve`].
pub const SIEVE: &str = "urn:ietf:params:jmap:sieve";
/// Capability контактов (RFC 9610); доступна при подключённом [`Jmap::with_dav`].
pub const CONTACTS: &str = "urn:ietf:params:jmap:contacts";

/// Встроенные capability.
const BUILTIN: [&str; 5] = [CORE, MAIL, SUBMISSION, WEBSOCKET, SETTINGS];
/// Встроенные capability, доступные при подключённом хранилище (URI заняты всегда).
const OPTIONAL: [&str; 3] = [VACATION, SIEVE, CONTACTS];

/// Зависимости сервера JMAP: реализации контрактов, выбранные приложением.
#[derive(Clone)]
pub struct Deps {
    /// Проверка Bearer-токенов и паролей приложений.
    pub auth: Arc<dyn Authenticator>,
    /// Папки.
    pub mailboxes: Arc<dyn MailboxStore>,
    /// Метаданные писем.
    pub messages: Arc<dyn MessageStore>,
    /// Журнал изменений (`/changes`, состояния).
    pub changes: Arc<dyn ChangeLog>,
    /// Почтовое хранилище (тела писем).
    pub store: Arc<dyn MailStore>,
    /// Корзина: `Email/set destroy` в корзине — «очистить корзину» (`TrashStore::purge`).
    pub trash: Arc<dyn TrashStore>,
    /// Политики: настройка `trash.purge` пользователя (`UserSettings`).
    pub policies: Arc<dyn PolicyResolver>,
    /// Полнотекстовый поиск.
    pub search: Arc<dyn SearchIndex>,
    /// Объекты BLOB: загруженные файлы (upload). Чтение — с распаковкой (`tp_blob::Decompressing` поверх
    /// хранилища): загрузка, совпавшая по содержимому со сжатой частью письма, читается из её объекта (MS-15);
    /// без распаковки такая загрузка — `Error::Internal` (проверка хэша), запись — как у хранилища.
    pub blobs: Arc<dyn BlobStore>,
    /// Учёт объектов BLOB: загруженный файл живёт без ссылок до сборки мусора по окну.
    pub blob_index: Arc<dyn BlobIndex>,
    /// Шина уведомлений: push (EventSource, WebSocket).
    pub bus: Arc<dyn EventBus>,
    /// Учётные записи: отображаемое имя в Identity.
    pub accounts: Arc<dyn AccountStore>,
    /// Права отправки от адреса `From` (Send As, Send on Behalf).
    pub authz: Arc<dyn Authorizer>,
    /// Очередь исходящей почты; `None` — узел не отправляет (EmailSubmission — `forbiddenToSend`).
    pub outbound: Option<Arc<dyn Outbound>>,
    /// Точки принятия решений модулей (`on_submission`; `before_final_delete` при очистке корзины).
    pub hooks: DecisionHooks,
    /// Capability модулей (`Registry::jmap_capabilities`).
    pub capabilities: Vec<Arc<dyn JmapCapability>>,
}

impl fmt::Debug for Deps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deps")
            .field("outbound", &self.outbound.is_some())
            .field(
                "capabilities",
                &self
                    .capabilities
                    .iter()
                    .map(|c| c.uri())
                    .collect::<Vec<_>>(),
            )
            .finish_non_exhaustive()
    }
}

/// Сервер JMAP: обработка запросов API и объект сессии; HTTP — [`Jmap::router`].
pub struct Jmap {
    pub(crate) d: Deps,
    pub(crate) cfg: JmapConfig,
    inflight: Slots,
    uploads: Slots,
    /// Загруженные файлы учётных записей (владелец blob `U…`).
    pub(crate) uploaded: crate::blob::Uploads,
    /// Правила потока почты (модуль, `Registry::mail_flow`); `None` — правила не применяются.
    pub(crate) mail_flow: Option<Arc<dyn MailFlow>>,
    /// Ключ подписи профиля `.mobileconfig`; `None` — профиль без подписи.
    pub(crate) profile_signer: Option<Arc<dyn ProfileSigner>>,
    /// Скрипты Sieve и автоответ; `None` — capability `vacationresponse` и `sieve` не объявляются.
    pub(crate) sieve: Option<Arc<dyn SieveStore>>,
    /// Календари и контакты; `None` — capability `contacts` не объявляется.
    pub(crate) dav: Option<Arc<dyn DavStore>>,
    stop: watch::Sender<bool>,
}

impl fmt::Debug for Jmap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Jmap")
            .field("d", &self.d)
            .field("cfg", &self.cfg)
            .field("mail_flow", &self.mail_flow.is_some())
            .field("profile_signer", &self.profile_signer.is_some())
            .field("sieve", &self.sieve.is_some())
            .field("dav", &self.dav.is_some())
            .finish_non_exhaustive()
    }
}

/// Контекст вызова метода.
pub(crate) struct Ctx<'a> {
    pub jmap: &'a Jmap,
    pub principal: &'a Principal,
    pub scope: AccountScope,
    /// Созданные в запросе объекты: creation id → id (ссылки `#cid`).
    pub created: &'a mut HashMap<String, String>,
    /// Ответы неявных вызовов (`EmailSubmission/set` → `Email/set`): после ответа метода, с тем же callId.
    pub implicit: Vec<(String, Map<String, Value>)>,
}

impl Ctx<'_> {
    /// Идентификатор учётной записи JMAP.
    pub fn account_id(&self) -> String {
        self.scope.account.to_string()
    }

    /// Проверяет `accountId` аргументов: доступна только своя учётная запись.
    pub fn check_account(&self, account_id: &str) -> MethodResult<()> {
        if account_id == self.account_id() {
            Ok(())
        } else {
            Err(MethodError::new("accountNotFound"))
        }
    }

    /// Проверяет число объектов в `/get`.
    pub fn check_get_limit(&self, ids: Option<&Vec<String>>) -> MethodResult<()> {
        match ids {
            Some(ids) if ids.len() > self.jmap.cfg.max_objects_in_get as usize => {
                Err(MethodError::new("requestTooLarge"))
            }
            _ => Ok(()),
        }
    }

    /// Идентификатор из аргумента: `#cid` — объект, созданный ранее в этом запросе (RFC 8620 §5.3).
    pub fn resolve_id<'b>(&'b self, id: &'b str) -> &'b str {
        id.strip_prefix('#')
            .and_then(|cid| self.created.get(cid))
            .map_or(id, String::as_str)
    }

    /// Текущее состояние учётной записи.
    pub async fn state(&self) -> MethodResult<ModSeq> {
        Ok(self.jmap.state(self.scope).await?)
    }
}

type Slots = Mutex<HashMap<(TenantId, AccountId), u32>>;

/// Слот одновременного запроса или загрузки учётной записи; освобождается при удалении.
pub(crate) struct Inflight<'a> {
    slots: &'a Slots,
    key: (TenantId, AccountId),
}

impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        let mut map = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(n) = map.get_mut(&self.key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(&self.key);
            }
        }
    }
}

impl Jmap {
    /// Сервер поверх зависимостей. URI capability модуля, совпадающий со встроенным или другим модулем,
    /// и повтор имени метода — `Error::Conflict`.
    pub fn new(d: Deps, cfg: JmapConfig) -> Result<Self> {
        let mut uris = BUILTIN.to_vec();
        uris.extend(OPTIONAL);
        let mut methods: Vec<&str> = crate::METHODS.iter().map(|(m, _)| *m).collect();
        for cap in &d.capabilities {
            if uris.contains(&cap.uri()) {
                return Err(Error::Conflict);
            }
            uris.push(cap.uri());
            for m in cap.methods() {
                if methods.contains(m) {
                    return Err(Error::Conflict);
                }
                methods.push(m);
            }
        }
        Ok(Self {
            d,
            cfg,
            inflight: Mutex::new(HashMap::new()),
            uploads: Mutex::new(HashMap::new()),
            uploaded: crate::blob::Uploads::default(),
            mail_flow: None,
            profile_signer: None,
            sieve: None,
            dav: None,
            stop: watch::channel(false).0,
        })
    }

    /// Подключает правила потока почты (CONTRACTS.md#Transport, `Registry::mail_flow`): `EmailSubmission/set`
    /// вызывает [`MailFlow::apply`] один раз на письмо до `DecisionHooks::on_submission` и очереди.
    /// `None` — правила не применяются (модуль не подключён).
    #[must_use]
    pub fn with_mail_flow(mut self, flow: Option<Arc<dyn MailFlow>>) -> Self {
        self.mail_flow = flow;
        self
    }

    /// Подключает подпись профиля `.mobileconfig` (JMAP-22): CMS SignedData ключом и цепочкой TLS-сертификата
    /// сервера, ключ — [`ProfileSigner::key`] на каждый запрос профиля. `None` — профиль без подписи
    /// (предупреждение в журнале при выдаче).
    #[must_use]
    pub fn with_profile_signer(mut self, signer: Option<Arc<dyn ProfileSigner>>) -> Self {
        self.profile_signer = signer;
        self
    }

    /// Подключает скрипты Sieve и автоответ (CONTRACTS.md#Sieve): capability
    /// `urn:ietf:params:jmap:vacationresponse` (JMAP-23) и `urn:ietf:params:jmap:sieve` (JMAP-24).
    /// `None` — capability не объявляются, их методы — `unknownMethod`.
    #[must_use]
    pub fn with_sieve(mut self, sieve: Option<Arc<dyn SieveStore>>) -> Self {
        self.sieve = sieve;
        self
    }

    /// Подключает хранилище календарей и контактов (CONTRACTS.md#Dav): capability
    /// `urn:ietf:params:jmap:contacts` (JMAP-25, JMAP-26). `None` — capability не объявляется.
    #[must_use]
    pub fn with_dav(mut self, dav: Option<Arc<dyn DavStore>>) -> Self {
        self.dav = dav;
        self
    }

    /// Хранилище Sieve для метода; не подключено — `unknownMethod`.
    pub(crate) fn sieve_store(&self) -> MethodResult<&Arc<dyn SieveStore>> {
        self.sieve.as_ref().ok_or(MethodError::new("unknownMethod"))
    }

    /// Хранилище DAV для метода; не подключено — `unknownMethod`.
    pub(crate) fn dav_store(&self) -> MethodResult<&Arc<dyn DavStore>> {
        self.dav.as_ref().ok_or(MethodError::new("unknownMethod"))
    }

    /// Подключена ли встроенная capability, зависящая от хранилища.
    fn optional_enabled(&self, uri: &str) -> bool {
        match uri {
            VACATION | SIEVE => self.sieve.is_some(),
            CONTACTS => self.dav.is_some(),
            _ => false,
        }
    }

    /// Остановка узла: закрывает соединения push (EventSource завершается, WebSocket получает Close).
    /// Запросы API обслуживаются до остановки слушателя вызывающим.
    pub fn shutdown(&self) {
        self.stop.send_replace(true);
    }

    /// Признак остановки для соединений push.
    pub(crate) fn stopping(&self) -> watch::Receiver<bool> {
        self.stop.subscribe()
    }

    /// Конфигурация.
    pub fn config(&self) -> &JmapConfig {
        &self.cfg
    }

    fn known_capability(&self, uri: &str) -> bool {
        BUILTIN.contains(&uri)
            || self.optional_enabled(uri)
            || self.d.capabilities.iter().any(|c| c.uri() == uri)
    }

    /// Объект сессии (RFC 8620 §2) для субъекта; `base` — внешний адрес сервера без `/` в конце.
    pub fn session(&self, p: &Principal, base: &str) -> Value {
        let c = &self.cfg;
        let account = p.account.to_string();
        let mut caps = Map::new();
        caps.insert(
            CORE.into(),
            json!({
                "maxSizeUpload": c.max_size_upload,
                "maxConcurrentUpload": c.max_concurrent_upload,
                "maxSizeRequest": c.max_size_request,
                "maxConcurrentRequests": c.max_concurrent_requests,
                "maxCallsInRequest": c.max_calls_in_request,
                "maxObjectsInGet": c.max_objects_in_get,
                "maxObjectsInSet": c.max_objects_in_set,
                "collationAlgorithms": ["i;ascii-casemap", "i;unicode-casemap"],
            }),
        );
        caps.insert(MAIL.into(), json!({}));
        caps.insert(SUBMISSION.into(), json!({}));
        caps.insert(SETTINGS.into(), json!({}));
        let ws = base
            .strip_prefix("https://")
            .map(|h| format!("wss://{h}"))
            .or_else(|| base.strip_prefix("http://").map(|h| format!("ws://{h}")))
            .unwrap_or_else(|| base.to_owned());
        caps.insert(
            WEBSOCKET.into(),
            json!({ "url": format!("{ws}/jmap/ws"), "supportsPush": true }),
        );
        let mut account_caps = Map::new();
        account_caps.insert(
            MAIL.into(),
            json!({
                "maxMailboxesPerEmail": 1,
                "maxMailboxDepth": null,
                "maxSizeMailboxName": 255,
                "maxSizeAttachmentsPerEmail": c.max_size_upload,
                "emailQuerySortOptions": crate::EMAIL_SORT_OPTIONS,
                "mayCreateTopLevelMailbox": true,
            }),
        );
        account_caps.insert(
            SUBMISSION.into(),
            json!({ "maxDelayedSend": 0, "submissionExtensions": {} }),
        );
        account_caps.insert(SETTINGS.into(), json!({}));
        let mut primary = Map::new();
        primary.insert(MAIL.into(), json!(account));
        primary.insert(SUBMISSION.into(), json!(account));
        primary.insert(SETTINGS.into(), json!(account));
        if let Some(sieve) = &self.sieve {
            caps.insert(VACATION.into(), json!({}));
            account_caps.insert(VACATION.into(), json!({}));
            primary.insert(VACATION.into(), json!(account));
            caps.insert(SIEVE.into(), crate::sieve::session_capability());
            account_caps.insert(
                SIEVE.into(),
                crate::sieve::account_capability(sieve.limits()),
            );
            primary.insert(SIEVE.into(), json!(account));
        }
        if self.dav.is_some() {
            caps.insert(CONTACTS.into(), json!({}));
            account_caps.insert(CONTACTS.into(), crate::contacts::account_capability());
            primary.insert(CONTACTS.into(), json!(account));
        }
        for cap in &self.d.capabilities {
            caps.insert(cap.uri().into(), cap.session_object());
            account_caps.insert(cap.uri().into(), json!({}));
            primary.insert(cap.uri().into(), json!(account));
        }
        let mut accounts = Map::new();
        accounts.insert(
            account.clone(),
            json!({
                "name": p.login,
                "isPersonal": true,
                "isReadOnly": false,
                "accountCapabilities": account_caps,
            }),
        );
        let mut session = json!({
            "capabilities": caps,
            "accounts": accounts,
            "primaryAccounts": primary,
            "username": p.login,
            "apiUrl": format!("{base}/jmap/api"),
            "downloadUrl": format!("{base}/jmap/download/{{accountId}}/{{blobId}}/{{name}}?accept={{type}}"),
            "uploadUrl": format!("{base}/jmap/upload/{{accountId}}/"),
            "eventSourceUrl": format!("{base}/jmap/eventsource/?types={{types}}&closeafter={{closeafter}}&ping={{ping}}"),
        });
        let state = digest(session.to_string().as_bytes());
        session["state"] = json!(state);
        session
    }

    fn session_state(&self, p: &Principal) -> String {
        self.session(p, "")["state"]
            .as_str()
            .unwrap_or_default()
            .to_owned()
    }

    fn enter(&self, p: &Principal) -> Option<Inflight<'_>> {
        slot(&self.inflight, p, self.cfg.max_concurrent_requests)
    }

    /// Слот загрузки (`maxConcurrentUpload`); `None` — лимит исчерпан.
    pub(crate) fn enter_upload(&self, p: &Principal) -> Option<Inflight<'_>> {
        slot(&self.uploads, p, self.cfg.max_concurrent_upload)
    }

    /// Выполняет запрос API (тело POST `apiUrl`). Ошибка всего запроса — [`Problem`].
    pub async fn api(&self, p: &Principal, body: &[u8]) -> std::result::Result<Response, Problem> {
        let req = request::parse(body, self.cfg.max_size_request)?;
        if let Some(uri) = req.using.iter().find(|u| !self.known_capability(u)) {
            return Err(Problem::unknown_capability(uri));
        }
        if req.method_calls.len() > self.cfg.max_calls_in_request as usize {
            return Err(Problem::limit("maxCallsInRequest"));
        }
        let _slot = self
            .enter(p)
            .ok_or(Problem::limit("maxConcurrentRequests"))?;
        let echo_created = req.created_ids.is_some();
        let mut created = req.created_ids.unwrap_or_default();
        let mut responses: Vec<Invocation> = Vec::with_capacity(req.method_calls.len());
        // Подстановки ссылок за запрос — не больше самого запроса по лимиту (`maxSizeRequest`).
        let mut budget = self.cfg.max_size_request;
        for Invocation(name, args, call_id) in req.method_calls {
            let mut ctx = Ctx {
                jmap: self,
                principal: p,
                scope: AccountScope {
                    tenant: p.tenant,
                    account: p.account,
                },
                created: &mut created,
                implicit: Vec::new(),
            };
            let result = match request::resolve_references(args, &responses, &mut budget) {
                Ok(args) => self.call(&mut ctx, &name, args, &req.using).await,
                Err(e) => Err(e),
            };
            let implicit = std::mem::take(&mut ctx.implicit);
            responses.push(match result {
                Ok(out) => Invocation(name, out, call_id.clone()),
                Err(e) => Invocation(
                    "error".into(),
                    request::object(e.to_json()),
                    call_id.clone(),
                ),
            });
            for (name, out) in implicit {
                responses.push(Invocation(name, out, call_id.clone()));
            }
        }
        Ok(Response {
            method_responses: responses,
            created_ids: echo_created.then_some(created),
            session_state: self.session_state(p),
        })
    }

    async fn call(
        &self,
        ctx: &mut Ctx<'_>,
        name: &str,
        args: Map<String, Value>,
        using: &[String],
    ) -> MethodResult<Map<String, Value>> {
        let uses = |uri: &str| using.iter().any(|u| u == uri);
        if let Some((_, cap)) = crate::METHODS.iter().find(|(m, _)| *m == name) {
            if !uses(cap) {
                return Err(MethodError::new("unknownMethod"));
            }
            return crate::call_builtin(ctx, name, args).await;
        }
        let cap = self
            .d
            .capabilities
            .iter()
            .find(|c| c.methods().contains(&name))
            .filter(|c| uses(c.uri()))
            .ok_or(MethodError::new("unknownMethod"))?;
        match cap.call(ctx.principal, name, Value::Object(args)).await? {
            Value::Object(m) => Ok(m),
            _ => Err(MethodError::server_fail("module returned non-object")),
        }
    }

    /// Текущее состояние учётной записи (наибольший `ModSeq`): от наибольшего `ModSeq` папок
    /// дочитывается журнал изменений папок (удаление пустой папки не меняет ни одну папку).
    pub(crate) async fn state(&self, scope: AccountScope) -> Result<ModSeq> {
        let mut base = ModSeq::ZERO;
        for mb in self.d.mailboxes.list(scope).await? {
            base = base.max(self.d.mailboxes.status(scope, mb.id).await?.highest_modseq);
        }
        loop {
            match self
                .d
                .changes
                .mailbox_changes(scope, base, self.cfg.max_changes as usize)
                .await
            {
                Ok(c) if c.has_more => base = c.new_state,
                Ok(c) => return Ok(c.new_state),
                Err(Error::Conflict) => return Ok(base),
                Err(e) => return Err(e),
            }
        }
    }

    /// Письма учётной записи: одной папки или всех.
    pub(crate) async fn messages_in(
        &self,
        scope: AccountScope,
        mailbox: Option<MailboxId>,
    ) -> Result<Vec<Message>> {
        let boxes = match mailbox {
            Some(m) => vec![m],
            None => self
                .d
                .mailboxes
                .list(scope)
                .await?
                .into_iter()
                .map(|m| m.id)
                .collect(),
        };
        let mut out = Vec::new();
        for m in boxes {
            out.extend(
                self.d
                    .messages
                    .list(scope, m, MessageQuery::All, usize::MAX)
                    .await?,
            );
        }
        Ok(out)
    }
}

fn slot<'a>(slots: &'a Slots, p: &Principal, max: u32) -> Option<Inflight<'a>> {
    let key = (p.tenant, p.account);
    let mut map = slots.lock().unwrap_or_else(PoisonError::into_inner);
    let n = map.entry(key).or_insert(0);
    if *n >= max {
        return None;
    }
    *n += 1;
    Some(Inflight { slots, key })
}

/// Идентификатор цепочки письма (`threadId`): по ключу цепочки из метаданных; письмо без ключа — своя цепочка.
pub(crate) fn thread_id(m: &Message) -> String {
    let key = if m.meta.thread_key.is_empty() {
        format!("id:{}", m.id)
    } else {
        m.meta.thread_key.clone()
    };
    format!("T{}", digest(key.as_bytes()))
}

/// Первые 12 байт SHA-256 в hex.
pub(crate) fn digest(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .take(12)
        .map(|b| format!("{b:02x}"))
        .collect()
}
