# META-51: входящие планирования и индекс занятости

lane: meta
deps: [CTR-47, META-47]
class: B
files: [crates/tp-meta/migrations/*, crates/tp-meta/src/*, crates/tp-meta/tests/*]
read: [docs/CONTRACTS.md]
checks: [nextest]
manual: no

## Сделать
- Миграция: `dav_busy` (объект, начало, конец, вид) — индекс занятости до горизонта; входящие планирования; `schedule_tag` объекта; RLS.
- `FreeBusy::query` одним запросом по учётным записям и интервалу; `inbox_*` и `Schedule-Tag` в `DavStore`.

Горизонт индекса для повторяющихся событий отменён решением 59: разворот при запросе и индекс только для
неповторяющихся — META-54 (волна meta-35 после этой).

## Приёмка
- Наборы `suite::freebusy`, `suite::itip` на PostgreSQL при `TEST_DATABASE_URL`.
