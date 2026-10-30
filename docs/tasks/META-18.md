# META-18: динамические учётные данные PostgreSQL

lane: meta
deps: [META-17, PLT-12]
class: A
files: [crates/tp-meta/src/*]
read: [docs/decisions/2026-10-30-config-events-secrets.md#Секреты — только через хранилище секретов]
checks: [nextest]
manual: docs/manual-tests/META-18.md

## Сделать
- По настройке `database.dynamic_credentials`: логин и пароль из database secrets engine OpenBao; до истечения аренды —
  продление, при отказе — новые учётные данные и `PgPool::set_connect_options` без остановки ролей.

## Приёмка
- Тест на `MemSecretStore` с коротким сроком аренды: новые соединения идут с новыми учётными данными, текущие запросы не обрываются.
- Ручная проверка на стенде с OpenBao (DEP-07).
