# PLT-10: метрики шины в общем реестре

lane: platform
deps: [CLU-05, PLT-03]
class: A
files: [crates/tp-platform/src/*]
read: [docs/decisions/2026-10-30-owner-answers.md#Платформа и наблюдаемость]
checks: [nextest]
manual: no

## Сделать
- Снимать счётчики шины (CLU-05: отправлено, получено, переподключения, ошибки) в общий реестр Prometheus платформы; в tp-cluster новых зависимостей нет.

## Приёмка
- `/metrics` содержит метрики шины в тесте с шиной в процессе.
