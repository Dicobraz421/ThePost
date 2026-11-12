# CTR-78: атомарная отметка «ящик удаляется»

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/tasks/MS-17.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 88: mailstore-10, PR #528. Гонка «восстановление против окончательного удаления».

## Сделать
- `AccountStore`: переход «отключён → удаляется» одной операцией с проверкой состояния; восстановление удаляемого —
  `Conflict`; testkit, набор.

## Приёмка
- CI зелёный; набор: параллельные восстановление и удаление — побеждает одно.
