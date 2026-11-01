# APP-08: сборка взаимодействия без опроса

lane: app
deps: [META-26, CLU-09, CLU-10]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-01-interaction-db-ha.md, docs/spec/S2.md#2.8, docs/spec/S3.md#3.1]
checks: [nextest]
manual: no

## Сделать
- Выбор шины: в процессе (один процесс) или `PgSignalBus` (кластер); секции Valkey удалить.
- Чтение журнала событий (`consume`) — по сигналу `LogAppended` и при `Resync`, периодический опрос удалить.
- `thepost doctor`: версия СУБД, синхронная реплика, отсутствие сторонних расширений.

## Приёмка
- Тест: событие журнала обрабатывается без ожидания периода; `config check` отклоняет секцию Valkey.
