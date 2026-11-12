# APP-43: CLI `thepost admin`: остальные операции Admin API

lane: app
deps: [APP-22]
class: A
files: [crates/thepost/src/*]
read: [docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

Хвост волны app-07 (APP-11), PR #353: политики, группы, корзина, аудит, журнал доставки, источники и маршруты транспорта в CLI не
вошли (минимум пилота).

## Сделать
- Команды по тому же шаблону (константа `Op` + действие + проверка в `OPERATIONS`, структуры tp-admin — APP-22):
  `policy get|set`, `group list|get|create|delete|members`, `trash list|restore`, `audit search`, `delivery search`,
  `transport source|route|domain list|create|delete`.
- Если CI-07 отнесёт `admin_client.rs` к полосе admin — карточка переходит в полосу admin.

## Приёмка
- Тест `operations_match_openapi` покрывает новые команды; прогон каждой против роутера tp-admin.
