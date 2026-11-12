# CTR-60: `DavStore` — глобальная адресная книга организации

lane: contracts
deps: [CTR-45, CTR-58]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md, docs/CONTRACTS.md#Dav, docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

Синк 12.11, часть 2, решение 72 (хвост jmap-16, PR #428, часть JMAP-26; п. 29 синка).

## Сделать
- Книга GAL — коллекция организации в `DavStore` (вид адресной книги, только чтение для пользователей), видна в доме
  каждой учётной записи организации; `list_objects`, `get_object`, `changes`, `find_cards` — как у обычной книги.
- Запись — служебной операцией `DavStore::sync_gal(tenant, cards)`: полный набор карточек (UID, vCard, индекс)
  заменяет прежний одной транзакцией; карточка с тем же содержимым сохраняет ETag и не попадает в `changes`
  (ETag по содержимому, решение 72, DAV-28); исчезнувшая — надгробие.
- Запись пользователя в книгу GAL — `Forbidden`. По умолчанию — `Error::Internal` (реализации — `tp-meta`, `tp-testkit`).
- Реализация в памяти, общий набор тестов, `docs/CONTRACTS.md#Dav`, CHANGELOG. Потребители: META-63, DAV-29, JMAP-39.

## Приёмка
- CI зелёный; тесты набора: повторный `sync_gal` без изменений не меняет ETag и `changes`; изменённая карточка — новый
  ETag; удалённая — надгробие; запись пользователя — `Forbidden`.
