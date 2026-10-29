# APP-03: Команда verify

lane: app
deps: [APP-01, BLOB-07]
class: A
files: [crates/thepost/src/verify.rs]
read: [docs/spec/S3.md#3.5]
checks: [nextest]
manual: no

## Сделать
- `thepost verify [--fix]` — запуск BLOB-07 с отчётом.

## Приёмка
- Тест на внесённых расхождениях.
