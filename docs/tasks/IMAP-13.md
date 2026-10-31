# IMAP-13: фаззинг разборщика IMAP

lane: imap
deps: [IMAP-01]
class: A
files: [crates/tp-imap/fuzz/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S6.md#6.8]
checks: [nextest]
manual: no

## Сделать
- Цели cargo-fuzz: команды IMAP4rev2, литералы, SEARCH, FETCH-атрибуты.

## Приёмка
- Прогон 10 мин без падений (CI-04).
