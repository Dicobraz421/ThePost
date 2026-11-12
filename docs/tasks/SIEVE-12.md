# SIEVE-12: адрес клиента во входе ManageSieve

lane: sieve
deps: [CTR-57]
class: A
files: [crates/tp-sieve/src/*]
read: [docs/spec/S6.md#6.3]
checks: [nextest]
manual: no

Хвост волны app-16, PR #293: протоколы не передают адрес клиента в проверку учётных данных.

## Сделать
- ManageSieve `AUTHENTICATE`: `SaslContext.peer` — адрес клиента соединения.

## Приёмка
- Тест: `AUTHENTICATE PLAIN` — в проверку учётных данных пришёл адрес клиента сессии.
