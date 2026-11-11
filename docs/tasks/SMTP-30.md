# SMTP-30: исход Moderate правил потока — в очередь модерации

lane: smtp
deps: [CTR-43, SMTP-22]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Решение владельца 2026-11-11, п. 9.

## Сделать
- `MailFlowAction::Moderate` — письмо в очередь модерации (CTR-43) вместо 451; отправителю — 250, модератору — уведомление.

## Приёмка
- Тест: правило с исходом Moderate — письмо в очереди, после одобрения доставлено, после отказа — DSN.
