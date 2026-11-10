# CTR-40: адресация организации в данных; перенос получателей без попытки

lane: contracts
deps: [CTR-24]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md]
checks: [nextest]
manual: no

Отчёт smtp-04 (SMTP-13, SMTP-14).

## Сделать
- Таблица переписывания адресов — в данных организации (каталог/транспорт, правится Admin API), а не в config.yaml
  (`smtp.addressing.rewrite` — по SRS источник — каталог); in-memory и conformance-тест.
- Закрепить в CONTRACTS: catch-all — псевдоним каталога с адресом `@domain` (`DirAlias.address`).
- Очередь: перенос получателей планировщиком без увеличения `attempts` (отдельная операция или флаг `reschedule`).
- CHANGELOG; переход полос smtp и meta — их волны.
