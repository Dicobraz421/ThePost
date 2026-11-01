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
