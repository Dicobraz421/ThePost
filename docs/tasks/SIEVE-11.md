# SIEVE-11: ManageSieve — проверка скрипта с пределами секции `sieve`

lane: sieve
deps: [SIEVE-09, SIEVE-07]
class: A
files: [crates/tp-sieve/src/*]
read: [docs/spec/S4.md#4.3]
checks: [nextest]
manual: no

Хвост волны app-25, PR #442.

Требует подтверждения владельца: проверять скрипты в ManageSieve с пределами секции `sieve` (рекомендация агента)? (PR #442, решение 2)

## Сделать
- `managesieve`: `PUTSCRIPT` и `CHECKSCRIPT` проверяют текст с пределами действующей секции `sieve` узла — теми же,
  что у доставки (`Rules`) и Admin API, а не `SieveConfig::default()`; `HAVESPACE` и коды `QUOTA/*` — по ним же.
- Точка получения секции — та же, что у `Rules`; если нужен новый аргумент `managesieve::serve` — передаёт узел (APP-27).

## Приёмка
- Тест на `MemSieve`: при уменьшенном пределе секции `CHECKSCRIPT` и `PUTSCRIPT` отвечают `NO`, тот же текст при
  пределах по умолчанию принимается.
