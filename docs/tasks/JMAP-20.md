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

## Формат (клиент Postman уже такой, S8-45)
- `CalendarPublication/get` — стандартный `/get` (`state`, `list`): `{ id, calendarId, detail, expires }`, `detail`: `freeBusy` или
  `details`, `expires` — UTCDate или `null`. Адреса в списке нет: хранится только хэш токена (META-34).
- `CalendarPublication/set` — стандартный `/set`: `create: { new: { calendarId, detail, expires? } }` → в `created.new` есть
  `id` и `url`, адрес отдаётся только здесь; `destroy: [id]` — отзыв; сверх политики — `notCreated`.

## Приёмка
- Тесты: создание, отзыв, отказ сверх политики.
