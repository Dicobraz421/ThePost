# SIEVE-07: правила доставки — реализация DeliveryRules и фаззинг

lane: sieve
deps: [SIEVE-04, SIEVE-05, SIEVE-06]
class: A
files: [crates/tp-sieve/src/*, crates/tp-sieve/fuzz/*]
read: [docs/CONTRACTS.md, docs/spec/S6.md]
checks: [nextest]
manual: no

## Сделать
- `Rules: DeliveryRules`: порядок — скрипт организации, автоответ из `Vacation`, активный скрипт пользователя; `stop` в скрипте
  организации останавливает всё; ошибка выполнения — implicit keep и запись в журнал (без содержимого письма).
- Кэш скомпилированных программ по (учётная запись, версия скрипта), сброс при изменении в `SieveStore`, объём — `sieve.cache`.
- Секция конфигурации `sieve` с `Default`: лимиты разбора и выполнения, `vacation`, кэш, `managesieve` (для SIEVE-08).
- Цели cargo-fuzz `parse` и `run` (SRS 6.8: фаззинг разборщиков).

## Приёмка
- Тесты на in-memory: порядок скриптов, `stop` организации, автоответ вне дат не срабатывает, кэш сбрасывается после `put`.
