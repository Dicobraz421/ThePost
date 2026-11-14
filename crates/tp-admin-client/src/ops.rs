//! Операции Admin API клиента: метод и шаблон пути, как в спецификации OpenAPI tp-admin.
//!
//! Каждая операция [`OPERATIONS`] сверяется со спецификацией (`tp_admin::server::openapi_spec`) тестом.

/// Операция Admin API: метод и шаблон пути, как в спецификации OpenAPI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Op {
    /// Метод HTTP.
    pub method: &'static str,
    /// Шаблон пути (`{имя}` — параметр).
    pub path: &'static str,
}

/// Вход администратора по паролю: сессия Admin API.
pub const SESSION: Op = Op {
    method: "POST",
    path: "/api/v1/session",
};
/// Текущий администратор.
pub const WHOAMI: Op = Op {
    method: "GET",
    path: "/api/v1/whoami",
};
/// Поиск учётной записи по имени входа и страница списка.
pub const ACCOUNT_FIND: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/accounts",
};
/// Новая учётная запись.
pub const ACCOUNT_CREATE: Op = Op {
    method: "POST",
    path: "/api/v1/orgs/{org}/accounts",
};
/// Учётная запись по идентификатору.
pub const ACCOUNT_GET: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/accounts/{account}",
};
/// Пароли приложений учётной записи.
/// Изменение учётной записи: отображаемое имя, блокировка входа.
pub const ACCOUNT_PATCH: Op = Op {
    method: "PATCH",
    path: "/api/v1/orgs/{org}/accounts/{account}",
};
pub const APP_PASSWORD_LIST: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/accounts/{account}/app-passwords",
};
/// Выпуск пароля приложения.
pub const APP_PASSWORD_CREATE: Op = Op {
    method: "POST",
    path: "/api/v1/orgs/{org}/accounts/{account}/app-passwords",
};
/// Отзыв пароля приложения.
pub const APP_PASSWORD_REVOKE: Op = Op {
    method: "DELETE",
    path: "/api/v1/orgs/{org}/accounts/{account}/app-passwords/{id}",
};
/// Принятые домены организации.
pub const DOMAIN_LIST: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/transport/domains",
};
/// Новый принятый домен.
pub const DOMAIN_CREATE: Op = Op {
    method: "POST",
    path: "/api/v1/orgs/{org}/transport/domains",
};
/// Удаление принятого домена.
pub const DOMAIN_DELETE: Op = Op {
    method: "DELETE",
    path: "/api/v1/orgs/{org}/transport/domains/{id}",
};
/// Скрипты Sieve учётной записи.
pub const SIEVE_LIST: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts",
};
/// Скрипт с текстом.
pub const SIEVE_GET: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts/{name}",
};
/// Создание или замена текста скрипта.
pub const SIEVE_PUT: Op = Op {
    method: "PUT",
    path: "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts/{name}",
};
/// Включение скрипта.
pub const SIEVE_ACTIVATE: Op = Op {
    method: "POST",
    path: "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts/{name}/activate",
};
/// Отключение правил учётной записи.
pub const SIEVE_DEACTIVATE: Op = Op {
    method: "POST",
    path: "/api/v1/orgs/{org}/accounts/{account}/sieve/deactivate",
};
/// Удаление скрипта (активный — 409).
pub const SIEVE_DELETE: Op = Op {
    method: "DELETE",
    path: "/api/v1/orgs/{org}/accounts/{account}/sieve/scripts/{name}",
};
/// Автоответ учётной записи.
pub const VACATION_GET: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/accounts/{account}/vacation",
};
/// Замена автоответа учётной записи целиком.
pub const VACATION_PUT: Op = Op {
    method: "PUT",
    path: "/api/v1/orgs/{org}/accounts/{account}/vacation",
};
/// Скрипт организации.
pub const ORG_SIEVE_GET: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/sieve",
};
/// Запись скрипта организации.
pub const ORG_SIEVE_PUT: Op = Op {
    method: "PUT",
    path: "/api/v1/orgs/{org}/sieve",
};
/// Удаление скрипта организации.
pub const ORG_SIEVE_DELETE: Op = Op {
    method: "DELETE",
    path: "/api/v1/orgs/{org}/sieve",
};

/// Организация и её политика.
pub const ORG_GET: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}",
};
/// Политика организации целиком.
pub const ORG_POLICY_PUT: Op = Op {
    method: "PUT",
    path: "/api/v1/orgs/{org}/policy",
};
/// Политика домена и действующая политика.
pub const DOMAIN_POLICY_GET: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/transport/domains/{id}/policy",
};
/// Политика домена целиком.
pub const DOMAIN_POLICY_PUT: Op = Op {
    method: "PUT",
    path: "/api/v1/orgs/{org}/transport/domains/{id}/policy",
};
/// Политика ящика целиком.
pub const ACCOUNT_POLICY_PUT: Op = Op {
    method: "PUT",
    path: "/api/v1/orgs/{org}/accounts/{account}/policy",
};
/// Группы организации.
pub const GROUP_LIST: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/groups",
};
/// Новая группа.
pub const GROUP_CREATE: Op = Op {
    method: "POST",
    path: "/api/v1/orgs/{org}/groups",
};
/// Группа.
pub const GROUP_GET: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/groups/{group}",
};
/// Изменение группы.
pub const GROUP_PATCH: Op = Op {
    method: "PATCH",
    path: "/api/v1/orgs/{org}/groups/{group}",
};
/// Удаление группы.
pub const GROUP_DELETE: Op = Op {
    method: "DELETE",
    path: "/api/v1/orgs/{org}/groups/{group}",
};
/// Корзина ящика.
pub const TRASH_LIST: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/accounts/{account}/trash",
};
/// Восстановление письма из корзины.
pub const TRASH_RESTORE: Op = Op {
    method: "POST",
    path: "/api/v1/orgs/{org}/accounts/{account}/trash/{message}/restore",
};
/// Скрытые письма корзины.
pub const TRASH_HIDDEN_LIST: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/accounts/{account}/trash/hidden",
};
/// Восстановление скрытого письма.
pub const TRASH_HIDDEN_RESTORE: Op = Op {
    method: "POST",
    path: "/api/v1/orgs/{org}/accounts/{account}/trash/hidden/{message}/restore",
};
/// Журнал аудита организации.
pub const AUDIT_SEARCH: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/audit",
};
/// Поиск в журнале доставки.
pub const DELIVERY_SEARCH: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/delivery",
};
/// Доверенные источники организации.
pub const SOURCE_LIST: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/transport/sources",
};
/// Создание или замена доверенного источника.
pub const SOURCE_PUT: Op = Op {
    method: "PUT",
    path: "/api/v1/orgs/{org}/transport/sources/{name}",
};
/// Удаление доверенного источника.
pub const SOURCE_DELETE: Op = Op {
    method: "DELETE",
    path: "/api/v1/orgs/{org}/transport/sources/{name}",
};
/// Политики доменов назначения.
pub const ROUTE_LIST: Op = Op {
    method: "GET",
    path: "/api/v1/orgs/{org}/transport/routes",
};
/// Создание или замена политики домена назначения.
pub const ROUTE_PUT: Op = Op {
    method: "PUT",
    path: "/api/v1/orgs/{org}/transport/routes/{pattern}",
};
/// Удаление политики домена назначения.
pub const ROUTE_DELETE: Op = Op {
    method: "DELETE",
    path: "/api/v1/orgs/{org}/transport/routes/{pattern}",
};

/// Все операции клиента (сверка со спецификацией OpenAPI — тест).
pub const OPERATIONS: &[Op] = &[
    SESSION,
    WHOAMI,
    ACCOUNT_FIND,
    ACCOUNT_CREATE,
    ACCOUNT_GET,
    ACCOUNT_PATCH,
    APP_PASSWORD_LIST,
    APP_PASSWORD_CREATE,
    APP_PASSWORD_REVOKE,
    DOMAIN_LIST,
    DOMAIN_CREATE,
    DOMAIN_DELETE,
    SIEVE_LIST,
    SIEVE_GET,
    SIEVE_PUT,
    SIEVE_ACTIVATE,
    SIEVE_DEACTIVATE,
    SIEVE_DELETE,
    VACATION_GET,
    VACATION_PUT,
    ORG_SIEVE_GET,
    ORG_SIEVE_PUT,
    ORG_SIEVE_DELETE,
    ORG_GET,
    ORG_POLICY_PUT,
    DOMAIN_POLICY_GET,
    DOMAIN_POLICY_PUT,
    ACCOUNT_POLICY_PUT,
    GROUP_LIST,
    GROUP_CREATE,
    GROUP_GET,
    GROUP_PATCH,
    GROUP_DELETE,
    TRASH_LIST,
    TRASH_RESTORE,
    TRASH_HIDDEN_LIST,
    TRASH_HIDDEN_RESTORE,
    AUDIT_SEARCH,
    DELIVERY_SEARCH,
    SOURCE_LIST,
    SOURCE_PUT,
    SOURCE_DELETE,
    ROUTE_LIST,
    ROUTE_PUT,
    ROUTE_DELETE,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Каждая операция клиента есть в спецификации OpenAPI tp-admin (метод и шаблон пути): расхождение — ошибка.
    #[test]
    fn operations_match_openapi() {
        let spec = serde_json::to_value(tp_admin::server::openapi_spec()).expect("spec");
        let paths = spec["paths"].as_object().expect("paths");
        for op in OPERATIONS {
            let item = paths
                .get(op.path)
                .unwrap_or_else(|| panic!("{}: нет в спецификации Admin API", op.path));
            assert!(
                item.get(op.method.to_ascii_lowercase()).is_some(),
                "{} {}: нет в спецификации Admin API",
                op.method,
                op.path
            );
        }
    }
}
