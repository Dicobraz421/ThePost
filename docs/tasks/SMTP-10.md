# SMTP-10: MTA-STS и DANE

lane: smtp
deps: [SMTP-09]
class: A
files: [crates/tp-smtp/src/policy.rs]
read: [docs/spec/S4.md#4.1, docs/spec/S6.md#6.2]
checks: [nextest]
manual: no

## Сделать
- Получение и кэш политик MTA-STS, проверка TLSA (DANE), отказ при несоответствии.

## Приёмка
- Тесты с мок-резолвером и мок-HTTP.
