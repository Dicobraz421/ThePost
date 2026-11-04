# JMAP-18: аудит доступа к ящику в JMAP

lane: jmap
deps: [CTR-35, JMAP-05]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/decisions/2026-11-04-exchange-parity.md, docs/spec/S5.md#5.2]
checks: [nextest]
manual: no

## Сделать
- Запись `MailboxAudit` для действий не владельца и действий владельца по политике.

## Приёмка
- Тесты: доступ представителя записан, чтение владельцем — нет.
