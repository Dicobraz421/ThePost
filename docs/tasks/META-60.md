# META-60: поля `Collection` — по умолчанию, подписка, оповещения

lane: meta
deps: [CTR-56, META-46]
class: B
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-meta/tests/*]
read: [docs/CONTRACTS.md#Dav]
checks: [nextest]
manual: no

Хвост волны jmap-17, PR #444.

## Сделать
- Миграция: столбцы полей CTR-56 в таблице коллекций; единственность коллекции по умолчанию на учётную запись и вид —
  ограничением СУБД; смена — одной транзакцией с `ModSeq` и событием `DavChanged`.

## Приёмка
- Общий набор тестов CTR-56 проходит на PostgreSQL; миграция применяется к базе с существующими коллекциями.
