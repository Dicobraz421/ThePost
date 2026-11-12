# JMAP-41: заблокированная учётная запись — закрытие push JMAP

lane: jmap
deps: [CTR-64]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/tasks/CTR-64.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 51: admin-13, PR #487.

## Сделать
- По `AccountDisabled` потоки EventSource и WebSocket учётной записи закрываются; обычные запросы и так проверяют вход.

## Приёмка
- Тест: открытый EventSource закрывается после сигнала.
