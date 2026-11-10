# KIT-07: in-memory ящик — 6 системных папок и дополнение при открытии

lane: testkit
deps: [CMP-05]
class: A
files: [crates/tp-testkit/src/*]
read: []
checks: [nextest]
manual: no

Отчёт meta-17 (CMP-05): в PostgreSQL системных папок 6 (с Archive) и `MailboxStore::list` дополняет недостающие; в
`tp-testkit` `SYSTEM_FOLDERS` — 5 папок, без дополнения. Тесты полос на фейках расходятся с PG.

## Сделать
- `SYSTEM_FOLDERS` фейка — Inbox, Drafts, Sent, Junk, Trash, Archive; `upsert` и `list` дополняют недостающие, не дублируя
  папки с той же ролью или именем верхнего уровня (без учёта регистра) — как `tp-meta::mailboxes::ensure_system`.
- Conformance-тест testkit: новая запись — 6 папок; запись без папок — дополняется при `list`.
- Тесты полос, считающие 5 папок, — поправить в своих волнах (перечислить в отчёте).
