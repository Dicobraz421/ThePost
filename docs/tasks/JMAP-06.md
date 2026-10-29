# JMAP-06: Бинарные данные

lane: jmap
deps: [JMAP-01, MS-03]
class: A
files: [crates/tp-jmap/src/blob.rs]
read: [docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- Upload/download, Blob/copy; лимиты размера.

## Приёмка
- Тест загрузки и скачивания.
