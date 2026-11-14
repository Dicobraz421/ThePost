//! Правила Sieve и автоответ учётной записи, скрипт организации (SRS 4.3, 5.2; ADM-14).
//!
//! Администратор ведёт скрипты и автоответ за пользователя (как в Exchange Admin Center): список, текст, замена,
//! удаление, выбор активного; скрипт организации выполняется до скрипта пользователя у всех её учётных записей.
//! Текст проверяется до записи ([`ScriptCheck`], у узла — разбор с пределами секции `sieve`): ошибки с позициями — 422
//! ([`ScriptProblem`]), в хранилище такой текст не попадает. Лимиты хранилища — 507, удаление активного скрипта — 409.
//! Хранилище подключает [`Admin::with_sieve`]; без него маршруты отвечают 503. Тексты скриптов и автоответа в аудит
//! не попадают (запись — метод, маршрут, путь, код).

use std::fmt;
use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};
use time::{OffsetDateTime, UtcOffset};
use tp_contracts::meta::AccountScope;
use tp_contracts::sieve::{ScriptInfo, SieveStore, Vacation};
use tp_contracts::{AccountId, Error, TenantId};
use utoipa::{OpenApi, ToSchema};

use crate::accounts::account;
use crate::extract::{Json, Path};
use crate::server::{Admin, ApiResult, Problem};

/// Ошибка в тексте скрипта с позицией.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ScriptError {
    /// Строка (с 1).
    pub line: u32,
    /// Столбец (с 1, в символах).
    pub column: u32,
    /// Описание.
    pub message: String,
}

/// Проверка текста скрипта до записи. Узел передаёт разбор с пределами секции `sieve` — те же, с которыми скрипт
/// выполняется при доставке и проверяется ManageSieve и JMAP;
/// реализована и для замыканий `Fn(&str) -> Vec<ScriptError>`.
pub trait ScriptCheck: Send + Sync + 'static {
    /// Ошибки текста; пусто — скрипт верен.
    fn check(&self, text: &str) -> Vec<ScriptError>;
}

impl<F> ScriptCheck for F
where
    F: Fn(&str) -> Vec<ScriptError> + Send + Sync + 'static,
{
    fn check(&self, text: &str) -> Vec<ScriptError> {
        self(text)
    }
}

/// Хранилище скриптов и проверка текста.
#[derive(Clone)]
pub(crate) struct Sieve {
    store: Arc<dyn SieveStore>,
    check: Arc<dyn ScriptCheck>,
}

impl fmt::Debug for Sieve {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sieve").finish_non_exhaustive()
    }
}

impl Admin {
    /// Подключает правила Sieve и автоответ ([`crate::sieve`]): хранилище и проверку текста скрипта.
    pub fn with_sieve(mut self, store: Arc<dyn SieveStore>, check: impl ScriptCheck) -> Self {
        self.sieve = Some(Sieve {
            store,
            check: Arc::new(check),
        });
        self
    }
}

/// Скрипт в списке учётной записи.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct Script {
    /// Имя.
    pub name: String,
    /// Размер текста, байт.
    pub size: u64,
    /// Активный (выполняется при доставке); активный не больше одного.
    pub active: bool,
}

impl From<ScriptInfo> for Script {
    fn from(s: ScriptInfo) -> Self {
        Self {
            name: s.name,
            size: s.size,
            active: s.active,
        }
    }
}

/// Скрипт с текстом.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ScriptBody {
    /// Скрипт.
    #[serde(flatten)]
    pub info: Script,
    /// Текст (RFC 5228).
    pub text: String,
}

/// Текст скрипта (запись скрипта учётной записи и организации; чтение скрипта организации).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ScriptText {
    /// Текст (RFC 5228).
    pub text: String,
}

/// Отказ записи скрипта: ошибки текста (RFC 9457 с расширением `errors`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct ScriptProblem {
    /// Код HTTP (422).
    pub status: u16,
    /// Краткое описание кода.
    pub title: String,
    /// Ошибки текста с позициями.
    pub errors: Vec<ScriptError>,
}

/// Автоответ учётной записи (модель JMAP `VacationResponse`, RFC 8621 §8); выполняется до активного скрипта.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct VacationBody {
    /// Включён.
    #[serde(default)]
    pub enabled: bool,
    /// Начало действия; `null` — сразу.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub from: Option<OffsetDateTime>,
    /// Конец действия (не включая); `null` — бессрочно.
    #[serde(default, with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub to: Option<OffsetDateTime>,
    /// Тема; `null` — по умолчанию («Автоответ: <тема письма>»).
    #[serde(default)]
    pub subject: Option<String>,
    /// Текст ответа.
    #[serde(default)]
    pub text_body: Option<String>,
    /// Текст ответа HTML.
    #[serde(default)]
    pub html_body: Option<String>,
}

impl From<Vacation> for VacationBody {
    fn from(v: Vacation) -> Self {
        Self {
            enabled: v.enabled,
            from: v.from,
            to: v.to,
            subject: v.subject,
            text_body: v.text_body,
            html_body: v.html_body,
        }
    }
}

impl From<VacationBody> for Vacation {
    fn from(v: VacationBody) -> Self {
        Self {
            enabled: v.enabled,
            from: v.from.map(|t| t.to_offset(UtcOffset::UTC)),
            to: v.to.map(|t| t.to_offset(UtcOffset::UTC)),
            subject: v.subject,
            text_body: v.text_body,
            html_body: v.html_body,
        }
    }
}

const SCRIPTS: &str = "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts";
const SCRIPT: &str = "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts/{name}";
const ACTIVATE: &str = "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts/{name}/activate";
const DEACTIVATE: &str = "/api/v1/orgs/{org}/accounts/{account}/sieve/deactivate";
const VACATION: &str = "/api/v1/orgs/{org}/accounts/{account}/vacation";
const ORG_SCRIPT: &str = "/api/v1/orgs/{org}/sieve";

/// Маршруты правил Sieve и автоответа.
pub(crate) fn routes(r: Router<Arc<Admin>>) -> Router<Arc<Admin>> {
    r.route(SCRIPTS, get(list_scripts))
        .route(SCRIPT, get(read).put(put).delete(remove))
        .route(ACTIVATE, post(activate))
        .route(DEACTIVATE, post(deactivate))
        .route(VACATION, get(vacation).put(set_vacation))
        .route(
            ORG_SCRIPT,
            get(org_script)
                .put(set_org_script)
                .delete(remove_org_script),
        )
}

/// Спецификация OpenAPI маршрутов модуля.
#[derive(Debug, OpenApi)]
#[openapi(
    paths(
        list_scripts,
        read,
        put,
        remove,
        activate,
        deactivate,
        vacation,
        set_vacation,
        org_script,
        set_org_script,
        remove_org_script
    ),
    components(schemas(
        Script,
        ScriptBody,
        ScriptText,
        ScriptError,
        ScriptProblem,
        VacationBody
    ))
)]
pub(crate) struct Doc;

fn sieve(a: &Admin) -> ApiResult<&Sieve> {
    Ok(a.sieve.as_ref().ok_or(Error::UNAVAILABLE)?)
}

/// Хранилище и область существующей учётной записи.
async fn scoped(a: &Admin, org: TenantId, id: AccountId) -> ApiResult<(&Sieve, AccountScope)> {
    let s = sieve(a)?;
    account(a, org, id).await?;
    Ok((
        s,
        AccountScope {
            tenant: org,
            account: id,
        },
    ))
}

/// Проверка текста до записи: размер — `QuotaExceeded`; ошибки текста — готовый ответ 422.
fn verify(s: &Sieve, text: &str) -> ApiResult<Option<Response>> {
    if text.len() > s.store.limits().max_script_size {
        return Err(Error::QuotaExceeded.into());
    }
    let errors = s.check.check(text);
    if errors.is_empty() {
        return Ok(None);
    }
    let status = StatusCode::UNPROCESSABLE_ENTITY;
    let body = ScriptProblem {
        status: status.as_u16(),
        title: status.canonical_reason().unwrap_or_default().to_owned(),
        errors,
    };
    let mut resp = (status, Json(body)).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/problem+json"),
    );
    Ok(Some(resp))
}

/// Скрипты учётной записи.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts", tag = "sieve",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)")),
    responses((status = 200, body = Vec<Script>), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn list_scripts(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
) -> ApiResult<Json<Vec<Script>>> {
    let (s, scope) = scoped(&a, org, id).await?;
    let list = s.store.list(scope).await?;
    Ok(Json(list.into_iter().map(Script::from).collect()))
}

/// Скрипт с текстом.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts/{name}", tag = "sieve",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)"),
        ("name" = String, Path, description = "Имя скрипта")),
    responses((status = 200, body = ScriptBody), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn read(
    State(a): State<Arc<Admin>>,
    Path((org, id, name)): Path<(TenantId, AccountId, String)>,
) -> ApiResult<Json<ScriptBody>> {
    let (s, scope) = scoped(&a, org, id).await?;
    let text = s.store.get(scope, &name).await?.ok_or(Error::NotFound)?;
    let active = s.store.active(scope).await?.is_some_and(|x| x.name == name);
    Ok(Json(ScriptBody {
        info: Script {
            size: text.len() as u64,
            name,
            active,
        },
        text,
    }))
}

/// Создаёт или заменяет скрипт (активность сохраняется); текст проверяется до записи.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts/{name}", tag = "sieve",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)"),
        ("name" = String, Path, description = "Имя скрипта (до 128 символов, без управляющих)")),
    request_body = ScriptText,
    responses((status = 204), (status = 400, description = "недопустимое имя", body = Problem),
        (status = 404, body = Problem), (status = 422, description = "ошибки в тексте", body = ScriptProblem),
        (status = 507, description = "лимит числа скриптов или размера текста", body = Problem),
        (status = 503, body = Problem)))]
async fn put(
    State(a): State<Arc<Admin>>,
    Path((org, id, name)): Path<(TenantId, AccountId, String)>,
    Json(body): Json<ScriptText>,
) -> ApiResult<Response> {
    let (s, scope) = scoped(&a, org, id).await?;
    if let Some(rejected) = verify(s, &body.text)? {
        return Ok(rejected);
    }
    s.store.put(scope, &name, body.text).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Удаляет скрипт; активный — 409 (сначала снять активность).
#[utoipa::path(delete, path = "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts/{name}", tag = "sieve",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)"),
        ("name" = String, Path, description = "Имя скрипта")),
    responses((status = 204), (status = 404, body = Problem),
        (status = 409, description = "скрипт активен", body = Problem), (status = 503, body = Problem)))]
async fn remove(
    State(a): State<Arc<Admin>>,
    Path((org, id, name)): Path<(TenantId, AccountId, String)>,
) -> ApiResult<StatusCode> {
    let (s, scope) = scoped(&a, org, id).await?;
    s.store.delete(scope, &name).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Делает скрипт активным; прежний активный становится неактивным.
#[utoipa::path(post, path = "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts/{name}/activate", tag = "sieve",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)"),
        ("name" = String, Path, description = "Имя скрипта")),
    responses((status = 204), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn activate(
    State(a): State<Arc<Admin>>,
    Path((org, id, name)): Path<(TenantId, AccountId, String)>,
) -> ApiResult<StatusCode> {
    let (s, scope) = scoped(&a, org, id).await?;
    s.store.activate(scope, Some(&name)).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Снимает активность со всех скриптов учётной записи.
#[utoipa::path(post, path = "/api/v1/orgs/{org}/accounts/{account}/sieve/deactivate", tag = "sieve",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)")),
    responses((status = 204), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn deactivate(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
) -> ApiResult<StatusCode> {
    let (s, scope) = scoped(&a, org, id).await?;
    s.store.activate(scope, None).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Автоответ учётной записи; не задан — выключен.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/accounts/{account}/vacation", tag = "sieve",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)")),
    responses((status = 200, body = VacationBody), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn vacation(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
) -> ApiResult<Json<VacationBody>> {
    let (s, scope) = scoped(&a, org, id).await?;
    Ok(Json(s.store.vacation(scope).await?.into()))
}

/// Заменяет автоответ учётной записи целиком.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/accounts/{account}/vacation", tag = "sieve",
    params(("org" = String, Path, description = "Организация (UUID)"),
        ("account" = String, Path, description = "Учётная запись (UUID)")),
    request_body = VacationBody,
    responses((status = 200, body = VacationBody), (status = 400, body = Problem), (status = 404, body = Problem),
        (status = 503, body = Problem)))]
async fn set_vacation(
    State(a): State<Arc<Admin>>,
    Path((org, id)): Path<(TenantId, AccountId)>,
    Json(body): Json<VacationBody>,
) -> ApiResult<Json<VacationBody>> {
    let (s, scope) = scoped(&a, org, id).await?;
    let v = Vacation::from(body);
    v.validate()?;
    s.store.set_vacation(scope, v.clone()).await?;
    Ok(Json(v.into()))
}

/// Скрипт организации; не задан — 404.
#[utoipa::path(get, path = "/api/v1/orgs/{org}/sieve", tag = "sieve",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 200, body = ScriptText), (status = 404, body = Problem), (status = 503, body = Problem)))]
async fn org_script(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<Json<ScriptText>> {
    let text = sieve(&a)?.store.org_script(org).await?;
    Ok(Json(ScriptText {
        text: text.ok_or(Error::NotFound)?,
    }))
}

/// Задаёт скрипт организации; текст проверяется до записи.
#[utoipa::path(put, path = "/api/v1/orgs/{org}/sieve", tag = "sieve",
    params(("org" = String, Path, description = "Организация (UUID)")),
    request_body = ScriptText,
    responses((status = 204), (status = 422, description = "ошибки в тексте", body = ScriptProblem),
        (status = 507, description = "лимит размера текста", body = Problem), (status = 503, body = Problem)))]
async fn set_org_script(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
    Json(body): Json<ScriptText>,
) -> ApiResult<Response> {
    let s = sieve(&a)?;
    if let Some(rejected) = verify(s, &body.text)? {
        return Ok(rejected);
    }
    s.store.set_org_script(org, Some(body.text)).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Удаляет скрипт организации; повторное удаление — не ошибка.
#[utoipa::path(delete, path = "/api/v1/orgs/{org}/sieve", tag = "sieve",
    params(("org" = String, Path, description = "Организация (UUID)")),
    responses((status = 204), (status = 503, body = Problem)))]
async fn remove_org_script(
    State(a): State<Arc<Admin>>,
    Path(org): Path<TenantId>,
) -> ApiResult<StatusCode> {
    sieve(&a)?.store.set_org_script(org, None).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;
    use serde_json::{Value, json};
    use tp_contracts::sieve::SieveStore;
    use tp_contracts::{AccountId, Error, TenantId};

    use super::sieve;
    use crate::server::Admin;
    use crate::testenv::{Env, config, deps};

    const SCRIPT: &str = "require \"fileinto\";\nfileinto \"Архив\";\n";

    fn base(account: AccountId) -> String {
        format!(
            "/api/v1/orgs/{}/accounts/{account}/sieve",
            TenantId::DEFAULT
        )
    }

    /// Приёмка ADM-14: цикл скрипта, роли, аудит.
    #[tokio::test]
    async fn script_cycle() {
        let env = Env::new();
        let scope = env.account("user@example.org").await;
        let base = base(scope.account);
        let scripts = format!("{base}/scripts");
        let text = |t: &str| Some(json!({ "text": t }));

        let (s, _, b) = env.call("GET", &scripts, Some("org"), None).await;
        assert_eq!((s, b), (StatusCode::OK, json!([])));
        for name in ["a", "b"] {
            let path = format!("{scripts}/{name}");
            let (s, _, b) = env.call("PUT", &path, Some("org"), text(SCRIPT)).await;
            assert_eq!(s, StatusCode::NO_CONTENT, "{b}");
        }
        let (s, _, _) = env
            .call("POST", &format!("{scripts}/a/activate"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (_, _, b) = env.call("GET", &scripts, Some("operator"), None).await;
        let size = SCRIPT.len();
        assert_eq!(
            b,
            json!([{"name": "a", "size": size, "active": true}, {"name": "b", "size": size, "active": false}])
        );
        let (s, _, b) = env
            .call("GET", &format!("{scripts}/a"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(
            b,
            json!({"name": "a", "size": size, "active": true, "text": SCRIPT})
        );

        // Замена текста сохраняет активность; активный не удаляется.
        let path = format!("{scripts}/a");
        let (s, _, _) = env.call("PUT", &path, Some("org"), text("keep;")).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (_, _, b) = env.call("GET", &path, Some("org"), None).await;
        assert_eq!((&b["active"], &b["text"]), (&json!(true), &json!("keep;")));
        let (s, _, _) = env.call("DELETE", &path, Some("org"), None).await;
        assert_eq!(s, StatusCode::CONFLICT);

        let (s, _, _) = env
            .call("POST", &format!("{scripts}/b/activate"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, _, _) = env.call("DELETE", &path, Some("org"), None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, _, _) = env.call("GET", &path, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let (s, _, _) = env
            .call("POST", &format!("{base}/deactivate"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert_eq!(env.sieve.active(scope).await, Ok(None));
        let (s, _, _) = env
            .call("DELETE", &format!("{scripts}/b"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, _, _) = env
            .call("POST", &format!("{scripts}/b/activate"), Some("org"), None)
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        // Аудит: все изменения, без текста скрипта.
        let log = env.audit.records();
        assert_eq!(log.len(), 10);
        assert_eq!(
            log[0].route,
            "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts/{name}"
        );
        assert!(log[0].path.ends_with("/sieve/scripts/a"));
        assert!(!format!("{log:?}").contains("fileinto"));

        // Оператор только читает; без роли — отказ.
        let path = format!("{scripts}/c");
        let (s, _, _) = env
            .call("PUT", &path, Some("operator"), text("keep;"))
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = env.call("GET", &scripts, Some("user"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert_eq!(env.sieve.list(scope).await, Ok(vec![]));
    }

    /// Приёмка ADM-17 (решение 62): администратор находит у пользователя правило пересылки, отключает его и
    /// включает обратно; каждое действие — в журнале аудита (кто, что, код), без текста правила.
    #[tokio::test]
    async fn admin_disables_forwarding_rule() {
        const FORWARD: &str = "redirect \"outside@example.net\";\n";
        let env = Env::new();
        let scope = env.account("user@example.org").await;
        // Правило создал и включил сам пользователь (ManageSieve, JMAP) — в аудите Admin API его нет.
        env.sieve
            .put(scope, "пересылка", FORWARD.into())
            .await
            .expect("put");
        env.sieve
            .activate(scope, Some("пересылка"))
            .await
            .expect("activate");
        let base = base(scope.account);
        let scripts = format!("{base}/scripts");
        let script = format!("{scripts}/пересылка");

        let (s, _, b) = env.call("GET", &scripts, Some("org"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(
            b,
            json!([{"name": "пересылка", "size": FORWARD.len(), "active": true}])
        );
        let (s, _, b) = env.call("GET", &script, Some("org"), None).await;
        assert_eq!((s, &b["text"]), (StatusCode::OK, &json!(FORWARD)));

        // Оператор отключить не может; администратор организации — отключает.
        let off = format!("{base}/deactivate");
        let (s, _, _) = env.call("POST", &off, Some("operator"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        assert!(env.sieve.active(scope).await.expect("active").is_some());
        let (s, _, _) = env.call("POST", &off, Some("org"), None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        assert_eq!(env.sieve.active(scope).await, Ok(None));
        // Правило не удалено: текст на месте, только не выполняется.
        let (_, _, b) = env.call("GET", &script, Some("org"), None).await;
        assert_eq!((&b["active"], &b["text"]), (&json!(false), &json!(FORWARD)));

        // Правка с той же проверкой и включение обратно.
        let bad = Some(json!({"text": "oops;"}));
        let (s, _, _) = env.call("PUT", &script, Some("org"), bad).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            env.sieve.get(scope, "пересылка").await,
            Ok(Some(FORWARD.into()))
        );
        let on = format!("{script}/activate");
        let (s, _, _) = env.call("POST", &on, Some("org"), None).await;
        assert_eq!(s, StatusCode::NO_CONTENT);

        // Аудит: отказ оператору, отключение, отклонённая правка, включение — чтения не пишутся.
        let log = env.audit.records();
        let got: Vec<(&str, &str, u16)> = log
            .iter()
            .map(|r| (r.method.as_str(), r.route.as_str(), r.status))
            .collect();
        assert_eq!(
            got,
            [
                ("POST", super::DEACTIVATE, 403),
                ("POST", super::DEACTIVATE, 204),
                ("PUT", super::SCRIPT, 422),
                ("POST", super::ACTIVATE, 204),
            ]
        );
        let done = &log[1];
        assert_eq!(done.tenant, Some(TenantId::DEFAULT));
        assert!(!done.actor_login.is_empty());
        assert!(done.path.contains(&scope.account.to_string()), "{done:?}");
        assert!(!format!("{log:?}").contains("outside@example.net"));
    }

    /// Приёмка ADM-14: неверный скрипт — 422 с позициями, в хранилище не попадает.
    #[tokio::test]
    async fn invalid_script_is_rejected() {
        let env = Env::new();
        let scope = env.account("user@example.org").await;
        let path = format!("{}/scripts/a", base(scope.account));
        let bad = Some(json!({"text": "keep;\n  oops;\n"}));
        let (s, h, b) = env.call("PUT", &path, Some("org"), bad.clone()).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(h["content-type"], "application/problem+json");
        assert_eq!(b["status"], 422);
        assert_eq!(
            b["errors"],
            json!([{"line": 2, "column": 3, "message": "неизвестная команда"}])
        );
        assert_eq!(env.sieve.list(scope).await, Ok(vec![]));

        let org = format!("/api/v1/orgs/{}/sieve", TenantId::DEFAULT);
        let (s, _, _) = env.call("PUT", &org, Some("org"), bad).await;
        assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(env.sieve.org_script(TenantId::DEFAULT).await, Ok(None));

        // Лимит размера — до разбора; недопустимое имя и лишние поля — отказ.
        let max = env.sieve.limits().max_script_size;
        let big = Some(json!({"text": "#".repeat(max + 1)}));
        let (s, _, _) = env.call("PUT", &path, Some("org"), big).await;
        assert_eq!(s, StatusCode::INSUFFICIENT_STORAGE);
        let name = "n".repeat(129);
        let long = format!("{}/scripts/{name}", base(scope.account));
        let ok = Some(json!({"text": "keep;"}));
        let (s, _, _) = env.call("PUT", &long, Some("org"), ok).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let extra = Some(json!({"text": "keep;", "active": true}));
        let (s, _, _) = env.call("PUT", &path, Some("org"), extra).await;
        assert!(s.is_client_error(), "{s}");
        assert_eq!(env.sieve.list(scope).await, Ok(vec![]));
    }

    /// Приёмка ADM-14: автоответ за пользователя.
    #[tokio::test]
    async fn vacation_roundtrip() {
        let env = Env::new();
        let scope = env.account("user@example.org").await;
        let path = format!(
            "/api/v1/orgs/{}/accounts/{}/vacation",
            TenantId::DEFAULT,
            scope.account
        );
        let (s, _, b) = env.call("GET", &path, Some("operator"), None).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["enabled"], false);
        assert_eq!(b["from"], Value::Null);

        let body = json!({
            "enabled": true,
            "from": "2026-12-28T03:00:00+03:00",
            "to": "2027-01-11T00:00:00Z",
            "subject": "В отпуске",
            "text_body": "Вернусь 11 января.",
        });
        let (s, _, b) = env.call("PUT", &path, Some("org"), Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(b["from"], "2026-12-28T00:00:00Z", "время — UTC");
        let (_, _, got) = env.call("GET", &path, Some("org"), None).await;
        assert_eq!(got, b);
        let stored = env.sieve.vacation(scope).await.expect("vacation");
        assert!(stored.enabled);
        assert_eq!(stored.subject.as_deref(), Some("В отпуске"));
        assert_eq!(stored.html_body, None);

        let bad =
            json!({"enabled": true, "from": "2027-01-11T00:00:00Z", "to": "2026-12-28T00:00:00Z"});
        let (s, _, _) = env.call("PUT", &path, Some("org"), Some(bad)).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        let off = Some(json!({"enabled": false}));
        let (s, _, _) = env.call("PUT", &path, Some("operator"), off.clone()).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, b) = env.call("PUT", &path, Some("org"), off).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b["subject"], Value::Null, "замена целиком");

        let log = env.audit.records();
        assert_eq!(log.len(), 4);
        assert_eq!(log[0].route, super::VACATION);
        assert!(!format!("{log:?}").contains("Вернусь"));
    }

    #[tokio::test]
    async fn org_script_cycle() {
        let env = Env::new();
        let path = format!("/api/v1/orgs/{}/sieve", TenantId::DEFAULT);
        let (s, _, _) = env.call("GET", &path, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let body = Some(json!({ "text": SCRIPT }));
        let (s, _, _) = env.call("PUT", &path, Some("operator"), body.clone()).await;
        assert_eq!(s, StatusCode::FORBIDDEN);
        let (s, _, _) = env.call("PUT", &path, Some("org"), body).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, _, b) = env.call("GET", &path, Some("operator"), None).await;
        assert_eq!((s, b), (StatusCode::OK, json!({ "text": SCRIPT })));
        let other = format!("/api/v1/orgs/{}/sieve", env.other_tenant);
        let (s, _, _) = env.call("GET", &other, Some("org"), None).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "чужая организация");
        let (s, _, _) = env.call("GET", &other, Some("other"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND, "скрипт — в своей организации");
        for _ in 0..2 {
            let (s, _, _) = env.call("DELETE", &path, Some("org"), None).await;
            assert_eq!(s, StatusCode::NO_CONTENT);
        }
        let (s, _, _) = env.call("GET", &path, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        assert_eq!(env.audit.records().len(), 4);
    }

    #[tokio::test]
    async fn missing_account_unavailable_and_openapi() {
        let env = Env::new();
        let missing = AccountId::new();
        let scripts = format!("{}/scripts", base(missing));
        let (s, _, _) = env.call("GET", &scripts, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let body = Some(json!({"text": "keep;"}));
        let (s, _, _) = env
            .call("PUT", &format!("{scripts}/a"), Some("org"), body)
            .await;
        assert_eq!(s, StatusCode::NOT_FOUND);
        let vacation = format!(
            "/api/v1/orgs/{}/accounts/{missing}/vacation",
            TenantId::DEFAULT
        );
        let (s, _, _) = env.call("GET", &vacation, Some("org"), None).await;
        assert_eq!(s, StatusCode::NOT_FOUND);

        let admin = Admin::new(deps(), config()).expect("admin");
        assert_eq!(sieve(&admin).err().map(|e| e.0), Some(Error::UNAVAILABLE));

        let (s, _, b) = env.call("GET", "/api/openapi.json", None, None).await;
        assert_eq!(s, StatusCode::OK);
        let paths = &b["paths"];
        for (path, methods) in [
            (super::SCRIPTS, &["get"][..]),
            (super::SCRIPT, &["get", "put", "delete"]),
            (super::ACTIVATE, &["post"]),
            (super::DEACTIVATE, &["post"]),
            (super::VACATION, &["get", "put"]),
            (super::ORG_SCRIPT, &["get", "put", "delete"]),
        ] {
            for m in methods {
                assert!(paths[path][m].is_object(), "{m} {path}");
            }
        }
        assert!(
            paths[super::SCRIPT]["put"]["responses"]["422"].is_object(),
            "{paths}"
        );
        assert!(b["components"]["schemas"]["ScriptProblem"].is_object());
        assert!(b["components"]["schemas"]["VacationBody"].is_object());
    }
}
