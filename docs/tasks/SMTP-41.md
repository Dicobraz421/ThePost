# SMTP-41: AUTH SMTP: перебор — только `AbuseGuard`

lane: smtp
deps: []
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/spec/S6.md#6.3]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 7; решение 99.

## Сделать
- `server/auth.rs`: убрать `RateLimiter::check_ip` и блокировку адреса после `limits.max_auth_failures` — блокировки
  ведёт `AbuseGuard` за проверкой учётных данных (`Guarded`); в сеансе остаётся только разрыв соединения после N неудач
  подряд (как у ManageSieve). Ключ, если больше не нужен, удалить из секции.

## Приёмка
- Тесты: неудачные AUTH не создают блокировку в `RateLimiter`; блокировка от `AbuseGuard` даёт отказ входа.
