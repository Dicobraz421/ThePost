# SYS-02: Ресурсы узла

lane: sys
deps: []
class: A
files: [crates/tp-sys/src/resources.rs]
read: [docs/spec/S5.md#5.5]
checks: [nextest]
manual: no

## Сделать
- Определение доступных CPU (cgroup v2 cpu.max, sched_getaffinity) и памяти (memory.max, /proc/meminfo) для автонастройки.

## Приёмка
- Тесты на фикстурах файлов cgroup.
