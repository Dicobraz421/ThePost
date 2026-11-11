# DAV-16: занятость — VFREEBUSY и реализация FreeBusy

lane: dav
deps: [DAV-13]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `POST` VFREEBUSY в `schedule-outbox` (RFC 6638 §5): ответ `schedule-response` по внутренним участникам; внешние — `3.7` (нет данных).
- `impl FreeBusy` поверх индекса занятости (`busy` в `ObjectIndex`, CTR-47): `TRANSP:TRANSPARENT` и `STATUS:CANCELLED` не занимают,
  `TENTATIVE` — отдельно.

## Приёмка
- Тесты: занятость двух пользователей с повторами и исключениями; Календарь macOS — «Доступность» показывает занятость (снимок запроса).
