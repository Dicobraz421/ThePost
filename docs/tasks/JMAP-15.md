# JMAP-15: отложенная отправка в EmailSubmission

lane: jmap
deps: [CTR-35, JMAP-07]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/decisions/2026-11-04-exchange-parity.md, docs/spec/S4.md#4.1]
checks: [nextest]
manual: no

## Сделать
- `sendAt`, `maxDelayedSend` в capability submission, `undoStatus` (`pending` → `canceled`).

## Приёмка
- Тесты: отправка со сроком, отмена, отказ сверх `maxDelayedSend`.
