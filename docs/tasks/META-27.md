# META-27: состав кластера с ролями и блокировки

lane: meta
deps: [CTR-23, META-16]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*]
read: [docs/decisions/2026-11-01-scale-shards-geo-admin.md, docs/decisions/2026-11-01-interaction-db-ha.md, docs/spec/S2.md#2.8, docs/spec/S6.md#6.3, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Записи узлов: роли, адрес служебного канала и площадка (`site`), `live_info()`.
- `AbuseBlocks` в PostgreSQL (по организации, RLS), после записи — сигнал `Blocked`.

## Приёмка
- Тесты миграции и conformance testkit.

## Состояние (meta-10)
- Сделано: миграция `0015_node_info_abuse_blocks` (адрес и площадка в `nodes`, таблица `abuse_blocks` с RLS);
  `Membership::heartbeat(&NodeInfo)` записывает роли, адрес, площадку, `live_info()` — роли ядра (имена ролей модулей
  в `NodeInfo` не попадают); `MembersChanged` — вход, изменение записи, выход живого узла; `AbuseBlocks for PgMeta`
  с сигналом `Blocked` после фиксации и удалением истёкших строк (`meta.abuse.purge_batch`). Тесты — `pg_abuse.rs`,
  `pg_membership.rs` (наборы testkit `membership`, `membership_info`, `abuse_blocks`, миграция с 14-й версии).
- Остаётся вне полосы: пометка «заглушка до META-27» в CONTRACTS.md#Membership (класс B); приложение пока продлевает
  запись через `PgMeta::heartbeat_roles` (адрес и площадку не передаёт).
