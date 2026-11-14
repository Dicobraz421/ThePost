# APP-73: e2e узла — ответ ресурса внешнему организатору письмом iMIP

lane: app
deps: [APP-71]
class: A
files: [crates/thepost/tests/*]
read: [docs/tasks/APP-71.md, docs/tasks/DAV-19.md]
checks: [nextest]
manual: no

## Сделать
- После #764 не проверено: приглашение ресурса (переговорной), присланное письмом iMIP с внешнего адреса, принимается
  автоматически, а `REPLY` уходит внешнему организатору письмом iMIP через `ImipSender` узла.
- e2e узла: входящее письмо `REQUEST` в ящик организации → бронь `ACCEPTED` в календаре ресурса → в исходящей очереди
  письмо `REPLY` с `text/calendar; method=REPLY` внешнему организатору; занятое время — `DECLINED`.

## Приёмка
- Тест e2e проходит в CI; тело `REPLY` разбирается как iTIP, `ATTENDEE` — ресурс со статусом `ACCEPTED`.
