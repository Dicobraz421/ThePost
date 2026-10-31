# META-28: хранение настроек транспорта

lane: meta
deps: [CTR-23, META-26]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*]
read: [docs/decisions/2026-11-01-transport-scope-external-mta.md, docs/spec/S4.md#4.1, docs/spec/S2.md#2.8, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- `TransportStore` на PostgreSQL: тип принятого домена — столбец таблицы доменов; источники, политики доменов назначения, правила потока — таблицы с RLS (кроме уровня кластера).
- Сигнал `TransportChanged` — после фиксации записи.

## Приёмка
- Набор `suite::transport_store` проходит на PostgreSQL (при `TEST_DATABASE_URL`).
