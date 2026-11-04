# SMTP-25: личные списки отправителей при доставке

lane: smtp
deps: [CTR-35, SMTP-16]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-04-exchange-parity.md, docs/spec/S4.md#4.1]
checks: [nextest]
manual: no

## Сделать
- При доставке: заблокированный отправитель — «Спам», надёжный — «Входящие» поверх вердикта антиспама.

## Приёмка
- Тесты на in-memory списках.
