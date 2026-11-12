# IMAP-22: заблокированная учётная запись — закрытие сеансов IMAP и POP3

lane: imap
deps: [CTR-64]
class: A
files: [crates/tp-imap/src/*]
read: [docs/tasks/CTR-64.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 51: admin-13, PR #487.

## Сделать
- По `AccountDisabled` открытые сеансы IMAP (в том числе IDLE) получают `* BYE` и закрываются, POP3 — закрытие
  соединения. ManageSieve и submission — тем же сигналом, при необходимости отдельными карточками.

## Приёмка
- Тест: сеанс в IDLE после сигнала получает `BYE`, новый вход отклонён.
