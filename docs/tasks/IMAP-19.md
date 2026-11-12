# IMAP-19: адрес клиента во входе IMAP и POP3

lane: imap
deps: [CTR-57]
class: A
files: [crates/tp-imap/src/*]
read: [docs/spec/S6.md#6.3]
checks: [nextest]
manual: no

Хвост волны app-16, PR #293: протоколы не передают адрес клиента в проверку учётных данных.

## Сделать
- IMAP `LOGIN`, `AUTHENTICATE` и POP3 `USER`/`PASS`, `AUTH`: адрес клиента — в `SaslContext.peer` и в `verify_from`
  (`session.rs`, `pop3.rs`; сейчас — `verify(creds)` без адреса).

## Приёмка
- Тесты IMAP и POP3: в проверку учётных данных пришёл адрес клиента сессии.
