# META-67: публикация `AccountDisabled` при блокировке

lane: meta
deps: [CTR-64]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/tasks/CTR-64.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 51: admin-13, PR #487.

## Сделать
- `AccountStore::upsert` (и синхронизация каталога): переход `enabled` в `false` — `AccountDisabled` после фиксации.

## Приёмка
- Тест PostgreSQL: блокировка публикует сигнал, повторное сохранение без изменения — нет.
