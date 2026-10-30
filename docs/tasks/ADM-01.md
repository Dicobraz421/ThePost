# ADM-01: Каркас Admin API

lane: admin
deps: [AUTH-03]
class: A
files: [crates/tp-admin/src/server.rs, crates/tp-admin/src/audit.rs]
read: [docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

## Сделать
- axum + utoipa (OpenAPI /api/openapi.json), OIDC, роли администраторов с областью (инсталляция или организация), ресурсы в контексте организации,
- реестр маршрутов модулей, журнал аудита с tenant.

## Приёмка
- Тест: изменение без роли → 403, с ролью → запись в аудит.
