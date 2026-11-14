# META-83: хранилище `ShareNotification` в PostgreSQL — миграция 0042

lane: meta
deps: [CTR-86]
class: A
files: [crates/tp-meta/*]
read: [docs/tasks/CTR-88.md, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Миграция **0042** (номер 0041 занят волной meta-54, META-81): таблица уведомлений об общем доступе по
  организации и учётной записи; журнал состояний для `changes`; удаление и срок хранения по контракту CTR-86.
- `ShareNotificationStore` в `PgMeta` по контракту CTR-86 (#765).

## Приёмка
- Набор `tp-testkit` `suite::sharing::share_notifications` проходит на `PgMeta` (PostgreSQL) наравне с памятью;
  тест миграции: таблица создаётся поверх 0041, повторный запуск ничего не меняет.
