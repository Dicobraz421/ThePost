# JMAP-34: ContactCard — перенос между книгами, запрос по индексу, тесты на корпусе vCard

lane: jmap
deps: [CTR-54, KIT-09, JMAP-26]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/spec/S4.md#4.4, docs/CONTRACTS.md#Dav]
checks: [nextest]
manual: no

Хвост волны jmap-16, PR #428.

Требует подтверждения владельца: перенос контакта между книгами — с сохранением `id` через `DavStore` (вариант (а) агента)? (п. 30 синка 12.11)

## Сделать
- `ContactCard/set`: смена `addressBookIds` — `DavStore::move_object` (CTR-54), `id` карточки сохраняется; сейчас —
  `invalidProperties`. Несколько книг у одной карточки — по-прежнему отказ.
- `ContactCard/query`: фильтры `text`, `name`, `name/given`, `name/surname`, `email` берут кандидатов из
  `DavStore::find_cards`, точная проверка — как сейчас; остальные фильтры — прежним перебором. Сейчас каждый запрос
  читает и разбирает все карточки учётной записи.
- Тесты «без потери полей» JSContact ↔ vCard — на корпусе `tp-testkit` (KIT-09) вместо трёх карточек внутри теста.

## Приёмка
- Тесты: перенос карточки — прежний `id`, изменения видны в `ContactCard/changes` и в CardDAV; запрос по имени и
  адресу не вызывает чтение всех объектов (счётчик `MemDav`); корпус vCard проходит туда и обратно без потерь.
