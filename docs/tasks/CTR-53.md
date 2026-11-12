# CTR-53: адреса учётной записи — основной и псевдонимы

lane: contracts
deps: []
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md#Metadata, docs/CONTRACTS.md#Directory, docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

Хвост волны dav-02, PR #427.

## Сделать
- `AccountStore::addresses(tenant, id) -> Result<Vec<String>>`: адреса, на которые учётной записи доставляется почта, —
  основной первым, затем псевдонимы каталога (`DirAlias` с целью — этой учётной записью) без catch-all `@domain`;
  нижний регистр, без повторов. Нет учётной записи — `Error::NotFound`. По умолчанию — только основной адрес через `get`.
- `tp-testkit`: реализация в памяти с псевдонимами и общий тест набора.
- `docs/CONTRACTS.md#Metadata`, CHANGELOG. Потребители: META-57 (PostgreSQL), DAV-23 (`calendar-user-address-set`).

## Приёмка
- CI зелёный; тест набора: учётная запись с двумя псевдонимами и catch-all домена — три адреса, основной первым.
