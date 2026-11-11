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

## Итог волны jmap-14
- Маршруты (`crates/tp-jmap/src/autoconfig.rs`): `/.well-known/caldav`, `/.well-known/carddav` — 301 на контекст DAV (любой метод);
  `/mail/config-v1.1.xml` и `/.well-known/autoconfig/mail/config-v1.1.xml` — Thunderbird (IMAP, SMTP, CardDAV, CalDAV);
  `/mail/profile.mobileconfig` — профиль iOS/macOS вошедшего пользователя. `/.well-known/jmap` — с JMAP-01.
- Серверы и порты — секция `jmap.autoconfig` (`host`, `organization`, `imap`, `submission`, `caldav`, `carddav`).
- Профиль **без подписи**: подпись CMS требует новой зависимости (полоса deps) — решение владельца; приёмка «подпись проверяется» не выполнена.
