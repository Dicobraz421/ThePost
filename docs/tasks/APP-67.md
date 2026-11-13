# APP-67: воркер отключённых ящиков в роли maintenance, истечение корзины в /metrics

lane: app
deps: [MS-20, APP-52]
class: A
files: [crates/thepost/src/roles.rs, crates/thepost/src/metrics.rs, crates/thepost/src/serve.rs, crates/thepost/tests/disabled_mailboxes.rs]
read: [docs/tasks/APP-52.md]
checks: [nextest]
manual: no

## Сделать
- Хвост MS-20 (#702): `DisabledMailboxes::run` нигде не запускался — отключённые ящики не удалялись по сроку.
  В `crates/thepost/src/roles.rs`: поле `Maintenance::disabled` и `run_disabled` — воркер работает под лизой
  `disabled-mailboxes` (один узел кластера, продление каждую треть срока), потеря лизы его останавливает, остановка
  роли — тоже, после неё лиза отдаётся. В `serve.rs` — `DisabledMailboxes::new(services.mail, meta, meta)`.
- Хвост APP-52: `Store::trash_stats()` (MS-20) — в рядах `tp_maintenance_purge_*` с `step="expire_trash"`;
  `register_purge` принимает снимок истечения корзины, значения читаются на каждый запрос `/metrics`.
- `mailstore.disabled.max_wait` в конфигурациях и примерах не осталось: удалён в MS-20 (#702), правок нет.

## Приёмка
- Тест узла с PostgreSQL: отключённый ящик с истёкшим сроком удаляет роль maintenance, действующий не тронут; лиза —
  у узла, после остановки отдана; ряды `step="expire_trash"` видны в `/metrics`.
- Модульный тест `metrics.rs`: значения `expire_trash` — из снимка `trash_stats`, показатель — один на все шаги.
- Источник: хвост MS-20 (#702) и APP-52.
