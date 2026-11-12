# APP-61: узел edge запускается без базы данных и хранилища писем

lane: app
deps: []
class: A
files: [crates/thepost/src/serve.rs, crates/thepost/src/services.rs]
read: [docs/decisions/2026-10-30-base-and-modules.md]
checks: [nextest]
manual: no

## Сделать
- После PLT-33 (#632) конфигурация edge без `database.url` и `blob.url` проходит проверку, но `serve.rs` подключает
  PostgreSQL и BLOB при любых ролях — узел edge падает при запуске.
- Подключать слой данных только ролям, которым он нужен (`roles::uses_data_layer`); edge — локальная очередь и реплика
  получателей (решение 2026-10-30, SRS 2.6).

## Приёмка
- Тест: узел с ролью smtp-in в режиме edge стартует без базы и хранилища; узел mailbox без них — ошибка конфигурации.
- Источник: отчёт platform-22 (#632).
