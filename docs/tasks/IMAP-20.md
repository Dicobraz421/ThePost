# IMAP-20: тест: EXPUNGE в корзине не трогает письмо, перенесённое из INBOX с `\Deleted`

lane: imap
deps: []
class: A
files: [crates/tp-imap/src/*]
read: [docs/CONTRACTS.md]
checks: [nextest]
manual: no

Хвост волны contracts-21 (CTR-33), PR #287: IMAP-тест не написан — крейт не входил в `files:` карточки контракта.

## Сделать
- Тест IMAP: письмо с `\Deleted` в INBOX → `EXPUNGE` (перенос в корзину снимает `\Deleted`, CTR-33) → `SELECT Trash`,
  `EXPUNGE` — письмо в корзине остаётся.

## Приёмка
- Тест в `src/tests/write.rs` зелёный.
