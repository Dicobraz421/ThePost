Начальный корпус фаззинга `tp-sieve` (CI-09): скрипты из теста `fuzz_examples` (`crates/tp-sieve/src/tests/rules.rs`).
Цель `parse` получает файлы `*.sieve` как есть; для цели `run` workflow `fuzz.yml` склеивает каждый скрипт
с письмами `crates/tp-testkit/corpus/*.eml` через байт NUL.
