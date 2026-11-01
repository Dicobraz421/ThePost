# CTR-21: контракты: сжатие BLOB, журнал доставки, уровни нагрузки, группы рассылки

lane: contracts
deps: [CTR-20]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S3.md#3.2, docs/spec/S4.md#4.5, docs/spec/S5.md#5.2, docs/spec/S6.md#6.3]
checks: [nextest]
manual: no

## Сделать
- Учёт BLOB: `Compression { None, Zstd }` в записи объекта; хэш — от исходного содержимого.
- `DeliveryLog`: `record(DeliveryEvent)` (отправитель, получатели, Message-ID, событие, решение и причина, узел, время, tenant), `search(DeliveryQuery) -> Page`.
- `LoadLevel { Normal, Elevated, High }` и `LoadMonitor` (текущий уровень, подписка `watch`) в `RoleCtx`.
- `DistributionGroup` (участники, ограничения отправителей, модераторы, скрыта из GAL) и `GroupStore` (раскрытие с защитой от циклов, очередь модерации).
- In-memory реализации — в tp-testkit в том же PR (ветка `x/contracts-10`).

## Приёмка
- Тесты контрактов зелёные; testkit реализует новые трейты.
