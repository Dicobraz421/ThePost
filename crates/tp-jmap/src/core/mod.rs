//! Ядро JMAP (RFC 8620): объект сессии, запрос API, ссылки на результаты, ошибки, лимиты,
//! аутентификация Bearer и Basic, реестр capability (встроенные и модулей, CONTRACTS.md#Extension points).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use tp_contracts::auth::{Authenticator, Principal};
use tp_contracts::ext::JmapCapability;
use tp_contracts::mailstore::MailStore;
use tp_contracts::meta::{
    AccountScope, ChangeLog, MailboxStore, Message, MessageQuery, MessageStore,
};
use tp_contracts::search::SearchIndex;
use tp_contracts::{AccountId, Error, MailboxId, ModSeq, Result, TenantId};

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
    /// Полнотекстовый поиск.
    pub search: Arc<dyn SearchIndex>,
    /// Capability модулей (`Registry::jmap_capabilities`).
    pub capabilities: Vec<Arc<dyn JmapCapability>>,
}

impl fmt::Debug for Deps {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Deps")
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
    inflight: Mutex<HashMap<(TenantId, AccountId), u32>>,
}

impl fmt::Debug for Jmap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Jmap")
            .field("d", &self.d)
            .field("cfg", &self.cfg)
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

    /// Текущее состояние учётной записи.
    pub async fn state(&self) -> MethodResult<ModSeq> {
        Ok(self.jmap.state(self.scope).await?)
    }
}

/// Слот одновременного запроса учётной записи; освобождается при удалении.
struct Inflight<'a> {
    jmap: &'a Jmap,
    key: (TenantId, AccountId),
}

impl Drop for Inflight<'_> {
    fn drop(&mut self) {
        let mut map = self
            .jmap
            .inflight
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
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
        let mut uris = vec![CORE, MAIL];
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
        })
    }

    /// Конфигурация.
    pub fn config(&self) -> &JmapConfig {
        &self.cfg
    }

    fn known_capability(&self, uri: &str) -> bool {
        uri == CORE || uri == MAIL || self.d.capabilities.iter().any(|c| c.uri() == uri)
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
        let mut primary = Map::new();
        primary.insert(MAIL.into(), json!(account));
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
        let key = (p.tenant, p.account);
        let mut map = self.inflight.lock().unwrap_or_else(PoisonError::into_inner);
        let n = map.entry(key).or_insert(0);
        if *n >= self.cfg.max_concurrent_requests {
            return None;
        }
        *n += 1;
        Some(Inflight { jmap: self, key })
    }

    /// Выполняет запрос API (тело POST `apiUrl`). Ошибка всего запроса — [`Problem`].
    pub async fn api(&self, p: &Principal, body: &[u8]) -> std::result::Result<Response, Problem> {
        if body.len() as u64 > self.cfg.max_size_request {
            return Err(Problem::limit("maxSizeRequest"));
        }
        let value: Value =
            serde_json::from_slice(body).map_err(|e| Problem::not_json(e.to_string()))?;
        let req: Request =
            serde_json::from_value(value).map_err(|e| Problem::not_request(e.to_string()))?;
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
        for Invocation(name, args, call_id) in req.method_calls {
            let mut ctx = Ctx {
                jmap: self,
                principal: p,
                scope: AccountScope {
                    tenant: p.tenant,
                    account: p.account,
                },
                created: &mut created,
            };
            let result = match request::resolve_references(args, &responses) {
                Ok(args) => self.call(&mut ctx, &name, args, &req.using).await,
                Err(e) => Err(e),
            };
            responses.push(match result {
                Ok(out) => Invocation(name, out, call_id),
                Err(e) => Invocation("error".into(), request::object(e.to_json()), call_id),
            });
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

/// Первые 12 байт SHA-256 в hex.
pub(crate) fn digest(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .take(12)
        .map(|b| format!("{b:02x}"))
        .collect()
}
