# CTR-54: `DavStore` — перенос объекта между коллекциями и поиск контактов по индексу

lane: contracts
deps: [CTR-45]
class: B
files: [crates/tp-contracts/src/*, crates/tp-testkit/src/*, docs/CONTRACTS.md]
read: [docs/CONTRACTS.md#Dav, docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

Хвост волны jmap-16, PR #428.

Подтверждено владельцем — решение 80 (`docs/decisions/2026-11-11-stage2-followups.md`).

## Сделать
- `DavStore::move_object(scope, from, name, to, pre)`: переносит ресурс в другую коллекцию учётной записи одной
  транзакцией; `DavObject::id`, тело и индекс сохраняются, ETag и `ModSeq` — новые; в исходной коллекции — надгробие
  (видно в `changes`), в целевой — создание. Коллекции разного вида или компонент не из `Collection::components`
  целевой — `Invalid`; UID или имя заняты в целевой — исход `UidConflict` или отказ по условию; лимит
  `DavLimits::max_objects` целевой — `QuotaExceeded`. По умолчанию — `Error::Internal` (реализации — `tp-meta`, `tp-testkit`).
- Поиск контактов: в `ObjectIndex` — поисковые строки контакта (`#[serde(default)]`; полное имя, части имени, адреса
  e-mail — нижний регистр, с пределами числа и длины), вычисляет вызывающий (`tp_dav::index::card`);
  `DavStore::find_cards(scope, collection, needle, limit)` — кандидаты, у которых строка индекса содержит `needle`;
  точную проверку фильтра делает вызывающий (как `in_range` для time-range). По умолчанию — перебор `list_objects`.
- Реализация в памяти, общий набор тестов, `docs/CONTRACTS.md#Dav`, CHANGELOG. Потребители: META-58, DAV-24, JMAP-34.

## Приёмка
- CI зелёный; тесты набора: перенос сохраняет `id` и виден в `changes` обеих коллекций; конфликт UID в целевой;
  `find_cards` по части имени и по адресу.
