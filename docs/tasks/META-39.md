# META-39: переписывание адресов, `postpone` и `Unblocked` в PostgreSQL

lane: meta
deps: [CTR-40, CTR-39]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*]
read: []
checks: [nextest]
manual: no

Отчёты contracts-28 (CTR-40) и contracts-27 (CTR-39).

## Сделать
- `TransportStore::put_rewrite`/`delete_rewrite` и `TenantTransport::rewrites` в PostgreSQL (миграция, RLS), сигнал
  `TransportChanged`; набор `suite::transport_rewrites`.
- `QueueStore::postpone` — перенос без увеличения `attempts`; набор `suite::queue_postpone`.
- `PgMeta::unblock` публикует `Unblocked` после фиксации; набор `suite::abuse_blocks_signals` на PG.
