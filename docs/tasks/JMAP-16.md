# JMAP-16: статус доставки EmailSubmission

lane: jmap
deps: [CTR-35, JMAP-07]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/decisions/2026-11-04-exchange-parity.md, docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- `deliveryStatus` по получателям из журнала доставки; изменения — через состояние объекта и push.

## Приёмка
- Тесты на in-memory журнале.
