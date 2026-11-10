# DEP-11: убрать Valkey из стенда разработки

lane: deploy
deps: [CLU-09]
class: A
files: [deploy/*]
read: []
checks: []
manual: no

Отчёт cluster-06 (CLU-09): шина Valkey удалена из кода; `deploy/compose/dev.yml` и `deploy/README.md` ещё поднимают
Valkey (`valkey-tls`, `TEST_VALKEY_URL`, `TP_VALKEY_PORT`).

## Сделать
- Убрать сервисы Valkey, переменные и строки README; проверить `docker compose config` профилей.
