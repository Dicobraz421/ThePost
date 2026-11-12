# ADM-18: изменение и блокировка учётной записи — `PATCH …/accounts/{account}`

lane: admin
deps: []
class: A
files: [crates/tp-admin/src/*]
read: [docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

Хвост волны console-02, PR #439.

Подтверждено владельцем — решение 73 (`docs/decisions/2026-11-11-stage2-followups.md`). Блокировка записи каталога — как ниже (409, если синхронизация перезаписывает значение).

## Сделать
- `PATCH /api/v1/orgs/{org}/accounts/{account}`, тело — `display_name` и `enabled` (оба необязательны, пустое тело —
  422); запись через `AccountStore::upsert`; ответ — `AccountInfo`. Локальная запись — оба поля. Запись каталога
  (`external_id` задан): `display_name` — 409 (ведёт каталог); `enabled` — только если синхронизация каталога значение
  не перезаписывает, иначе 409 с пояснением.
- Запись в журнал аудита; уникальный `operationId`; операция в `openapi_spec()`.
- Экран консоли и команда CLI — отдельными карточками полос console и admin (после CI-07).

## Приёмка
- Тесты роутера: блокировка локальной записи — `enabled: false` в ответе и в `GET`, запись в аудите; `display_name`
  записи каталога — 409; неизвестная учётная запись — 404.
