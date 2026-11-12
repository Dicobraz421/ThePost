# SMTP-37: журнал доставки: запись о приёме iTIP

lane: smtp
deps: [CTR-66, SMTP-33]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/tasks/CTR-66.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 32: smtp-13, PR #447.

## Сделать
- После `ItipDelivery` — событие `Itip` в журнале доставки (метод, итог), без содержимого.

## Приёмка
- Тест: приём `REQUEST` пишет событие в журнал доставки.
