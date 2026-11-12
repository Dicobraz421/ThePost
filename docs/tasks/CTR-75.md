# CTR-75: `TrashStore::hidden_message` — заголовки скрытого письма

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/tasks/MS-19.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 84: mailstore-10, PR #528. MS-19 сводится к контракту.

## Сделать
- `TrashStore::hidden_message(scope, MessageId)` — метаданные и заголовки скрытого письма без снятия скрытия; testkit,
  набор, CHANGELOG. Реализация — META-74; потребитель — ADM-32.

## Приёмка
- CI зелёный; набор: скрытое письмо читается, остаётся скрытым.
