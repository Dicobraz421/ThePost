# ADM-17: Sieve-скрипты пользователя в Admin API и CLI

lane: admin
deps: []
class: A
files: [crates/tp-admin/src/*, crates/thepost/src/admin_client.rs]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- Admin API: список скриптов пользователя, чтение, запись (с проверкой tp-sieve и лимитами), включение и
  отключение — через `SieveStore`; каждое действие — в журнал аудита; CLI `thepost admin sieve
  list|get|put|activate|deactivate` (решение 62).

## Приёмка
- Тест роутера: администратор отключает правило пересылки; запись в аудите.
