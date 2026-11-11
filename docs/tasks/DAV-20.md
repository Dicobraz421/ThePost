# DAV-20: публикация календаря по ссылке ICS

lane: dav
deps: [DAV-17, CTR-36, META-34]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/decisions/2026-11-04-calendar-external-api.md]
checks: [nextest]
manual: no

## Сделать
- `GET /dav/pub/<токен>.ics` без входа: токен ищется по хэшу (`CalendarPublications`), уровень детализации — «занят/свободен»,
  «тема и время», «полностью»; отозванная или просроченная ссылка — 404; `ETag` и `Cache-Control`.
- Ограничение частоты запросов по токену; выдача и отзыв — в журнале аудита (выдача — JMAP-20).

## Приёмка
- Тесты: три уровня детализации на одном событии, отзыв, срок, неверный токен.
