# CTR-76: политика архива: возраст, квота, выборка «пора в архив»

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/tasks/MS-16.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 85: mailstore-10, PR #528.

## Сделать
- Поля политики архива (возраст переноса, квота архива), выборка писем «пора в архив» с ближайшим сроком, раздельный
  учёт квоты архива; testkit, набор, CHANGELOG. Реализация — META-75, потребитель — MS-16.

## Приёмка
- CI зелёный.
