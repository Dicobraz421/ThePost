# CTR-37: системный сигнал изменения корзины

lane: contracts
deps: [CTR-23]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md]
checks: [nextest]
manual: no

Отчёт mailstore-08 (MS-14): таймер истечения корзины пересчитывается по сигналу `Notify` в памяти; в кластере удаление
на другом узле подхватывается только через `trash.max_wait`.

## Сделать
- `EventKind::TrashChanged { account }` (системный вид, `subscribe_system()`), публикуется после `delete_to_trash`
  и `purge`; in-memory реализация и conformance-тест в testkit; CHANGELOG в CONTRACTS.md.
- Подписка воркера MS-14 — волна mailstore после мержа.
