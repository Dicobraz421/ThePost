# AUTH-18: LDAP: удалить псевдоним `DisabledRule`

lane: auth
deps: []
class: A
files: [crates/tp-auth/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 14; решение 105.

## Сделать
- `pub type DisabledRule = FlagRule` («прежнее имя») удалить, тесты — на `FlagRule`.

## Приёмка
- `cargo check --workspace` без `DisabledRule`.
