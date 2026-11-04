# JMAP-17: расширения: списки отправителей и оргструктура

lane: jmap
deps: [CTR-35, JMAP-01]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/decisions/2026-11-04-exchange-parity.md, docs/spec/S4.md#4.1, docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

## Сделать
- Capability `urn:thepost:senderlists` (get/set) и `urn:thepost:orgchart` (дерево подразделений с членами и старшинством).

## Приёмка
- Тесты методов.
