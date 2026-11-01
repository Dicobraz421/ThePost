# IMAP-14: EXPUNGE в корзине — TrashStore::purge; APPEND — MailStore::append

lane: imap
deps: [CTR-26, CTR-27, IMAP-07]
class: A
files: [crates/tp-imap/src/*]
read: [docs/decisions/2026-11-01-trash-purge.md]
checks: [nextest]
manual: no

## Сделать
- EXPUNGE/UID EXPUNGE в корзине → `purge` (вместо `NO [CANNOT]`); APPEND → `MailStore::append` (вместо `ingest`).

## Приёмка
- Тест: «очистить корзину» в клиенте — письма исчезают у пользователя; APPEND не вызывает точки принятия решений доставки.
