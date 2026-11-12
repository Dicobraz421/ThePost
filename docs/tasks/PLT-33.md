# PLT-33: обязательные параметры по ролям — edge без базы данных

lane: platform
deps: []
class: A
files: [crates/tp-platform/src/config.rs, crates/tp-platform/src/roles.rs]
read: [docs/decisions/2026-10-30-base-and-modules.md]
checks: [nextest]
manual: no

## Сделать
- Сейчас `REQUIRED` (`database.url`, `blob.url`, `domain`) требуется для любой роли, а пресет edge по решению 2026-10-30
  работает без доступа к БД (DMZ, локальная очередь, реплика получателей). `thepost config check` и запуск узла edge
  отказывают без `database.url`.
- Заменить общий список на обязательные параметры по набору ролей узла: `database.url` и `blob.url` — только ролям,
  которым нужны метаданные или хранилище писем; `domain` — всем. Старый общий `REQUIRED` удалить, потребителей поправить
  в том же PR.
- Сообщение об ошибке называет роль, которой нужен параметр.

## Приёмка
- Тест: конфигурация edge без `database.url` и `blob.url` проходит проверку; mailbox без них — ошибка с именем роли;
  `all` — как прежде.
- Источник: отчёт deploy-37 (#621, #623).
