# SMTP-04: SPF, DKIM, DMARC, ARC

lane: smtp
deps: [SMTP-03]
class: A
files: [crates/tp-smtp/src/authcheck.rs]
read: [docs/spec/S4.md#4.1]
checks: [nextest]
manual: no

## Сделать
- mail-auth с DNS-резолвером, политика DMARC (reject/quarantine → Junk), Authentication-Results.

## Приёмка
- Тесты на подписанных фикстурах с мок-резолвером.
