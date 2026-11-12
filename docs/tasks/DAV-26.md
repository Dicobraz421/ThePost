# DAV-26: адрес клиента во входе CalDAV и CardDAV

lane: dav
deps: [CTR-57]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S6.md#6.3]
checks: [nextest]
manual: no

Хвост волны app-16, PR #293: протоколы не передают адрес клиента в проверку учётных данных.

## Сделать
- `server.rs`: Basic и Bearer — `verify_from(creds, peer)`, адрес — из соединения (`ConnectInfo`).

## Приёмка
- Тест: `PROPFIND` с Basic — в проверку учётных данных пришёл адрес соединения.
