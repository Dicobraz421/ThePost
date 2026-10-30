# KIT-06: MemSecretStore и MemMembership

lane: testkit
deps: [CTR-14, CTR-15]
class: A
files: [crates/tp-testkit/src/*]
read: [docs/CONTRACTS.md#Secrets, docs/CONTRACTS.md#Events]
checks: [nextest]
manual: no

## Сделать
- `MemSecretStore`: секреты из кода теста, отзыв и истечение аренды, инъекция `Unavailable`.
- `MemMembership` с `ManualClock`: продление, истечение, выход узла.

## Приёмка
- Общий набор тестов контрактов `SecretStore` и `Membership` проходит на in-memory реализациях.
