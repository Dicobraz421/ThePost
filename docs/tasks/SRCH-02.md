# SRCH-02: Индексатор

lane: search
deps: [SRCH-01, KIT-03]
class: A
files: [crates/tp-search/src/indexer.rs]
read: [docs/spec/S4.md#4.6, docs/CONTRACTS.md#Events]
checks: [nextest]
manual: no

## Сделать
- Роль indexer: подписчик журнала событий (EventSink), лиза на раздел, пакетный коммит по времени/числу, удаление из индекса по событию окончательного удаления.

## Приёмка
- Тест: задержка индексации в пределах интервала коммита.
