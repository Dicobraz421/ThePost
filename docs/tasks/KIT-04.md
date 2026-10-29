# KIT-04: Корпус писем и хелперы тестов

lane: testkit
deps: []
class: A
files: [crates/tp-testkit/src/corpus.rs, crates/tp-testkit/corpus/*]
read: [docs/spec/APP-D.md#Д.3]
checks: [nextest]
manual: no

## Сделать
- ≥40 .eml: multipart/mixed/alternative/related, вложения PDF/DOCX/XLSX, 8bit, кодировки КОИ-8/Windows-1251/UTF-8,
- битый MIME, длинные строки, вложенные message/rfc822; генератор синтетических писем по модели Д.3.
- Хелпер `pg_url() -> Option<String>` из TEST_DATABASE_URL и `pg_schema()` — изолированная схема на тест.

## Приёмка
- Все письма разбираются mail-parser без паники (тест).
