# JMAP-27: JMAP Calendars — Calendar

lane: jmap
deps: [CTR-45, DAV-03, JMAP-25]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md]
checks: [nextest]
manual: no

По текущей редакции JMAP Calendars IETF (SRS 4.4: «по мере стабилизации»); отличия редакции — в комментарии модуля.

## Сделать
- Capability `urn:ietf:params:jmap:calendars`: `Calendar/get`, `/changes`, `/set` (имя, цвет, порядок, `isSubscribed`, `isDefault`,
  `includeInAvailability`) поверх `DavStore` (вид `Calendar`); типы push `Calendar`, `CalendarEvent`.

## Приёмка
- Тесты на in-memory: создание, изменение, удаление, `/changes`; календарь из CalDAV виден в JMAP.
