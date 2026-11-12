# CTR-64: сигнал `AccountDisabled` — вход учётной записи запрещён

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md#EventBus]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 51: admin-13, PR #487. блокировка учётной записи не рвёт открытые сеансы IMAP и JMAP.

## Сделать
- `EventKind::AccountDisabled { account }` — после фиксации `enabled = false` (публикует META-67); подписчики —
  протоколы с долгими сеансами (IMAP-22, JMAP-41). testkit, `suite::signals`, CHANGELOG.

## Приёмка
- CI зелёный; набор сигналов проверяет доставку `AccountDisabled`.
