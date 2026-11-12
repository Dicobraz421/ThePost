# JMAP-43: Email/query по тексту в чужом ящике — в журнал аудита

lane: jmap
deps: [CTR-81]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- `Email/query` с фильтром `text` или `body` по чужому аккаунту пишет `AuditAction::Search` (строка запроса — в
  `detail`), как IMAP-23.

## Приёмка
- Тест: запрос представителя с `text` — одна запись `Search`; с фильтром только `from` — записей нет.
