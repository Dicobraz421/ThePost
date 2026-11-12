# JMAP-37: адрес клиента во входе JMAP по HTTP

lane: jmap
deps: [CTR-57]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/spec/S6.md#6.3]
checks: [nextest]
manual: no

Хвост волны app-16, PR #293: протоколы не передают адрес клиента в проверку учётных данных.

## Сделать
- `core/http.rs`: Basic и Bearer — `verify_from(creds, peer)`, адрес — из соединения (`ConnectInfo`); за обратным
  прокси — адрес прокси (доверенные прокси и `X-Forwarded-For` — вне карточки).

## Приёмка
- Тест: запрос с Basic — в проверку учётных данных пришёл адрес соединения.
