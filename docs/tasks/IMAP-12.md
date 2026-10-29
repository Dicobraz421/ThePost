# IMAP-12: POP3

lane: imap
deps: [IMAP-04]
class: A
files: [crates/tp-imap/src/pop3.rs]
read: [docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- POP3 (RFC 1939) + STLS, SASL; выключен по умолчанию.

## Приёмка
- Тест сеанса.
