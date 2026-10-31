# JMAP-10: правила потока в EmailSubmission

lane: jmap
deps: [JMAP-07, CTR-23]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/decisions/2026-11-01-transport-scope-external-mta.md, docs/spec/S4.md#4.1, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `EmailSubmission/set`: `MailFlow::apply` до постановки в очередь; `Reject` — ошибка `forbiddenToSend` с текстом правила.

## Приёмка
- Тест на заглушке `MailFlow`: отказ и изменённое тело.
