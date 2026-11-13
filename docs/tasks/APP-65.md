# APP-65: пределы Sieve узла для JMAP — те же, что у ManageSieve

lane: app
deps: [JMAP-35]
class: A
files: [crates/thepost/src/services.rs]
read: [docs/tasks/JMAP-35.md]
checks: [nextest]
manual: no

## Сделать
- JMAP-35 (#635): `Jmap::with_sieve_limits` — пределы разбора скриптов в `SieveScript/set`, `/validate` и capability.
- В `crates/thepost/src/services.rs`: `Services::jmap` — `.with_sieve_limits(self.sieve.limits())`, пределы из той же
  секции `sieve`, что у ManageSieve (`Services::managesieve`) и доставки; отдельная копия `Services::sieve_limits`
  не нужна — Admin API берёт пределы оттуда же. Без этого JMAP проверяет скрипты пределами по умолчанию.

## Приёмка
- Тест узла: скрипт сверх уменьшенного предела секции `sieve` отклоняется через JMAP (`invalidSieve` в `/validate`
  и `/set`) так же, как проверкой ManageSieve.
- Источник: хвост JMAP-35 (#635).
