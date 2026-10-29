# IMAP-01: Парсер и сериализатор IMAP4rev2

lane: imap
deps: []
class: A
files: [crates/tp-imap/src/proto/*]
read: [docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- Разбор команд и формирование ответов RFC 9051: литералы, LITERAL+, строки, списки, последовательности.

## Приёмка
- Тесты по примерам RFC; fuzz-цель парсера.
