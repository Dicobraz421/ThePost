# META-22: журнал доставки и журнал аудита только на добавление

lane: meta
deps: [CTR-21, META-13]
class: B
files: [crates/tp-meta/src/*, crates/tp-meta/migrations/*]
read: [docs/decisions/2026-11-01-borrowings-security.md, docs/spec/S5.md#5.2, docs/spec/S3.md#3.1]
checks: [nextest]
manual: no

## Сделать
- Таблица журнала доставки с секционированием по суткам, удаление секций по сроку хранения (секция конфигурации, по умолчанию 30 сут).
- `DeliveryLog` на PostgreSQL: запись пакетами, поиск по отправителю, получателю, Message-ID, интервалу.
- Журнал аудита и журнал доставки — для роли приложения только INSERT и SELECT (права в миграции).

## Приёмка
- Тесты: поиск, удаление старых секций, UPDATE и DELETE от роли приложения отклоняются.
