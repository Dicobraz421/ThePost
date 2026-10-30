# BLOB-10: кэш сегментов индексов

lane: blob
deps: [BLOB-04, CTR-12]
class: A
files: [crates/tp-blob/src/*]
read: [docs/decisions/2026-10-30-owner-answers.md#Контракты и данные]
checks: [nextest]
manual: no

## Сделать
- NVMe-кэш хранит и `*_raw` (сегменты неизменяемы — CTR-12).

## Приёмка
- Тест: повторное чтение сегмента идёт из кэша.
