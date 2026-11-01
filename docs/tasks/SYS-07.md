# SYS-07: seccomp и Landlock по ролям

lane: sys
deps: [SYS-04, SYS-06]
class: B
files: [crates/tp-sys/src/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S2.md#2.7, docs/spec/S6.md#6.8]
checks: [nextest]
manual: docs/manual-tests/SYS-07.md

## Сделать
- Профиль seccomp на набор ролей (объединение разрешённых системных вызовов), режим журнала для отладки.
- Landlock: доступ только к каталогам ролей (BLOB-диск, кэш NVMe, конфигурация, секреты); edge — только конфигурация и файлы секретов.
- Применение после инициализации ролей, до приёма соединений.

## Приёмка
- Тест: запрещённый вызов и доступ вне разрешённых каталогов отклоняются; на ядре без Landlock — предупреждение, не отказ.
