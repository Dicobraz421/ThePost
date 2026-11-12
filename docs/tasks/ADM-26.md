# ADM-26: `GET /api/v1/orgs` — список организаций

lane: admin
deps: [ADM-15]
class: A
files: [crates/tp-admin/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

Синк 12.11, часть 2, решение 73 (хвост console-01, PR #429, решение 3; п. 26 синка).

## Сделать
- `GET /api/v1/orgs`: организации, доступные вошедшему администратору (по областям его ролей), — массив
  `Organization`; без модуля «Мультитенантность» — одна `TenantId::DEFAULT` (решение 10).
- Уникальный `operationId`, операция в `openapi_spec()`.

## Приёмка
- Тесты роутера: администратор инсталляции видит организацию по умолчанию; без прав — 403.
