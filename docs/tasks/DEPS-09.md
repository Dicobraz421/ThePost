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
  частей сквозные, вложенные письма — `MessageRef`, списки тел RFC 8621, `AddressList`; одной волной класса B: манифесты, код всех
  потребителей, снимки BODYSTRUCTURE и JMAP и удаление 0.11 — в одном PR (ветка `x/…`). Две версии рядом под другим
  именем (`mail-parser-1`, закрытый #522) — нельзя (решение 93). `tp-dav` и `calcard` остаются на 0.11 до обновления
  `calcard` (хвост реестра, п. 91).

## Приёмка
- Снимки BODYSTRUCTURE IMAP и ответов JMAP на корпусе testkit — без изменений или с объяснёнными отличиями; CI зелёный.
