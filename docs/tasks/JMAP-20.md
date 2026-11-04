# JMAP-20: расширение публикации календарей

lane: jmap
deps: [CTR-36, JMAP-14]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/decisions/2026-11-04-calendar-external-api.md, docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

## Сделать
- Capability `urn:thepost:calendarpublish`: `CalendarPublication/get`, `/set` (создать, отозвать); уровень не выше политики; запись в аудит.

## Приёмка
- Тесты: создание, отзыв, отказ сверх политики.
