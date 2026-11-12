# CTR-62: `AccountStore::by_address` с псевдонимами и адресами групп

lane: contracts
deps: [CTR-53]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md#Metadata, docs/tasks/CTR-53.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 46: dav-04, PR #485. участник встречи на псевдоним или адрес группы получает письмо вместо записи в
календарь.

## Сделать
- `AccountStore::by_address(tenant, address) -> Option<AddressTarget>`: учётная запись по основному адресу или
  псевдониму, группа — по адресу группы; по умолчанию — `by_login`. testkit, набор, CHANGELOG. Реализация — META-68.

## Приёмка
- CI зелёный; тест набора: псевдоним → учётная запись, адрес группы → группа.
