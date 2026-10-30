# META-16: членство узлов в PostgreSQL, разделы журнала по назначению

lane: meta
deps: [CTR-14, META-06]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*]
read: [docs/decisions/2026-10-30-config-events-secrets.md#Разделы журнала событий между экземплярами (#45, вариант 3), docs/CONTRACTS.md#Events]
checks: [nextest]
manual: no

## Сделать
- `Membership for PgMeta`: таблица `nodes` (узел, роли, `expires_at` по часам СУБД), продление и выход; срок — в `EventsConfig`.
- `EventConsumer` берёт лизы только назначенных ему разделов (`PartitionAssigner`), освобождает лизы разделов,
  снятых с него, после фиксации текущей пачки.

## Приёмка
- Тест на PostgreSQL: два экземпляра с назначением пополам обрабатывают каждый свои разделы; после выхода одного
  второй получает все разделы, события не теряются и не фиксируются дважды.
