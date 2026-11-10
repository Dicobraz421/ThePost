# META-36: `EventBus` для `PgSignalBus`; сигнал `TrashChanged` из PostgreSQL

lane: meta
deps: [META-26, CTR-37]
class: A
files: [crates/tp-meta/src/*]
read: []
checks: [nextest]
manual: no

Отчёты meta-10 (META-26) и contracts-25 (CTR-37). Зависимость `futures` — PR `x/meta-dep` (#231).

## Сделать
- `impl EventBus for PgSignalBus`: `Subscription` → `EventStream` (`futures::stream::unfold`); прогон `suite::signals` при `TEST_DATABASE_URL`.
- `PgMeta::purge` публикует `TrashChanged { account }` после фиксации, если что-то скрыто или стёрто.
