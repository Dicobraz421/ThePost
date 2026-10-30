# META-06: Журнал событий ядра

lane: meta
deps: [META-01, META-05, CTR-04]
class: A
files: [crates/tp-meta/src/events.rs]
read: [docs/CONTRACTS.md#Events, docs/spec/S2.md#2.5]
checks: [nextest]
manual: no

## Сделать
- EventLog: append в транзакции вызывающего, раздел = hash(account) mod N; чтение подписчиком после позиции, фиксация курсора;
- лиза на раздел подписчика; очистка событий, прочитанных всеми подписчиками.

## Приёмка
- Тесты: at-least-once, порядок в пределах ящика, два экземпляра подписчика не читают один раздел одновременно.
