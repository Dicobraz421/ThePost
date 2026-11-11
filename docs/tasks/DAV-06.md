# DAV-06: CalDAV — calendar-query и free-busy-query

lane: dav
deps: [DAV-05]
class: A
files: [crates/tp-dav/src/*]
read: [docs/spec/S4.md#4.4]
checks: [nextest]
manual: no

## Сделать
- `REPORT calendar-query` (RFC 4791 §7.8): `comp-filter`, `prop-filter`, `param-filter`, `text-match` (`i;ascii-casemap`, `i;unicode-casemap`),
  `time-range` по индексу (`in_span`) с точной проверкой развёрнутых повторов calcard; `calendar-data` с `expand` и `limit-recurrence-set`.
- `REPORT free-busy-query` (RFC 4791 §7.10) по календарю — VFREEBUSY из объектов (`TRANSP`, `STATUS`).

## Приёмка
- Тесты: фильтры на наборе событий (повторы, исключения `EXDATE`/`RECURRENCE-ID`, весь день, часовые пояса), `expand`.
