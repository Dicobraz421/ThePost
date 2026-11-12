# META-70: `PolicyStore` и `AuditLog` в PostgreSQL

lane: meta
deps: [CTR-25]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/tasks/CTR-25.md]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 6 и 12; решение 98, 104.

## Сделать
- `impl PolicyStore for PgMeta` поверх готовых `policy`/`set_policy`; `impl AuditLog` поверх `audit_log` (только
  добавление, META-22): запись, чтение по организации, интервалу, курсору.

## Приёмка
- Наборы CTR-25 на PostgreSQL.
