# ADM-38: CLI: политики, группы, корзина, аудит, журнал доставки, транспорт

lane: admin
deps: [APP-22]
class: A
files: [crates/thepost/src/admin_client.rs]
read: [docs/tasks/APP-43.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 66: app-39, PR #530. Часть APP-43 в `admin_client.rs` — полоса admin (решение 87).

## Сделать
- Команды `policy`, `group`, `trash`, `audit`, `delivery`, `transport source|route` по шаблону CLI (APP-43); домены —
  только `transport domain` (решение 94).

## Приёмка
- `operations_match_openapi` покрывает новые команды; прогон каждой против роутера tp-admin.
