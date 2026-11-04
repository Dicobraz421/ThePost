# IMAP-15: аудит доступа к ящику в IMAP

lane: imap
deps: [CTR-35, IMAP-07]
class: A
files: [crates/tp-imap/src/*]
read: [docs/decisions/2026-11-04-exchange-parity.md, docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

## Сделать
- Запись `MailboxAudit` для действий не владельца и действий владельца по политике.

## Приёмка
- Тесты: доступ к общему ящику записан, чтение владельцем — нет.
