# PLT-17: перечитывание конфигурации и сертификатов по событиям

lane: platform
deps: [PLT-16]
class: A
files: [crates/tp-platform/src/*]
read: [docs/decisions/2026-11-01-interaction-db-ha.md, docs/spec/S2.md#2.8]
checks: [nextest]
manual: no

## Сделать
- Конфигурация и файлы TLS — inotify на каталог (замена ConfigMap через символическую ссылку) и SIGHUP; опрос — только если inotify недоступен.
- Наблюдение за памятью и давлением ресурсов — триггеры PSI cgroups; интервальный замер RSS — только при их отсутствии.

## Приёмка
- Тесты: изменение файла применяется без ожидания периода; при недоступном inotify — запасной опрос.
