# META-20: нестабильный pg_lease second_holder_waits_for_expiry

lane: meta
deps: []
class: A
files: [crates/tp-meta/tests/pg_lease.rs]
read: []
checks: [nextest]
manual: no

## Сделать
- Тест падает на медленной машине (лиза 400 мс, отчёт meta-03, PR #120). Убрать зависимость от скорости машины: ждать событие, а не время, либо задать запас, не связанный с таймингом планировщика.

## Приёмка
- 50 прогонов подряд под нагрузкой (`cargo nextest run --retries 0` в цикле) без падений.
