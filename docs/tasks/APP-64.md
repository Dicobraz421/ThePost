# APP-64: POP3 сверяет сеансы с хранилищем после потери сигнала блокировки

lane: app
deps: []
class: A
files: [crates/thepost/src/protocols.rs]
read: [docs/tasks/IMAP-22.md]
checks: [nextest]
manual: no

## Сделать
- IMAP-22 (#641): при потере сигнала `AccountDisabled` IMAP сверяет сеансы с `Extensions::accounts`, POP3 — нет:
  `Pop3Server::new(session())` в `crates/thepost/src/protocols.rs` создаётся без `AccountStore`.
- Передать `AccountStore` в `Pop3Server` в узле, чтобы сверка после переподписки работала и для POP3.

## Приёмка
- Тест: сигнал потерян, учётная запись заблокирована — сеанс POP3 закрывается после сверки.
