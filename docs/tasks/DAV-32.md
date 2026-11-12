# DAV-32: открытый `index::for_write`; срок обработанных сообщений входящих

lane: dav
deps: [DAV-15]
class: A
files: [crates/tp-dav/src/*]
read: [docs/tasks/DAV-15.md, docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 55: jmap-19a, PR #493. и п. 34 (meta-32, PR #448): JMAP пишет календарь без расчёта индекса tp-dav; при
`dav.schedule_keep_processed = false` (решение 79) уведомлениям JMAP о приглашениях нечего показать.

## Сделать
- `tp_dav::index::for_write` — публичная функция расчёта индекса объекта (`Schedule-Tag`, занятость с обрезкой) для
  JMAP (JMAP-40).
- При `schedule_keep_processed = true` — срок хранения обработанных (`dav.schedule_processed_ttl`, по умолчанию 30 сут) и шаг очистки для
  maintenance (подключение — APP-53).

## Приёмка
- Тесты: индекс из `for_write` совпадает с индексом при записи по CalDAV; обработанные старше срока удаляются.
