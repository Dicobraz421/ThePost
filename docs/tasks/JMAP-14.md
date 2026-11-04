# JMAP-14: автонастройка клиентов

lane: jmap
deps: [JMAP-01]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/decisions/2026-11-04-exchange-parity.md, docs/spec/S4.md#4.2]
checks: [nextest]
manual: no

## Сделать
- `/.well-known/jmap`, `/.well-known/caldav`, `/.well-known/carddav`; автонастройка Thunderbird (`/mail/config-v1.1.xml`); профиль `.mobileconfig`, подписанный сертификатом организации.

## Приёмка
- Снимки ответов; подпись профиля проверяется.
