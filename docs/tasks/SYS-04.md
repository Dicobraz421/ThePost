# SYS-04: Sandbox подпроцесса

lane: sys
deps: []
class: B
files: [crates/tp-sys/src/sandbox/*]
read: [docs/spec/S4.md#4.6]
checks: [nextest]
manual: no

## Сделать
- Запуск дочернего процесса (тот же бинарник, подкоманда) с seccomp-фильтром (seccompiler), rlimit CPU/AS,
- таймаутом, вводом через stdin и выводом через stdout; единственное место с `unsafe` — с комментариями SAFETY.

## Приёмка
- Тесты: превышение памяти и времени завершают процесс с ошибкой; запрещённый syscall (socket) убивает процесс.
