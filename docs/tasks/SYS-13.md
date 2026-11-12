# SYS-13: свободное место файловой системы (`statvfs`)

lane: sys
deps: []
class: A
files: [crates/tp-sys/src/*]
read: [docs/tasks/APP-10.md]
checks: [nextest]
manual: no

Хвост волны app-06 (APP-10), PR #291: функции `statvfs` в tp-sys нет, а `unsafe` вне tp-sys запрещён.

## Сделать
- `tp_sys::fs_usage(path) -> { total, available }` (`statvfs`), ошибки — `io::Error`.

## Приёмка
- Тест: для временного каталога `available ≤ total`, несуществующий путь — ошибка.
