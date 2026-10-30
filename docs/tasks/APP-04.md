# APP-04: Профиль SMB

lane: app
deps: [APP-01, SMTP-04, IMAP-04, JMAP-04, SRCH-02, ADM-01]
class: A
files: [crates/thepost/src/*]
read: [docs/spec/S1.md#1.2]
checks: [nextest]
manual: no

## Сделать
- roles: [all], локальный BLOB, шина в процессе; конфигурация SMB из deploy/config.

## Приёмка
- Старт SMB-профиля до ready.
