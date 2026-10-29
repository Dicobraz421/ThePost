# SRCH-04: Анализаторы

lane: search
deps: [SRCH-01]
class: A
files: [crates/tp-search/src/analyzers.rs]
read: [docs/spec/S4.md#4.6]
checks: [nextest]
manual: no

## Сделать
- Токенизация, нижний регистр, ё→е, стеммеры Snowball (ru, en), выбор по скрипту текста.

## Приёмка
- Тест: «письмами» находит «письмо».
