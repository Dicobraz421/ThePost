# ADM-25: `GET /api/v1/session` — настройки входа OIDC для консоли

lane: admin
deps: [ADM-15]
class: A
files: [crates/tp-admin/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

Синк 12.11, часть 2, решение 73 (хвост console-01, PR #429, решение 2; п. 26 синка).

## Сделать
- `GET /api/v1/session` без входа: `{"oidc": {"issuer", "client_id", "scope"?}}` из настроек OIDC Admin API (форма,
  принятая консолью в #429); OIDC не настроен — `{"oidc": null}`. Секретов в ответе нет.
- Уникальный `operationId`, операция в `openapi_spec()`.

## Приёмка
- Тесты роутера: с настройкой OIDC и без; ответ без заголовка `Authorization`.
