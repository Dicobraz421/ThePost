# CTR-58: признак «скрыта из GAL» у учётной записи каталога

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md#Directory, docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

Хвост волны dav-03 (DAV-12), PR #443, решение 1: у `DirAccount` нет признака скрытия из адресной книги — скрыть человека
можно только отключив вход.

Требует подтверждения владельца: добавить признак в контракт каталога (вариант А агента), а не скрывать только отключением входа? (PR #443, решение 1)

## Сделать
- `DirAccount`: `hidden_from_gal: bool` (`#[serde(default)]`), как у групп и почтовых контактов; testkit, наборы каталога;
  `docs/CONTRACTS.md` и CHANGELOG.

## Приёмка
- CI зелёный; набор каталога проверяет признак.
