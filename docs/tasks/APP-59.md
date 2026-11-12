# APP-59: узел запускает экспорт журнала аудита в syslog/OTLP

lane: app
deps: [ADM-34]
class: A
files: [crates/thepost/src/*]
read: [docs/decisions/2026-11-11-stage2-followups.md]
checks: [nextest]
manual: no

## Сделать
- `AuditExportConfig::start` нигде не вызывается — `admin.audit.syslog`/OTLP в `config.yaml` ничего не делают (отчёт admin-23,
  #546). Запускать экспорт в узле при заданной секции; перечитывание без перезапуска; ошибка подключения — в `doctor`.

## Приёмка
- Тест узла: запись аудита уходит в тестовый приёмник syslog (отдельный файл в `crates/thepost/tests/`).
