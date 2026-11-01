//! Ошибки JMAP (RFC 8620 §3.6): ошибки запроса (problem details, RFC 7807), ошибки методов, ошибки `/set`.

use serde_json::{Map, Value, json};
use tp_contracts::Error;

/// Ошибка всего запроса: ответ HTTP 4xx с `application/problem+json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    /// Код HTTP.
    pub status: u16,
    /// Тип (`urn:ietf:params:jmap:error:...`).
    pub kind: String,
    /// Описание для разработчика клиента.
    pub detail: String,
    /// Превышенный лимит (`maxCallsInRequest`, ...) для `urn:ietf:params:jmap:error:limit`.
    pub limit: Option<&'static str>,
}

impl Problem {
    fn new(kind: &str, detail: impl Into<String>) -> Self {
        Self {
            status: 400,
            kind: format!("urn:ietf:params:jmap:error:{kind}"),
            detail: detail.into(),
            limit: None,
        }
    }

    /// Тело не JSON.
    pub fn not_json(detail: impl Into<String>) -> Self {
        Self::new("notJSON", detail)
    }

    /// JSON, но не объект Request.
    pub fn not_request(detail: impl Into<String>) -> Self {
        Self::new("notRequest", detail)
    }

    /// Неизвестная capability в `using`.
    pub fn unknown_capability(uri: &str) -> Self {
        Self::new("unknownCapability", format!("unknown capability: {uri}"))
    }

    /// Превышен лимит запроса.
    pub fn limit(limit: &'static str) -> Self {
        Self {
            limit: Some(limit),
            ..Self::new("limit", format!("limit exceeded: {limit}"))
        }
    }

    /// Ошибка HTTP вне запроса API (загрузка, скачивание, push): код и описание, тип `about:blank`.
    pub fn http(status: u16, detail: impl Into<String>) -> Self {
        Self {
            status,
            kind: "about:blank".into(),
            detail: detail.into(),
            limit: None,
        }
    }

    /// Тело problem details.
    pub fn to_json(&self) -> Value {
        let mut v = json!({ "type": self.kind, "status": self.status, "detail": self.detail });
        if let Some(limit) = self.limit {
            v["limit"] = json!(limit);
        }
        v
    }
}

/// Ошибка метода: ответ `["error", {"type": ...}, callId]`, остальные вызовы запроса выполняются.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MethodError {
    /// Тип ошибки (`invalidArguments`, ...).
    pub kind: &'static str,
    /// Описание.
    pub description: Option<String>,
}

impl MethodError {
    /// Ошибка без описания.
    pub const fn new(kind: &'static str) -> Self {
        Self {
            kind,
            description: None,
        }
    }

    /// Ошибка с описанием.
    pub fn with(kind: &'static str, description: impl Into<String>) -> Self {
        Self {
            kind,
            description: Some(description.into()),
        }
    }

    /// `invalidArguments`.
    pub fn invalid(description: impl Into<String>) -> Self {
        Self::with("invalidArguments", description)
    }

    /// `serverFail`.
    pub fn server_fail(description: impl Into<String>) -> Self {
        Self::with("serverFail", description)
    }

    /// Тело ответа `error`.
    pub fn to_json(&self) -> Value {
        let mut v = json!({ "type": self.kind });
        if let Some(d) = &self.description {
            v["description"] = json!(d);
        }
        v
    }
}

impl From<Error> for MethodError {
    fn from(e: Error) -> Self {
        match e {
            Error::Invalid(m) => Self::invalid(m),
            Error::NotFound => Self::invalid("not found"),
            Error::Forbidden => Self::new("forbidden"),
            Error::Unavailable { .. } => Self::new("serverUnavailable"),
            Error::Conflict => Self::server_fail("conflict"),
            Error::QuotaExceeded => Self::server_fail("quota exceeded"),
            Error::Internal(m) => {
                tracing::error!(error = %m, "jmap: внутренняя ошибка метода");
                Self::new("serverFail")
            }
        }
    }
}

/// Ошибка журнала изменений: устаревшее или чужое состояние (`Conflict`) — `cannotCalculateChanges`.
pub fn changes_error(e: Error) -> MethodError {
    match e {
        Error::Conflict => MethodError::new("cannotCalculateChanges"),
        e => e.into(),
    }
}

/// Результат метода.
pub type MethodResult<T> = std::result::Result<T, MethodError>;

/// Ошибка одного объекта в `/set` (`notCreated`, `notUpdated`, `notDestroyed`).
pub fn set_error(kind: &str, description: Option<&str>, properties: &[&str]) -> Value {
    let mut m = Map::new();
    m.insert("type".into(), json!(kind));
    if let Some(d) = description {
        m.insert("description".into(), json!(d));
    }
    if !properties.is_empty() {
        m.insert("properties".into(), json!(properties));
    }
    Value::Object(m)
}
