# APP-63: пересчёт поисковых строк старых контактов в узле

lane: app
deps: [META-78]
class: A
files: [crates/thepost/src/roles.rs, crates/thepost/src/serve.rs, crates/thepost/tests/*]
read: [docs/tasks/META-78.md]
checks: [nextest]
manual: no

Хвост META-78 (#646): `PgMeta::reindex_cards` реализован в tp-meta, но узел его не вызывает — карточки, записанные
до миграции 0030, не находятся `addressbook-query`, пока клиент их не перезапишет.

## Сделать
- Роль maintenance: при запуске один раз в фоне `meta.reindex_cards(|b| tp_dav::index::card(b).ok().map(|i| i.search))`.
  Старт роли пересчёт не ждёт; ошибка — в журнал, прерванный пересчёт продолжает следующий запуск; остановка роли
  прерывает пересчёт.

## Приёмка
- Тест узла на PostgreSQL: карточка без поисковых строк (`search_pending`) после запуска роли maintenance находится
  `find_cards`.
