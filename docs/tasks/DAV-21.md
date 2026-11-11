# DAV-21: ссылка занятости FBURL

lane: dav
deps: [DAV-20, DAV-16]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4, docs/decisions/2026-11-04-calendar-external-api.md]
checks: [nextest]
manual: no

## Сделать
- `GET /dav/freebusy/<токен>.vfb` — VFREEBUSY пользователя на интервал (параметры `start`, `end`, предел — политика), без входа.
- `FBURL` в vCard глобальной адресной книги и `calFBURL` в каталоге — для Internet Free/Busy Outlook и внешних серверов (SRS 4.4).

## Приёмка
- Тесты: интервал, предел политики, отозванная ссылка; `FBURL` в карточке GAL.
