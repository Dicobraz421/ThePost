# DAV-36: `dav.schedule_keep_processed` по умолчанию включён

lane: dav
deps: [DAV-32, APP-53]
class: A
files: [crates/tp-dav/src/*]
read: [docs/tasks/DAV-32.md]
checks: [nextest]
manual: no

Приёмка CMP-10, ошибка 3 (шаг 4.4): в профиле SMB лента `CalendarEventNotification` пуста, потому что применённые
сервером сообщения iTIP хранятся только при `dav.schedule_keep_processed: true`, а по умолчанию было `false`.
Решение владельца от 14.11: «Включить по умолчанию».

## Сделать
- `DavConfig::default()`: `schedule_keep_processed: true`. Хранение ограничено `schedule_processed_ttl` (30 суток),
  очистку делает роль maintenance (APP-53). Во входящих CalDAV применённые сообщения по-прежнему не видны (решение 92).
- Значение по умолчанию в `docs/INSTALL.md` — полоса docs (DOC-07), в этой волне не трогать.

## Приёмка
- Тест: без настройки применённое приглашение остаётся во входящих для ленты и не видно в CalDAV.
