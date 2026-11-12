# APP-47: шарды и реплики СУБД в узле

lane: app
deps: [META-30, META-31, META-32, META-41]
class: A
files: [crates/thepost/src/*]
read: [docs/tasks/META-30.md, docs/tasks/META-31.md, docs/tasks/META-32.md]
checks: [nextest]
manual: no

Хвосты волн meta-13 (PR #393), meta-14 (PR #401), meta-25 (PR #403): шарды и чтение с реплик реализованы в tp-meta, но в
`crates/thepost` не подключены.

## Сделать
- `PgMeta::with_shards` (справочник шардов, `ShardDirectory::migrate` на всех шардах, `follow` по `ConfigReloaded`,
  `Resync`, `ShardsChanged`); журнал событий — `PgShardLogs`, потребители — `EventConsumer::with_shard`; сигналы —
  `ShardedSignalBus` вместо `PgSignalBus`.
- Фоновые роли (maintenance, GC, «сироты», indexer) — `PgMeta::with_intent(ReadIntent::Stale)`; `database.read_replica`,
  `database.replica_max_lag`, `database.direct_urls` — в секции и схеме конфигурации узла.

## Приёмка
- Интеграционный тест на двух шардах (две БД одного PostgreSQL): письмо в ящик второго шарда доставляется, IDLE получает
  сигнал; фоновая роль читает с реплики.
