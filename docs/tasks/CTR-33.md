# CTR-33: перенос в корзину снимает \Deleted

lane: contracts
deps: [CTR-26]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md, crates/tp-meta/src/*, crates/tp-testkit/src/*]
read: [docs/decisions/2026-11-01-trash-purge.md]
checks: [nextest]
manual: no

## Сделать
- `to_trash`/`delete_to_trash` оставляют флаг `\Deleted` (отчёт imap-05, PR #181): письмо, удалённое из INBOX через EXPUNGE, первым же EXPUNGE в корзине стирается/скрывается. Перенос в корзину снимает `\Deleted`; в контракте — явно; реализации — в том же PR (ветка `x/contracts-21`).

## Приёмка
- Тест: EXPUNGE в INBOX → письмо в корзине без `\Deleted`; EXPUNGE в корзине его не трогает.
