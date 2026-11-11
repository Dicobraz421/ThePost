# AUTH-12: Authorizer — права на календари и адресные книги

lane: auth
deps: [CTR-48, AUTH-07]
class: A
files: [crates/tp-auth/src/*]
read: [docs/spec/S4.md#4.4, docs/spec/S4.md#4.5, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Единый слой прав (SRS 4.5) для коллекций: владелец — всё; `CollectionAcl` с группами и `anyone`; представители (`Delegates`) —
  чтение или запись всех календарей владельца; «только занятость» — `FreeBusy` без деталей; политика организации ограничивает выдачу.

## Приёмка
- Тесты на in-memory: группы, отрицательные права, представитель, ограничение политикой.
