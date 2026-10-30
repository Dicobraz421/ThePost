# META-15: секции конфигурации tp-meta из config.yaml

lane: meta
deps: [CTR-13, META-07]
class: B
files: [crates/tp-meta/src/*, crates/tp-meta/Cargo.toml]
read: [docs/decisions/2026-10-30-config-events-secrets.md#Длительности в конфигурации (#43, вариант 2), docs/CONTRACTS.md#Config]
checks: [nextest]
manual: no

## Сделать
- Зависимости `serde`, `schemars` в tp-meta — отдельным PR `x/meta-dep` (Cargo.toml крейта + Cargo.lock).
- `Deserialize + JsonSchema` для `MetaConfig`, `PoolConfig` (секция `database`), `EventsConfig`, `DoctorConfig`;
  длительности — `tp_contracts::config::duration`; поля с `#[serde(default)]`, неизвестные поля не теряются для строгой проверки.

## Приёмка
- Тест: YAML с `acquire_timeout: 5s`, `lease_ttl: 30s` → секции; пустая секция → `Default`; `acquire_timeout: 5` — ошибка.
