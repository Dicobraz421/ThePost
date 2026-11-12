# CTR-77: сигнал «ящик отключён или срок изменён»

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/tasks/MS-17.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 86: mailstore-10, PR #528. Воркер узнаёт об отключении ящика при сверке раз в час.

## Сделать
- `EventKind::DisabledMailboxChanged { account }` после отключения, восстановления и смены срока; testkit, набор.

## Приёмка
- CI зелёный.
