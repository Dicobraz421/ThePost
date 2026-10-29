# SMTP-11: Лимиты отправки и метрики

lane: smtp
deps: [SMTP-06]
class: A
files: [crates/tp-smtp/src/limits.rs]
read: [docs/spec/S6.md#6.3, docs/spec/S6.md#6.4]
checks: [nextest]
manual: no

## Сделать
- Лимит писем/получателей в час на учётку, метрики очереди и сессий.

## Приёмка
- Тест превышения лимита.
