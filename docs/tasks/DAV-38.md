# DAV-38: `calendar-user-type` по типу ресурса — `ROOM` или `RESOURCE`

lane: dav
deps: [CTR-88, DAV-19]
class: A
files: [crates/tp-dav/*]
read: [docs/decisions/2026-11-11-stage2-followups.md#121, docs/tasks/CTR-88.md, docs/tasks/DAV-19.md]
checks: [nextest]
manual: no

## Сделать
- `calendar-user-type` принципала ресурса — по `Account::resource_type`: помещение — `ROOM`, оборудование — `RESOURCE`
  (RFC 6638 §2.4.2, `CUTYPE` RFC 5545); у пользователя — `INDIVIDUAL`, как сейчас.
- Поиск принципалов: `principal-property-search` по `calendar-user-type` различает `ROOM` и `RESOURCE`;
  `calendarserver-principal-search` с `context="location"` находит помещения, с `context="resource"` — оборудование.
- Бронирование (DAV-19) — одинаково для обоих типов.
- Решение 121 (а) уже выполнено в DAV-18 (`proxy.rs`: новый представитель с записью — `schedule_on_behalf: true`, у
  прежнего признак сохраняется); если теста на это нет — добавить.

## Приёмка
- Тесты: `PROPFIND` принципала помещения — `ROOM`, оборудования — `RESOURCE`; поиск с `context="location"` и
  `context="resource"`; представитель, назначенный через `PROPPATCH group-member-set` в `calendar-proxy-write`,
  рассылает приглашения с `SENT-BY`.
