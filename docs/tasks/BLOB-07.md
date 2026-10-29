# BLOB-07: Verify

lane: blob
deps: [BLOB-06]
class: A
files: [crates/tp-blob/src/verify.rs]
read: [docs/spec/S3.md#3.5]
checks: [nextest]
manual: no

## Сделать
- Сверка в обе стороны: сироты и записи без объекта; отчёт JSON; режим исправления (удалить сирот, пометить повреждённые).

## Приёмка
- Тест на testkit с внесёнными расхождениями.
