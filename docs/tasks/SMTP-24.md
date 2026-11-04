# SMTP-24: отложенная отправка и отмена

lane: smtp
deps: [CTR-35, SMTP-14]
class: A
files: [crates/tp-smtp/src/*]
read: [docs/decisions/2026-11-04-exchange-parity.md, docs/spec/S4.md#4.1]
checks: [nextest]
manual: no

## Сделать
- Выпуск письма по `release_at` (таймер ближайшего срока, SRS 2.8); отменённая до срока отправка не уходит, тело освобождается.

## Приёмка
- Тесты: выпуск в срок, отмена до срока.
