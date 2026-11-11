# META-50: журнал автоответов

lane: meta
deps: [META-49]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-meta/tests/*]
read: [docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `impl VacationLog for PgMeta`: (учётная запись, отправитель, handle) → время ответа; `seen` — одним `INSERT … ON CONFLICT` с проверкой периода.
- Очистка записей старше наибольшего периода (`meta.sieve.vacation_log_ttl`) — задание maintenance.

## Приёмка
- Тесты на PostgreSQL: второй ответ в периоде не отправляется, после периода — отправляется; параллельные вызовы — один ответ.
