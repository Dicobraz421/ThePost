# DEPS-09: mail-parser 1.0 в коде крейтов

lane: deps
deps: []
class: B
files: [Cargo.toml, Cargo.lock, crates/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Хвост волны deps-02 (DEPS-02), PR #369; решение 17: mail-parser 1.0 — этап 3 (новый API, ≈115 мест в 8 крейтах).

## Сделать
- Переход tp-imap, tp-jmap, tp-mailstore, tp-search, tp-testkit, tp-smtp, tp-sieve, thepost на mail-parser 1.0: номера
  частей сквозные, вложенные письма — `MessageRef`, списки тел RFC 8621, `AddressList`; calcard держит 0.11 — две версии
  в дереве допустимы до её обновления. Ветка `x/…`, класс B.

## Приёмка
- Снимки BODYSTRUCTURE IMAP и ответов JMAP на корпусе testkit — без изменений или с объяснёнными отличиями; CI зелёный.
