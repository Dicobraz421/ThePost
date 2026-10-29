# SRCH-06: Кэш индексов

lane: search
deps: [SRCH-05]
class: A
files: [crates/tp-search/src/cache.rs]
read: [docs/spec/S4.md#4.6]
checks: [nextest]
manual: no

## Сделать
- LRU индексов на NVMe по размеру, холодная загрузка, метрики попаданий.

## Приёмка
- Тест вытеснения.
