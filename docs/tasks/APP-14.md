# APP-14: ACME и переключение СУБД в `serve`

lane: app
deps: [META-25, DEP-05]
class: A
files: [crates/thepost/src/*]
read: []
checks: [nextest]
manual: no

Отчёты deploy-02 (DEP-05) и meta-09 (META-25).

## Сделать
- ACME: `Acme::run` запускается в `serve` при секции `acme`; слушатель HTTP-01 на порту 80 (только `/.well-known/acme-challenge/`, остальное — 404).
  Режим `acme` эталонной установки (deploy/ansible) становится рабочим.
- СУБД: `infra.rs` — `connect_failover` вместо `connect_rotating` (список `database.urls`, поиск primary при переключении);
  `Failover::metrics` — в `/metrics`.
- Тесты: сборка сервисов с секцией `acme` (HTTP-01 отвечает токеном из хранилища вызова); пул на `connect_failover`.
