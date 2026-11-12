# APP-54: e2e узла: локальный домен — принятый домен в БД

lane: app
deps: []
class: A
files: [crates/thepost/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 4; решение 96.

## Сделать
- `tests/e2e.rs`: вместо `smtp.delivery.local_domains` — принятый домен организации через Admin API или `PgMeta`.

## Приёмка
- e2e зелёный без ключа.
