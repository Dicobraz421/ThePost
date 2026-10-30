# APP-01: Сборка ролей по конфигурации

lane: app
deps: [PLT-05, PLT-09, META-04, META-13, BLOB-03, MS-02, CLU-01]
class: A
files: [crates/thepost/src/*]
read: [docs/spec/S2.md#2.1, docs/spec/S1.md#1.2]
checks: [nextest]
manual: no

## Сделать
- main: CLI (clap) `serve`, `config schema`, `config check` (строгий режим PLT-09; решение 2026-10-30, #29); пресеты ролей; выбор реализаций (локальный/S3, шина в процессе/Valkey);
- модули: cargo features + modules.<имя>.enabled → Module.register в реестрах.
- Разрешено зависеть от крейтов всех полос.

## Приёмка
- Запуск с минимальным config против PostgreSQL из compose проходит до ready (тест при TEST_DATABASE_URL).
