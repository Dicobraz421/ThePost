# ADM-14: Admin API — правила Sieve и автоответ

lane: admin
deps: [CTR-46, SIEVE-02]
class: A
files: [crates/tp-admin/src/*]
read: [docs/spec/S5.md#5.2, docs/spec/S4.md#4.3, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `GET/PUT/DELETE /api/v1/orgs/{org}/accounts/{account}/sieve/scripts[/{name}]`, `POST …/activate`; проверка текста — `tp_sieve::check`
  (ошибки с позицией — 422); `GET/PUT …/vacation` (автоответ за пользователя, как в Exchange Admin Center).
- Скрипт организации: `GET/PUT/DELETE /api/v1/orgs/{org}/sieve`.
- Схемы в OpenAPI, запись в журнал аудита; роли: оператор только читает.

## Приёмка
- Тесты на in-memory: цикл скрипта, неверный скрипт — 422, автоответ, аудит; OpenAPI содержит операции.
