# JMAP-07: Identity и EmailSubmission

lane: jmap
deps: [JMAP-05, SMTP-06]
class: A
files: [crates/tp-jmap/src/submission.rs]
read: [docs/spec/S4.md#4.1, docs/spec/S4.md#4.5]
checks: [nextest]
manual: no

## Сделать
- Identity/get,set; EmailSubmission/set → очередь; проверка Send As.

## Приёмка
- Тест: отправка попадает в очередь.
