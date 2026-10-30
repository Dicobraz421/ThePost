# ADM-03: Корзина

lane: admin
deps: [ADM-01, MS-05]
class: A
files: [crates/tp-admin/src/retention.rs]
read: [docs/spec/S5.md#5.2, docs/spec/S4.md#4.7]
checks: [nextest]
manual: no

## Сделать
- Список и восстановление элементов корзины; срок корзины через политику организации/домена/ящика.

## Приёмка
- Тесты API.
