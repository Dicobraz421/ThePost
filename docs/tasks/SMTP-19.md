# SMTP-19: фаззинг разборщиков SMTP

lane: smtp
deps: [SMTP-01]
class: A
files: [crates/tp-smtp/fuzz/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S6.md#6.8]
checks: [nextest]
manual: no

## Сделать
- Цели cargo-fuzz: команды сессии, расширения ESMTP, PROXY v2, ответы Milter.

## Приёмка
- Прогон 10 мин без падений (CI-04).
