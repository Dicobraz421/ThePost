# SMTP-42: smtp-out без опроса: сигнал очереди и таймер ближайшего срока

lane: smtp
deps: [META-72]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/tasks/CTR-73.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 74: smtp-07, PR #548.

## Сделать
- `SmtpOutService::run`: ожидание сигнала очереди или ближайшего срока `next_due`; опрос раз в 5 с удалить.

## Приёмка
- Тест: письмо уходит сразу после постановки, отложенное — в свой срок, без опроса.
