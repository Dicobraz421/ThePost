# META-11: ACL на PostgreSQL

lane: meta
deps: [CTR-05, META-03]
class: B
files: [crates/tp-meta/src/acl.rs, crates/tp-meta/migrations/*]
read: [docs/spec/S4.md#4.5]
checks: [nextest]
manual: no

## Сделать
- Хранение ACL ящиков (RFC 4314) и делегирования Send As / On Behalf.

## Приёмка
- Тесты CRUD и выборки прав.
