# SYS-10: дескрипторы сокетов systemd в `TcpListener`

lane: sys
deps: [SYS-01]
class: A
files: [crates/tp-sys/src/*]
read: []
checks: [nextest]
manual: no

Отчёт deploy-05 (DEP-08): узел не принимает сокеты systemd — `TODO(PLT-16)` в `thepost/protocols.rs`: для
`listen_fds` → `InheritedSockets::adopt` нужен `FromRawFd`, а `unsafe` разрешён только в tp-sys.

## Сделать
- Безопасная функция tp-sys: дескриптор из `LISTEN_FDS` (проверка `SO_ACCEPTCONN`, `SOCK_STREAM`, владение — один раз)
  → `std::net::TcpListener`; повторный захват того же номера — ошибка.
- Тест: сокет, созданный в тесте, через номер дескриптора → `TcpListener` принимает соединение.
