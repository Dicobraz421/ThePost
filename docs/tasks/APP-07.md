# APP-07: хранилище секретов и остаток связки Ф0

lane: app
deps: [APP-01, PLT-12, PLT-13, BLOB-11, CLU-07, META-19]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-10-30-config-events-secrets.md#Секреты — только через хранилище секретов]
checks: [nextest]
manual: no

## Сделать
- `serve`: `VaultStore` (PLT-12) из секции `secrets`, `Reloader::with_secret_store`; `doctor` выводит `secrets::doctor()`.
- Перейти на `Deserialize`-секции tp-blob и tp-cluster (BLOB-11, CLU-07) и новые имена длительностей (PLT-13); убрать обход `spawn_blocking`.
- `verify` на рабочей БД через META-19.

## Приёмка
- Тест `roles: [all]` до ready с секретами из `MemSecretStore`; `doctor` показывает состояние хранилища.
