# DOC-07: ожидания APP-28 и CMP-10 по `inbox/`/`outbox/` и ленте уведомлений; значение по умолчанию в INSTALL

lane: docs
deps: [DAV-36]
class: A
files: [docs/manual-tests/APP-28.md, docs/manual-tests/CMP-10.md, docs/INSTALL.md]
read: [docs/tasks/DAV-36.md]
checks: [none]
manual: no

Приёмка CMP-10, ошибки 3 и 4.

## Сделать
- `APP-28.md` п. 2 и `CMP-10.md` п. 1.2: `inbox/` и `outbox/` не перечисляются в `Depth: 1` дома календарей.
  Клиенты находят их по `schedule-inbox-URL` и `schedule-outbox-URL` принципала (RFC 6638) — проверять так.
- `CMP-10.md`: примечание к п. 4.4 — после dav-14 (DAV-36) лента `CalendarEventNotification` работает по умолчанию.
- `docs/INSTALL.md`: `dav.schedule_keep_processed` по умолчанию `true` (DAV-36).

## Приёмка
- Ожидания совпадают с поведением tp-dav (`tests/caldav_home.rs`, `schedule_stubs`) и с `DavConfig::default()`.
