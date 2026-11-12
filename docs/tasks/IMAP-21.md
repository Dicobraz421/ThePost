# IMAP-21: фазз-тесты разбора IMAP — постоянное зерно

lane: imap
deps: [IMAP-18]
class: A
files: [crates/tp-imap/src/*]
read: [docs/tasks/IMAP-18.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 38: imap-09, PR #479. `proto::tests::fuzz_never_panics` и `fuzz_mutated_examples` остались на случайном
зерне — случайная находка уронит CI чужой волны.

## Сделать
- Оба теста — с постоянным зерном (как `mutated_examples` в IMAP-18); случайный поиск — ночному `fuzz.yml`.

## Приёмка
- 20 прогонов `cargo nextest run -p tp-imap` подряд дают одинаковый результат.
