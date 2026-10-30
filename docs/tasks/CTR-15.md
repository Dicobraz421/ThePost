# CTR-15: хранилище секретов и ссылки на секреты

lane: contracts
deps: [CTR-10]
class: B
files: [crates/tp-contracts/src/*, docs/CONTRACTS.md]
read: [docs/decisions/2026-10-30-config-events-secrets.md#Секреты — только через хранилище секретов, docs/CONTRACTS.md#Config]
checks: [nextest]
manual: no

## Сделать
- `SecretRef` — ссылка `<путь>#<поле>`; в config.yaml — `{ secret: "thepost/db#password" }`; `Deserialize` и JSON Schema.
- `Secret` — значение, скрытое в `Debug`/`Display`, обнуляется при удалении; доступ только через `expose()`.
- `SecretStore`: `get(&SecretRef) -> Result<Secret>`, `lease_ttl` у динамических секретов, `renew`; ошибки —
  `NotFound`, `Forbidden`, `Unavailable`.
- Точка расширения: `Registry::set_secret_store` (замена встроенного клиента OpenBao/Vault).
- CONTRACTS.md: раздел Secrets; секреты в конфигурации — только `SecretRef`.

## Приёмка
- Тесты разбора ссылки, скрытия значения в `Debug`, схемы поля.
