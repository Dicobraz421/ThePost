# META-01: Схема БД v1 и миграции

lane: meta
deps: [CTR-03]
class: B
files: [crates/tp-meta/migrations/*, crates/tp-meta/src/schema.rs]
read: [docs/spec/S3.md#3.1, docs/spec/S3.md#3.4, docs/spec/S3.md#3.6, docs/spec/S3.md#3.8]
checks: [nextest]
manual: no

## Сделать
- Таблицы tenants, domains, accounts, groups, mailboxes, messages, message_parts, blobs(tenant, hash, gen, refcnt, state, size, zero_since),
- changelog, events, event_cursors, shards; tenant_id и account_id в ключах, без FK между ящиками; организация по умолчанию;
- RLS по tenant_id (параметр сеанса app.tenant_id через SET LOCAL); sqlx migrate; применение при старте по флагу.

## Приёмка
- Миграции применяются на чистую БД и идемпотентны; запрос без установленного tenant не видит строк (тесты с TEST_DATABASE_URL).
