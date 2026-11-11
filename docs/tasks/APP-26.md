# APP-26: правила Sieve и автоответ в узле — доставка, Admin API, JMAP VacationResponse

lane: app
deps: [SIEVE-07, SMTP-32, META-50, ADM-14, JMAP-23]
class: A
files: [crates/thepost/src/*]
read: [docs/spec/S4.md#4.3]
checks: [nextest]
manual: docs/manual-tests/APP-26.md

## Сделать
- `tp_sieve::Rules` (хранилища `PgMeta`: `SieveStore`, `VacationLog`) в доставке smtp-in и submission (`Delivery::with_rules`);
  секция `sieve` в config.yaml.
- `SieveStore` в Admin API (ADM-14) и в `tp_jmap::Deps` — `VacationResponse` (JMAP-23) для Postman; CLI `thepost admin`
  — команды правил и автоответа.
- Ручная проверка: автоответ, заданный в Postman (JMAP) и через Admin API, отвечает внешнему отправителю один раз за период; правило `fileinto` раскладывает
  письма.

## Приёмка
- Интеграционный тест узла: скрипт через Admin API → письмо по SMTP → папка и автоответ в очереди; сценарии ручной проверки без замечаний.
