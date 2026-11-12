# IMAP-23: поиск по тексту в чужом ящике — в журнал аудита

lane: imap
deps: [CTR-81]
class: A
files: [crates/tp-imap/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- `SEARCH`/`UID SEARCH` с ключами `BODY` или `TEXT` в общем или делегированном ящике пишет `AuditAction::Search` с
  папкой и строкой запроса; поиск только по заголовкам и флагам не журналируется.

## Приёмка
- Тест: представитель ищет `BODY "договор"` в общем ящике — одна запись `Search`; владелец в своём — записей нет.
