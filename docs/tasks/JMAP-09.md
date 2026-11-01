# JMAP-09: фаззинг разборщика JMAP

lane: jmap
deps: [JMAP-01]
class: A
files: [crates/tp-jmap/fuzz/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S6.md#6.8]
checks: [nextest]
manual: no

## Сделать
- Цели cargo-fuzz: запросы JMAP (вызовы методов, ссылки на результаты), фильтры Email/query.

## Приёмка
- Прогон 10 мин без падений (CI-04).
