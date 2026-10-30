# CTR-13: длительности humantime в секциях конфигурации

lane: contracts
deps: [CTR-10]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-10-30-config-events-secrets.md#Длительности в конфигурации (#43, вариант 2), docs/CONTRACTS.md#Config]
checks: [nextest]
manual: no

## Сделать
- Модуль `tp_contracts::config::duration`: `serialize`/`deserialize` для `Duration` и `Option<Duration>` (`#[serde(with = …)]`)
  в формате `5s`, `250ms`, `10m`, `24h`, `1h30m`; без новых зависимостей.
- JSON Schema поля: `type: string` с шаблоном (для `schemars(with = …)`).
- CONTRACTS.md#Config: длительности — только строки этого вида, поля без единицы в имени.

## Приёмка
- Тесты round-trip и отказ на `5`, `5 sec`, `-1s`, переполнение; схема поля в снимке.
