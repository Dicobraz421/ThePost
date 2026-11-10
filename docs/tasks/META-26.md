# META-26: сигналы через LISTEN/NOTIFY

lane: meta
deps: [CTR-23, META-25]
class: A
files: [crates/tp-meta/src/*]
read: [docs/decisions/2026-11-01-interaction-db-ha.md, docs/spec/S2.md#2.8, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `PgSignalBus: EventBus`: публикатор узла — отдельная короткая транзакция после фиксации, склейка пачек без таймера.
- Слушатель — одно прямое соединение с primary (мимо PgBouncer), переподключение через META-25, после него — `Resync` подписчикам.
- Фильтрация по интересам на узле; ограниченный буфер подписчика, при переполнении — `Resync`.
- Сигналы `LogAppended` после фиксации записей журнала событий, `MembersChanged` при входе и выходе узла.
- Метрики: задержка сигнала, размер пачек, время NOTIFY.

## Приёмка
- Conformance-тесты `EventBus` (как у шины в процессе) при `TEST_DATABASE_URL`; тест склейки под нагрузкой.

## Состояние (meta-10)
- Сделано: `tp_meta::signals::PgSignalBus` (публикатор со склейкой, слушатель мимо PgBouncer — `connect_listener`,
  переподключение через `Failover` с `Resync`, фильтрация по интересам, буфер подписчика с `Resync`, метрики
  `SignalMetrics`); `PgMeta::with_bus` — `LogAppended` после фиксации событий журнала, `MembersChanged` при входе и
  выходе узла. Тесты — `tests/pg_signals.rs` (при `TEST_DATABASE_URL`).
- Не сделано: `impl EventBus for PgSignalBus` и прогон `tp_testkit::suite::signals` — у `tp-meta` нет зависимости
  `futures` (поток `EventStream` не собрать). Нужен PR `x/meta-dep` (`futures.workspace = true` в `tp-meta`), затем —
  обёртка `Subscription` в `futures::stream::unfold` (несколько строк) и conformance-тест набором testkit.
