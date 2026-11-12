# META-62: повтор `enqueue` и пароля приложения при переключении СУБД

lane: meta
deps: [META-40]
class: A
files: [crates/tp-meta/src/*, crates/tp-meta/tests/*, docs/manual-tests/DEP-09.md]
read: [docs/tasks/META-40.md]
checks: [nextest]
manual: no

Хвост волны meta-24 (META-40), PR #263: `enqueue` и `app_passwords.insert` без повтора при переключении СУБД; `rewrite`
при настоящем обрыве и работа за PgBouncer не проверены.

## Сделать
- `enqueue`, `app_passwords.insert` — повтор по ключу (`with_tenant_keyed`, как META-40).
- Тест с настоящим обрывом (`pg_terminate_backend` во время записи) для `enqueue`, `rewrite`, пароля приложения; проверка
  за PgBouncer — шаг в `docs/manual-tests/DEP-09.md`.

## Приёмка
- Тесты PostgreSQL: после обрыва запись выполнена ровно один раз.
