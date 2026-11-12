# JMAP-40: календарь из JMAP: `Schedule-Tag` и занятость

lane: jmap
deps: [DAV-32]
class: A
files: [crates/tp-jmap/src/*]
read: [docs/tasks/DAV-32.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 45: dav-04, PR #485. `calendar_event.rs` пишет событие без `Schedule-Tag` и без обрезки занятости.

## Сделать
- `CalendarEvent/set`: индекс объекта — `tp_dav::index::for_write` (DAV-32), как у CalDAV.

## Приёмка
- Тест: событие из JMAP получает `Schedule-Tag` и ту же занятость, что при записи по CalDAV.
