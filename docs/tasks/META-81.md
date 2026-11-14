# META-81: тип ресурса бронирования в PostgreSQL — миграция и хранилище

lane: meta
deps: [CTR-88]
class: A
files: [crates/tp-meta/*]
read: [docs/tasks/CTR-88.md, docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Миграция (номер выдаёт оркестратор): столбец `resource_type` у учётных записей — `text`, `CHECK` на `room` и
  `equipment`, у пользователя `NULL` (ограничение по виду учётной записи); существующим ресурсам — `room`.
- `PgMeta`: чтение и запись `Account::resource_type` и `AccountUpsert::resource_type` по контракту CTR-88; смена вида
  на пользователя обнуляет тип.

## Приёмка
- Тест набора `tp-testkit` на `PgMeta` проходит; тест миграции: ресурс, созданный до неё, читается как `Room`.
