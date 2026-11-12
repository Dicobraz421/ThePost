# APP-53: узел: планирование в JMAP и очистка входящих

lane: app
deps: [JMAP-31, DAV-32]
class: A
files: [crates/thepost/src/*]
read: [docs/tasks/APP-28.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 55: jmap-19a, PR #493. `Jmap::with_schedule` в узле не вызывается — приглашения из Postman не уходят.

## Сделать
- `services.rs`: `Jmap::with_schedule` (те же `ItipSender` и `FreeBusy`, что у CalDAV, APP-28); шаг maintenance —
  очистка обработанных сообщений входящих по сроку (DAV-32), если `schedule_keep_processed`.

## Приёмка
- Интеграционный тест узла: событие с участником из JMAP — приглашение во входящих участника.
