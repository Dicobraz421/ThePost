# JMAP-21: push — изменение всех типов по `Resync`

lane: jmap
deps: [CLU-13]
class: A
files: [crates/tp-jmap/src/*]
read: []
checks: [nextest]
manual: no

Отчёт cluster-08 (CLU-13): `changed_types` (`push.rs`) для `Resync` отдаёт пустой список — `StateChange` не отправляется.

## Сделать
- `Resync` → `StateChange` со всеми типами учётной записи (EventSource и WebSocket); клиент перечитает состояния.
- Тест: `Resync` в шине → клиент push получает `StateChange` со всеми типами.
