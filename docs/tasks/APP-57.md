# APP-57: узел: экспорт журнала аудита в syslog и OTLP

lane: app
deps: [ADM-29]
class: A
files: [crates/thepost/src/*]
read: [docs/tasks/ADM-29.md]
checks: [nextest]
manual: no

Хвост реестра облачных волн, п. 78: admin-19, PR #535. узел не запускает экспорт аудита — `admin.audit.syslog`/`otlp` не действуют.

## Сделать
- `serve.rs`: `cfg.admin.audit.start(profile, &*secrets)`, приёмники — в `Fanout` вместе с журналом аудита в БД (ADM-34).

## Приёмка
- Интеграционный тест узла: изменение через Admin API доходит до тестового приёмника syslog.
