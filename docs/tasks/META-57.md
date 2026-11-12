# META-57: адреса учётной записи в PostgreSQL

lane: meta
deps: [CTR-53]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/CONTRACTS.md#Metadata, docs/CONTRACTS.md#Directory]
checks: [nextest]
manual: no

Хвост волны dav-02, PR #427.

## Сделать
- `AccountStore::addresses` (CTR-53): основной адрес и псевдонимы учётной записи из таблиц каталога одним запросом в
  контексте организации (`with_tenant`), catch-all не возвращается. Миграция не требуется; если нужен индекс по цели
  псевдонима — отдельным PR класса B.

## Приёмка
- Общий тест набора CTR-53 проходит на PostgreSQL; псевдоним чужой организации не виден (RLS).
