# CTR-27: MailStore::append — письмо клиента без правил доставки

lane: contracts
deps: [CTR-20]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md, crates/tp-mailstore/src/*]
read: [docs/decisions/2026-11-01-trash-purge.md]
checks: [nextest]
manual: no

## Сделать
- `MailStore::append(scope, mailbox, raw, flags, received_at) -> Delivered`: загрузка BLOB, квота, `MessageStore::insert`; без `DecisionHook::on_delivery` и копий.
- Реализация в tp-mailstore — в том же PR (ветка `x/contracts-15`).

## Приёмка
- Тест: append применяет квоту и не вызывает точки принятия решений доставки.
