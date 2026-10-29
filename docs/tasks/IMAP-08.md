# IMAP-08: IDLE

lane: imap
deps: [IMAP-04, CLU-01]
class: A
files: [crates/tp-imap/src/idle.rs]
read: [docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- IDLE через EventBus, EXISTS/EXPUNGE/FETCH FLAGS уведомления, таймаут простоя из конфигурации.

## Приёмка
- Тест: изменение в другой сессии приходит в IDLE.
