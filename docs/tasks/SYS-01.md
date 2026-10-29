# SYS-01: Сигналы процесса

lane: sys
deps: []
class: A
files: [crates/tp-sys/src/signals.rs]
read: [docs/spec/S2.md#2.2]
checks: [nextest]
manual: no

## Сделать
- Поток сигналов SIGHUP/SIGTERM/SIGINT → enum Signal (tokio).

## Приёмка
- Тест: отправка SIGHUP себе принимается потоком.
