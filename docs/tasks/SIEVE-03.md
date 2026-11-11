# SIEVE-03: интерпретатор — управление, тесты ядра, сравнения

lane: sieve
deps: [SIEVE-02]
class: A
files: [crates/tp-sieve/src/*]
read: [docs/spec/S4.md#4.3]
checks: [nextest]
manual: no

## Сделать
- Выполнение программы над письмом (`mail-parser`): `if`/`elsif`/`else`, `stop`, `keep`, `discard`, implicit keep (RFC 5228 §2.10.2).
- Тесты `address` (`:all`, `:localpart`, `:domain`), `header`, `exists`, `size`, `true`/`false`, `allof`/`anyof`/`not`; заголовки
  декодируются (RFC 2047), адреса разбираются по RFC 5322.
- Компараторы `i;ascii-casemap`, `i;octet`; `:is`, `:contains`, `:matches` (`*`, `?`, экранирование).
- Лимит шагов выполнения (`sieve.max_steps`) — ошибка выполнения и implicit keep.

## Приёмка
- Таблица тестов: письма корпуса testkit × скрипты → ожидаемые действия; превышение шагов — implicit keep.
