# BLOB-16: метрика «сирот» после цикла GC

lane: blob
deps: []
class: A
files: [crates/tp-blob/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- Счётчики GC: найдено и удалено «сирот», осталось после цикла (решение 57, Д.7 строка 14) — в `/metrics`.

## Приёмка
- Тест: после цикла с обрывом загрузки «сирот» 0, метрика это показывает.
