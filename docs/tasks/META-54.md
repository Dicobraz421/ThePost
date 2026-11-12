# META-54: занятость повторяющихся событий — разворот при запросе

lane: meta
deps: [CTR-47, META-51]
class: B
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*, crates/tp-meta/tests/*, crates/tp-contracts/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- `FreeBusy::query` разворачивает правила повторения (RRULE, EXDATE, RDATE, часовые пояса — calcard) на
  запрошенный интервал; индекс занятости — только для неповторяющихся событий; горизонт индекса убрать (решение 59).
  Правка контракта CTR-47 — в этой карточке (класс B).
- Периоды повторяющихся событий, которые META-51 (PR #448) пишет в `dav_busy` до `busy_horizon`, — убрать: строки
  только у неповторяющихся; `DavLimits::busy_horizon` — удалить или оставить без действия (с записью в CHANGELOG).

## Приёмка
- Тест: еженедельная встреча без конца видна через 3 года; исключённая дата не занята.
