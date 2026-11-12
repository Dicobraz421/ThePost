# ADM-27: уникальные `operationId` в схеме Admin API

lane: admin
deps: [ADM-15]
class: A
files: [crates/tp-admin/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Синк 12.11, часть 2, решение 73 (хвост console-01, PR #429: 5 совпадающих пар — дефект схемы).

## Сделать
- Переименовать совпадающие `operationId` (5 пар из PR #429) — имя по действию и ресурсу; тест: все `operationId`
  в `openapi_spec()` уникальны.
- CLI (`OPERATIONS`) и консоль сверяются со схемой — переименования перечислить в PR для UI-13.

## Приёмка
- Тест уникальности `operationId` зелёный; сверка команд CLI со схемой проходит.
