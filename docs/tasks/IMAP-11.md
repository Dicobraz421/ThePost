# IMAP-11: COMPRESS и мелкие расширения

lane: imap
deps: [IMAP-02]
class: A
files: [crates/tp-imap/src/compress.rs]
read: [docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- COMPRESS=DEFLATE, UNSELECT; совместная проверка LITERAL+ и BINARY.

## Приёмка
- Тест сжатого сеанса.
