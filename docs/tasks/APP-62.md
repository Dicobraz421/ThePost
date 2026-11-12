# APP-62: приглашения из почтового клиента в узле — права отправителя и iTIP для JMAP и DAV

lane: app
deps: []
class: A
files: [crates/thepost/src/services.rs]
read: [docs/tasks/SMTP-36.md]
checks: [nextest]
manual: no

## Сделать
- SMTP-36 (#633): `Delivery::with_authz` и путь `apply_itip` для submission и `Delivery::submit`.
- В `crates/thepost/src/services.rs`: `delivery()` — `.with_authz(self.authz.clone())`; `base_delivery` (JMAP и DAV) —
  `.with_itip(...)` и `.with_authz(...)`. Без этого на узле приглашения разбираются только из submission и только от
  своего адреса или псевдонима.

## Приёмка
- Тест узла: приглашение, отправленное через JMAP от имени с правом Send on Behalf, попадает в календарь адресата.
- Источник: отчёт smtp-16 (#633).
