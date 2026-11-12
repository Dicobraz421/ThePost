# SIEVE-13: одна проверка скрипта: удалить `tp_sieve::check`

lane: sieve
deps: [SIEVE-11, JMAP-35, ADM-35]
class: A
files: [crates/tp-sieve/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Аудит 12.11 «старый путь рядом с новым не оставлять» (решение 93), п. 3; решение 95.

## Сделать
- После SIEVE-11, JMAP-35 и ADM-35 вызовов не остаётся: `pub fn check` (пределы по умолчанию) удалить — проверка
  только `compile` с `Limits` секции `sieve`; документация крейта.

## Приёмка
- `cargo check --workspace` без `tp_sieve::check`; тесты tp-sieve на `compile`.
