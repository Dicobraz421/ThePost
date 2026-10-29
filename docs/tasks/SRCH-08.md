# SRCH-08: Перестройка индекса ящика

lane: search
deps: [SRCH-02]
class: A
files: [crates/tp-search/src/rebuild.rs]
read: [docs/spec/S4.md#4.6]
checks: [nextest]
manual: no

## Сделать
- Задание ReindexMailbox: построение нового индекса рядом и атомарная замена манифеста.

## Приёмка
- Тест: поиск работает во время перестройки.
