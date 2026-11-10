# CTR-38: контракты GC, владельца загрузки и восстановления скрытых писем

lane: contracts
deps: [CTR-23]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md]
checks: [nextest]
manual: no

Отчёты blob-07 (BLOB-14), jmap-06 (JMAP-12), admin-04 (ADM-09).

## Сделать
- `BlobIndex::next_gc_due()` — ближайший `zero_since + window` (MIN), вместо поиска делением пополам в tp-blob.
- Владелец загрузки: «отметить загрузку (tenant, account, key, until)», «проверить владельца», очистка вместе с GC —
  вместо учёта в памяти узла tp-jmap (в кластере и после перезапуска загрузка с другого узла — `notFound`).
- `MailStore::restore` восстанавливает и скрытые письма (`trash.purge: hide`) с уведомлением клиентов (событие в шину),
  чтобы Admin API не шёл в обход через `TrashStore::restore`.
- In-memory реализации и conformance-тесты в testkit; CHANGELOG. Реализации в PG и переход полос — их волны после мержа.
