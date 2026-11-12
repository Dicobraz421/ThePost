# SMTP-44: лимиты при отложенной отправке, один путь отправки

lane: smtp
deps: [SMTP-43]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- `Delivery::schedule` проверяет и учитывает «писем в час» и «получателей в час», как `submit`; сверх лимита —
  `451 4.7.1` (решение 115). `submit_unlimited` удалён.

## Приёмка
- Тест `scheduled_over_hourly_limit_is_refused`. Сделано в #609.
