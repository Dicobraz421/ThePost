# APP-58: узел: вход GSSAPI

lane: app
deps: [DEPS-12]
class: A
files: [crates/thepost/src/*]
read: [docs/tasks/DEPS-12.md]
checks: [nextest]
manual: no

Хвост волны deps-10…12, PR #550: фича `gssapi` проброшена, но `AuthSasl::with_gssapi` в `services.rs` не вызывается.

## Сделать
- `services.rs`: при фиче `gssapi` и заданном keytab — `AuthSasl::with_gssapi`; без фичи — механизм не объявляется.

## Приёмка
- Тест сборки сервисов под фичей: механизм `GSSAPI` объявлен в SMTP и IMAP.
