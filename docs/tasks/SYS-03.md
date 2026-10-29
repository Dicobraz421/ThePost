# SYS-03: Сокеты слушателей

lane: sys
deps: []
class: A
files: [crates/tp-sys/src/socket.rs]
read: [docs/spec/S6.md#6.1]
checks: [nextest]
manual: no

## Сделать
- Bind TCP с SO_REUSEPORT, backlog, keepalive, IPv4/IPv6; передача в tokio TcpListener.

## Приёмка
- Тест: два слушателя на одном порту с SO_REUSEPORT.
