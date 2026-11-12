# JMAP-38: JMAP Principal: тип `resource` для переговорных и оборудования

lane: jmap
deps: [JMAP-30, CTR-48]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/tasks/JMAP-30.md, docs/tasks/CTR-48.md]
checks: [nextest]
manual: no

Хвост волны jmap-18 (JMAP-30), PR #449, решение 2: у всех `Principal` тип `individual` — вид учётной записи
(`AccountKind::Resource`) появится с CTR-48; фильтр `type: resource` ничего не возвращает.

## Сделать
- `Principal/get`, `/query`: `type` — из вида учётной записи (`resource`, `individual`), фильтр `type`.

## Приёмка
- Тест (приёмка JMAP-30): переговорная находится в `Principal/query` с `type: resource`.
