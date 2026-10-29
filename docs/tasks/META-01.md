# META-01: Схема БД v1 и миграции

lane: meta
deps: [CTR-03]
class: B
files: [crates/tp-meta/migrations/*, crates/tp-meta/src/schema.rs]
read: [docs/spec/S3.md#3.1, docs/spec/S3.md#3.4, docs/spec/S3.md#3.6]
checks: [nextest]
manual: no

## Сделать
- Таблицы tenants, accounts, mailboxes, messages, message_parts, blobs(hash, gen, refcnt, state, size, zero_since),
- changelog, shards; все ключи ящика с account_id, без FK между ящиками; sqlx migrate; применение при старте по флагу.

## Приёмка
- Миграции применяются на чистую БД и идемпотентны (тест с TEST_DATABASE_URL).
