# APP-01: Сборка ролей по конфигурации

lane: app
deps: [PLT-05, META-04, BLOB-03, MS-02, CLU-01]
class: A
files: [crates/thepost/src/*]
read: [docs/spec/S2.md#2.1, docs/spec/S1.md#1.2]
checks: [nextest]
manual: no

## Сделать
- main: CLI (clap) `serve`, выбор реализаций (локальный/S3, шина в процессе/Valkey) по конфигурации, запуск ролей.
- Разрешено зависеть от крейтов всех полос.

## Приёмка
- Запуск с минимальным config против PostgreSQL из compose проходит до ready (тест при TEST_DATABASE_URL).
